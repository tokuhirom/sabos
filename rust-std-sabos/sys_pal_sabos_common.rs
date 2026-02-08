// sys/pal/sabos/common.rs — SABOS PAL 共通関数
//
// unsupported/common.rs をベースに、SABOS 向けの実装を追加する。
// init / cleanup / unsupported / abort_internal / _start を提供する。

use crate::io as std_io;

// SAFETY: ランタイム初期化時に一度だけ呼ばれる。
// NOTE: 外部から Rust コードが呼ばれる場合、この関数が実行される保証はない。
//
// std の lang_start() → rt::init() → sys::pal::init() の順で呼ばれる。
// argc/argv を sys::args に転送して std::env::args() が動くようにする。
pub unsafe fn init(argc: isize, argv: *const *const u8, _sigpipe: u8) {
    unsafe { crate::sys::args::init(argc, argv) }
}

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
///   _start() → _start_rust(argc, argv) → main(argc, argv) → lang_start() → ユーザーの fn main() → exit()
///
/// _start はアセンブリで実装する。iretq でジャンプされるため、エントリ時の
/// RSP は 16 バイトアラインされている（16n）。しかし System V ABI の関数呼び出し
/// 規約では、call 命令がリターンアドレスを push するため、関数エントリでは
/// RSP = 16n - 8 であることが前提。この 8 バイトのずれが GPF の原因になる。
///
/// 対策として、_start をアセンブリで書いて RSP を明示的に 16n - 8 に調整してから
/// Rust 関数を call する。call 命令自体がさらに 8 バイト push するため、
/// _start_rust のエントリでは RSP = 16n - 16 = 16 の倍数になり、ABI 準拠となる。
#[cfg(not(test))]
core::arch::global_asm!(
    ".global _start",
    "_start:",
    // RSP を 16 バイトアラインに調整する。
    // iretq でジャンプされた時点で RSP は 16n（16 の倍数）。
    // System V ABI では call 前に RSP が 16 の倍数であることが要求される。
    // call _start_rust が 8 バイト push するため、_start_rust エントリでは
    // RSP = 16n - 8 となり、ABI の前提通りになる。
    "and rsp, -16",        // RSP を 16 バイト境界に揃える（念のため）
    "call _start_rust",    // Rust 関数を呼ぶ（リターンアドレスを push）
    "ud2",                 // ここには来ないはず
);

/// _start から呼ばれる Rust 側エントリポイント
///
/// iretq でジャンプされた時点で rdi=argc, rsi=argv, rdx=envp がレジスタにセットされている。
/// _start アセンブリの `call _start_rust` は System V ABI に従うため、
/// rdi → argc, rsi → argv としてそのまま引数として渡ってくる。
/// これにより std::env::args() が正しく動作する。
#[cfg(not(test))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start_rust(argc: isize, argv: *const *const u8) -> ! {
    unsafe extern "C" {
        fn main(argc: isize, argv: *const *const u8) -> i32;
    }

    // カーネルが setup_user_stack_args() でスタックに配置した argc/argv を
    // そのまま main() に渡す。std の lang_start() → init() に伝播し、
    // std::env::args() で取得できるようになる。
    let result = unsafe { main(argc, argv) };

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
