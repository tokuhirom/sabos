// sys/thread/sabos.rs — SABOS スレッド PAL 実装
//
// SYS_THREAD_CREATE(110) / SYS_THREAD_EXIT(111) / SYS_THREAD_JOIN(112) を使って
// std::thread::spawn() を実装する。
//
// ## スタック確保
//
// スレッドのユーザー空間スタックは SYS_MMAP(28) で匿名ページとして確保し、
// join 時に SYS_MUNMAP(29) で解放する。x86_64 ではスタックは下向きに伸びるため、
// スタックトップ = 確保した領域の末尾（16 バイトアラインメント）。
//
// ## thread_local の制約（no_threads モード）
//
// SABOS は thread_local で `no_threads` モードを使っている。
// このモードでは thread-local 変数が実際にはグローバルな Cell になる。
// そのため ThreadInit::init() を呼ぶと set_current() が rtabort! する
// （メインスレッドで既に CURRENT が設定済みのため）。
//
// 対策として init.init() をスキップし、rust_start を直接取り出して実行する。
// これにより std::thread::current() はスポーンしたスレッドからは
// メインスレッドのハンドルを返す（不正確だがクラッシュしない）。
// 将来 thread_local を `thread_local_key` モードに切り替えれば正しく動作する。

use crate::num::NonZero;
use crate::thread::ThreadInit;
use crate::time::Duration;
use crate::io;

////////////////////////////////////////////////////////////////////////////////
// 定数
////////////////////////////////////////////////////////////////////////////////

/// スレッドのデフォルト最小スタックサイズ（64KB）
pub const DEFAULT_MIN_STACK_SIZE: usize = 64 * 1024;

/// MMAP のプロテクションフラグ: 読み取り可能
const MMAP_PROT_READ: u64 = 0x1;
/// MMAP のプロテクションフラグ: 書き込み可能
const MMAP_PROT_WRITE: u64 = 0x2;
/// MMAP のフラグ: 匿名マッピング
const MMAP_FLAG_ANONYMOUS: u64 = 0x1;

////////////////////////////////////////////////////////////////////////////////
// syscall ヘルパー（インラインアセンブリ）
////////////////////////////////////////////////////////////////////////////////

