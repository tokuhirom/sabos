// sys/args/sabos.rs — SABOS コマンドライン引数 PAL 実装
//
// カーネルが iretq 前に rdi=argc, rsi=argv をレジスタにセットし、
// setup_user_stack_args() でスタック上に null 終端 C 文字列の配列を配置する。
// Unix と同じ形式（argc + argv[] + NULL 終端）なので、Unix 実装に倣って
// Atomic 変数に保存し、std::env::args() から遅延取得する。

#![allow(dead_code)] // init は起動時に一度だけ呼ばれるため

pub use super::common::Args;
use crate::ffi::CStr;
use crate::os::sabos::ffi::OsStringExt;

/// ランタイム初期化時に呼ばれる。argc/argv を Atomic 変数に保存する。
pub unsafe fn init(argc: isize, argv: *const *const u8) {
    unsafe { imp::init(argc, argv) }
}

/// std::env::args() から呼ばれる。保存済みの argc/argv をパースして返す。
pub fn args() -> Args {
    let (argc, argv) = imp::argc_argv();

    let mut vec = Vec::with_capacity(argc as usize);

    for i in 0..argc {
        // SAFETY: argv は argc が正なら非 null で、少なくとも argc 個の要素を持つ。
        let ptr = unsafe { argv.offset(i).read() };

        // NULL エントリが来たら終了（argc と実際のエントリ数がずれる場合の安全策）
        if ptr.is_null() {
            break;
        }

        // SAFETY: ptr が非 null であることは確認済み。
        // カーネルが setup_user_stack_args() で null 終端 C 文字列として配置している。
        let cstr = unsafe { CStr::from_ptr(ptr) };
        vec.push(OsStringExt::from_vec(cstr.to_bytes().to_vec()));
    }

    Args::new(vec)
}

mod imp {
    use crate::ffi::c_char;
    use crate::ptr;
    use crate::sync::atomic::{Atomic, AtomicIsize, AtomicPtr, Ordering};

    // カーネルが提供した argc/argv を静的変数に保存する。
    // init() で一度だけ書き込み、args() で読み取る。
    static ARGC: Atomic<isize> = AtomicIsize::new(0);
    static ARGV: Atomic<*mut *const u8> = AtomicPtr::new(ptr::null_mut());

    #[inline(always)]
    pub unsafe fn init(argc: isize, argv: *const *const u8) {
        // Relaxed ordering で十分: init は他のスレッド生成前に実行される
        ARGC.store(argc, Ordering::Relaxed);
        ARGV.store(argv as *mut _, Ordering::Relaxed);
    }

    pub fn argc_argv() -> (isize, *const *const c_char) {
        let argv = ARGV.load(Ordering::Relaxed);
        let argc = if argv.is_null() { 0 } else { ARGC.load(Ordering::Relaxed) };
        (argc, argv.cast())
    }
}
