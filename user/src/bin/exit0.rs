// exit0.rs — exec テスト用の最小ユーザープログラム
//
// すぐに終了するだけの ELF。exec の同期実行テストで使う。
// 引数付きで起動された場合は、引数と環境変数の受け渡しテストも行う。
//
// 使い方:
//   - 引数なし: "exit0: ok\n" を出力して終了（従来と同じ）
//   - 引数あり: 引数と環境変数の検証を行い、"exit0: args_ok\n" を出力して終了

#![no_std]
#![no_main]

#[path = "../args.rs"]
mod args;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

/// エントリポイント: argc/argv/envp を受け取る。
///
/// System V ABI 互換のレジスタ渡しにより、
/// rdi = argc, rsi = argv, rdx = envp として引数が渡される。
/// 既存の `_start()` 宣言と同じ extern "C" fn なので、
/// 引数なし呼び出しの場合はレジスタの値を無視するだけ。
#[unsafe(no_mangle)]
pub extern "C" fn _start(argc: usize, argv: *const *const u8, envp: *const *const u8) -> ! {
    // argc/argv/envp を初期化
    unsafe { args::init(argc, argv, envp); }

    if args::argc() <= 1 {
        // 引数なし: 従来の動作（exec_exit0 テスト互換）
        syscall::write_str("exit0: ok\n");
    } else {
        // 引数あり: 引数・環境変数の受け渡しテスト
        test_args();
    }

    syscall::exit();
}

/// 引数と環境変数の受け渡しテスト。
///
/// テスト条件:
/// - argc == 3
/// - argv[0] == "/EXIT0.ELF"
/// - argv[1] == "hello"
/// - argv[2] == "world"
/// - 環境変数 "TEST_KEY" == "test_value"
fn test_args() {
    let mut ok = true;

    // argc のチェック
    if args::argc() != 3 {
        syscall::write_str("exit0: FAIL argc != 3\n");
        ok = false;
    }

    // argv[0] のチェック
    match args::argv(0) {
        Some(s) if s == "/EXIT0.ELF" => {}
        Some(s) => {
            syscall::write_str("exit0: FAIL argv[0] = ");
            syscall::write_str(s);
            syscall::write_str("\n");
            ok = false;
        }
        None => {
            syscall::write_str("exit0: FAIL argv[0] = None\n");
            ok = false;
        }
    }

    // argv[1] のチェック
    match args::argv(1) {
        Some(s) if s == "hello" => {}
        Some(s) => {
            syscall::write_str("exit0: FAIL argv[1] = ");
            syscall::write_str(s);
            syscall::write_str("\n");
            ok = false;
        }
        None => {
            syscall::write_str("exit0: FAIL argv[1] = None\n");
            ok = false;
        }
    }

    // argv[2] のチェック
    match args::argv(2) {
        Some(s) if s == "world" => {}
        Some(s) => {
            syscall::write_str("exit0: FAIL argv[2] = ");
            syscall::write_str(s);
            syscall::write_str("\n");
            ok = false;
        }
        None => {
            syscall::write_str("exit0: FAIL argv[2] = None\n");
            ok = false;
        }
    }

    // 環境変数のチェック
    match args::getenv("TEST_KEY") {
        Some(v) if v == "test_value" => {}
        Some(v) => {
            syscall::write_str("exit0: FAIL TEST_KEY = ");
            syscall::write_str(v);
            syscall::write_str("\n");
            ok = false;
        }
        None => {
            syscall::write_str("exit0: FAIL TEST_KEY = None\n");
            ok = false;
        }
    }

    if ok {
        syscall::write_str("exit0: args_ok\n");
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