/// SYS_MMAP(28): 匿名ページを確保する
///
/// スレッドのスタック領域を確保するために使用。
/// prot = READ | WRITE, flags = ANONYMOUS で呼ぶ。
fn syscall_mmap(addr_hint: u64, len: u64, prot: u64, flags: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 28u64,          // SYS_MMAP
            in("rdi") addr_hint,
            in("rsi") len,
            in("rdx") prot,
            in("r10") flags,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_MUNMAP(29): ページマッピングを解除する
///
/// スレッド join 後にスタック領域を解放するために使用。
fn syscall_munmap(addr: u64, len: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 29u64,          // SYS_MUNMAP
            in("rdi") addr,
            in("rsi") len,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_THREAD_CREATE(110): 新しいスレッドを作成する
///
/// カーネルは同一アドレス空間（CR3 を共有）で新しいタスクを作成し、
/// entry_point に arg を渡して実行を開始する。
///
/// 引数:
///   rdi — エントリポイント（関数ポインタ）
///   rsi — スタックトップ（ユーザー空間アドレス）
///   rdx — エントリポイントに渡す引数（ThreadInit のポインタ）
///
/// 戻り値:
///   正の値 — スレッド ID（タスク ID）
///   負の値 — エラー
fn syscall_thread_create(entry_ptr: u64, stack_ptr: u64, arg: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 110u64,         // SYS_THREAD_CREATE
            in("rdi") entry_ptr,
            in("rsi") stack_ptr,
            in("rdx") arg,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_THREAD_EXIT(111): 現在のスレッドを終了する
///
/// この関数は戻らない。カーネルがタスクを Finished 状態にして
/// 他のタスクにスイッチする。
fn syscall_thread_exit(exit_code: i32) -> ! {
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 111u64,         // SYS_THREAD_EXIT
            in("rdi") exit_code as u64,
            options(noreturn),
        );
    }
}

/// SYS_THREAD_JOIN(112): スレッドの終了を待つ
///
/// 引数:
///   rdi — 待つスレッドのタスク ID
///   rsi — タイムアウト（ms、0 なら無期限）
///
/// 戻り値:
///   0 以上 — スレッドの終了コード
///   負の値 — エラー
fn syscall_thread_join(thread_id: u64, timeout_ms: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 112u64,         // SYS_THREAD_JOIN
            in("rdi") thread_id,
            in("rsi") timeout_ms,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_YIELD(32): CPU を他のスレッドに譲る
fn syscall_yield() {
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 32u64,          // SYS_YIELD
            lateout("rax") _,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
}

/// SYS_SLEEP(33): 指定ミリ秒スリープする
fn syscall_sleep(ms: u64) {
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 33u64,          // SYS_SLEEP
            in("rdi") ms,
            lateout("rax") _,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
}

////////////////////////////////////////////////////////////////////////////////
// Thread
////////////////////////////////////////////////////////////////////////////////

/// SABOS のスレッドハンドル。
///
/// カーネルのタスク ID と、SYS_MMAP で確保したスタック領域の情報を保持する。
/// join() でスレッドの終了を待ち、スタック領域を解放する。
pub struct Thread {
    /// カーネルのタスク ID（スレッド ID）
    tid: u64,
    /// SYS_MMAP で確保したスタック領域のベースアドレス
    stack_base: u64,
    /// スタック領域のサイズ（バイト）
    stack_size: usize,
}

// Thread は Send + Sync であることをコンパイラに伝える。
// スレッドハンドルの操作（join/kill）はスレッドセーフ。
unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

/// スレッドのエントリトランポリン（アセンブリ）。
///
/// iretq でジャンプされた時点で RSP は 16 の倍数（16n）。
/// しかし System V ABI では関数エントリで RSP = 16n - 8 が期待される
/// （call 命令がリターンアドレスを push するため）。
///
/// _start と同じパターンで、アセンブリで RSP をアラインしてから
/// Rust 関数を call する。call が 8 バイト push するため、
/// _thread_entry_rust のエントリでは RSP = 16n - 8 となり ABI 準拠。
///
/// rdi にはスレッドの引数（Box<ThreadInit> のポインタ）がセットされている。
/// System V ABI の call では rdi はそのまま第1引数として渡る。
core::arch::global_asm!(
    ".global _thread_entry_trampoline",
    "_thread_entry_trampoline:",
    "and rsp, -16",              // RSP を 16 バイト境界に揃える（念のため）
    "call _thread_entry_rust",   // Rust 関数を呼ぶ（rdi = arg がそのまま渡る）
    "ud2",                       // ここには来ないはず（_thread_entry_rust は divergent）
);

/// スレッドのエントリポイント（Rust 側）。
///
/// _thread_entry_trampoline から call で呼ばれる。
/// rdi = arg = Box<ThreadInit> のポインタ。
///
/// ## no_threads モードの制約
///
/// SABOS は thread_local で `no_threads` モードを使っている。
/// このモードでは thread-local 変数が実際にはグローバルな Cell になるため、
/// ThreadInit::init() を呼ぶと set_current() が rtabort! する
/// （メインスレッドで既に CURRENT が設定済みのため）。
///
/// そのため init.init() をスキップし、rust_start を直接取り出して実行する。
#[unsafe(no_mangle)]
extern "C" fn _thread_entry_rust(arg: u64) -> ! {
    // arg は Box<ThreadInit> のポインタ。
    // Box::from_raw でヒープ上の ThreadInit を復元する。
    let init = unsafe { Box::from_raw(arg as *mut ThreadInit) };

    // no_threads モードでは init.init() を呼べない（rtabort! するため）。
    // 代わりに ThreadInit を destructure して rust_start を直接取り出す。
    // handle は使わないが、正しく drop する。
    let ThreadInit { handle: _handle, rust_start } = *init;

    // ユーザーのクロージャを実行
    rust_start();

    // スレッドを終了（終了コード 0）
    syscall_thread_exit(0);
}

impl Thread {
    /// 新しいスレッドを作成する。
    ///
    /// 手順:
    ///   1. SYS_MMAP でスタック領域を確保（READ | WRITE, ANONYMOUS）
    ///   2. ThreadInit をヒープに配置して生ポインタに変換
    ///   3. SYS_THREAD_CREATE で thread_entry をエントリポイントとして起動
    ///   4. カーネルが同一アドレス空間でスレッドを実行開始
    ///
    /// # Safety
    ///
    /// std::thread::Builder::spawn_unchecked のドキュメントに記載された
    /// 安全性要件を呼び出し元が満たす必要がある。
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        // スタックサイズを決定（最低 DEFAULT_MIN_STACK_SIZE）
        let stack_size = if stack < DEFAULT_MIN_STACK_SIZE {
            DEFAULT_MIN_STACK_SIZE
        } else {
            stack
        };

        // SYS_MMAP でスタック領域を確保
        let base = syscall_mmap(
            0,
            stack_size as u64,
            MMAP_PROT_READ | MMAP_PROT_WRITE,
            MMAP_FLAG_ANONYMOUS,
        );
        if base <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "SYS_MMAP failed: cannot allocate thread stack",
            ));
        }
        let stack_base = base as u64;

        // x86_64 ではスタックは下向きに伸びる。
        // スタックトップ = 確保した領域の末尾。
        // 16 バイトアラインメントを保証する。
        let stack_top = (stack_base + stack_size as u64) & !0xF;

        // ThreadInit をヒープに配置して生ポインタに変換。
        // _thread_entry_rust() で Box::from_raw で復元される。
        let data = Box::into_raw(init);

        // アセンブリトランポリンのアドレスを取得。
        // _thread_entry_trampoline は global_asm! で定義されている。
        unsafe extern "C" {
            fn _thread_entry_trampoline();
        }
        let entry = _thread_entry_trampoline as *const () as u64;

        // SYS_THREAD_CREATE でスレッドを起動。
        // entry = _thread_entry_trampoline, stack = stack_top, arg = data ポインタ
        let tid = syscall_thread_create(
            entry,
            stack_top,
            data as u64,
        );

        if tid <= 0 {
            // スレッド作成失敗 — ThreadInit とスタックを回収
            unsafe {
                drop(Box::from_raw(data));
            }
            syscall_munmap(stack_base, stack_size as u64);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "SYS_THREAD_CREATE failed",
            ));
        }

        Ok(Thread {
            tid: tid as u64,
            stack_base,
            stack_size,
        })
    }

    /// スレッドの終了を待つ。
    ///
    /// SYS_THREAD_JOIN で無期限にスレッドの終了を待ち、
    /// 終了後に SYS_MUNMAP でスタック領域を解放する。
    pub fn join(self) {
        // 無期限待ち（timeout_ms = 0）
        let _ = syscall_thread_join(self.tid, 0);

        // スタック領域を解放
        syscall_munmap(self.stack_base, self.stack_size as u64);
    }
}

