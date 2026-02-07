// sys/pal/sabos/common.rs — SABOS PAL 共通関数
//
// unsupported/common.rs をベースに、SABOS 向けの実装を追加する。
// init / cleanup / unsupported / abort_internal / _start を提供する。

use crate::io as std_io;

// SAFETY: ランタイム初期化時に一度だけ呼ばれる。
// NOTE: 外部から Rust コードが呼ばれる場合、この関数が実行される保証はない。
pub unsafe fn init(_argc: isize, _argv: *const *const u8, _sigpipe: u8) {}

// SAFETY: ランタイムクリーンアップ時に一度だけ呼ばれる。
// NOTE: プログラムがアボートした場合、この関数が実行される保証はない。
pub unsafe fn cleanup() {}

/// サポートされていない操作のエラーを返す
pub fn unsupported<T>() -> std_io::Result<T> {
    Err(unsupported_err())
}

/// サポートされていない操作のエラー値
pub fn unsupported_err() -> std_io::Error {
    std_io::Error::UNSUPPORTED_PLATFORM
}

/// 内部アボート: SYS_EXIT(60) を呼んでプロセスを終了する
pub fn abort_internal() -> ! {
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 60u64,   // SYS_EXIT
            in("rdi") 134u64,  // SIGABRT 相当の終了コード
            options(noreturn)
        );
    }
}

/// SABOS 用エントリポイント: _start → main() → exit()
///
/// Rust std の `fn main()` を使うためには、リンカが期待する `_start` シンボルと、
/// std の `lang_start` が呼び出す `main(argc, argv)` のブリッジが必要。
/// Hermit OS の `runtime_entry` と同じ役割を果たす。
///
/// 呼び出しフロー:
///   _start() → main(0, null) → lang_start() → ユーザーの fn main() → exit()
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start() -> ! {
    unsafe extern "C" {
        fn main(argc: isize, argv: *const *const u8) -> i32;
    }

    // SABOS では現在 argc/argv を渡さないので 0 / null で呼ぶ
    let result = unsafe { main(0, core::ptr::null()) };

    // SYS_EXIT でプロセスを終了
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 60u64,   // SYS_EXIT
            in("rdi") result as u64,
            options(noreturn)
        );
    }
}