/// 利用可能な並列度（コア数）を返す。
/// SABOS はシングルコアなので常に 1。
pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    Ok(unsafe { NonZero::new_unchecked(1) })
}

/// 現在のスレッドの OS レベル ID を返す。
/// TODO: SYS_GETPID(35) で取得可能だが、現状は未実装。
pub fn current_os_id() -> Option<u64> {
    None
}

/// CPU を他のスレッドに譲る（SYS_YIELD）。
pub fn yield_now() {
    syscall_yield();
}

/// スレッド名を設定する（SABOS では未対応）。
pub fn set_name(_name: &crate::ffi::CStr) {
    // SABOS ではスレッド名の設定は未対応
}

/// 指定時間スリープする（SYS_SLEEP）。
///
/// Duration をミリ秒に変換して SYS_SLEEP を呼ぶ。
/// 1ms 未満の Duration は 1ms に切り上げる（0ms スリープは何もしないため）。
pub fn sleep(dur: Duration) {
    let ms = dur.as_millis();
    let ms = u64::try_from(ms).unwrap_or(u64::MAX);
    // Duration が 0 でなければ最低 1ms スリープする
    if ms == 0 && !dur.is_zero() {
        syscall_sleep(1);
    } else if ms > 0 {
        syscall_sleep(ms);
    }
}
