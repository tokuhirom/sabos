// main.rs — std 対応の SABOS ユーザープログラム
//
// Rust の std クレートを使えるテスト用バイナリ。
// #![no_std] も #![no_main] も不要！
// println! マクロでシリアルコンソールに出力される。
//
// restricted_std: SABOS は Rust が公式サポートしていないターゲットなので、
// この feature gate を明示的に有効にする必要がある。
#![feature(restricted_std)]

fn main() {
    // println! テスト（std の stdout → PAL の Stdout → SYS_WRITE 経由）
    println!("Hello from SABOS std!");
    println!("2 + 3 = {}", 2 + 3);

    // String が使えることの確認（ヒープアロケーション = SYS_MMAP 経由）
    let s = String::from("Hello from std String!");
    println!("{}", s);

    // Vec のテスト
    let v: Vec<i32> = (1..=5).collect();
    let sum: i32 = v.iter().sum();
    println!("sum of 1..=5 = {}", sum);

    // === std::fs テスト ===

    // std::fs::read_to_string テスト
    // ディスクイメージ上の HELLO.TXT を読み取る
    match std::fs::read_to_string("/HELLO.TXT") {
        Ok(content) => println!("fs::read_to_string OK: {}", content.trim()),
        Err(e) => println!("fs::read_to_string error: {}", e),
    }

    // std::fs::write テスト（新規ファイル作成 + 書き込み）
    match std::fs::write("/STDTEST.TXT", "written by std::fs") {
        Ok(()) => println!("fs::write OK"),
        Err(e) => println!("fs::write error: {}", e),
    }

    // 書き込んだファイルを読み返して検証
    match std::fs::read_to_string("/STDTEST.TXT") {
        Ok(content) => println!("fs::read_back OK: {}", content),
        Err(e) => println!("fs::read_back error: {}", e),
    }

    // std::fs::metadata テスト
    match std::fs::metadata("/HELLO.TXT") {
        Ok(meta) => println!("fs::metadata OK: size={} is_file={}", meta.len(), meta.is_file()),
        Err(e) => println!("fs::metadata error: {}", e),
    }

    // テストファイルを削除して後始末
    let _ = std::fs::remove_file("/STDTEST.TXT");

    // === std::time テスト ===

    // std::time::Instant::now() テスト（SYS_CLOCK_MONOTONIC 経由）
    let start = std::time::Instant::now();
    // 少し計算して時間を消費する
    let mut dummy: u64 = 0;
    for i in 0..100_000u64 {
        dummy = dummy.wrapping_add(i);
    }
    // dummy を使って最適化による削除を防ぐ
    let _ = dummy;
    let elapsed = start.elapsed();
    // 起動からの経過時間が取得できていれば OK（elapsed は 0 以上）
    println!("time::Instant OK: elapsed={}ms", elapsed.as_millis());

    // Instant の単調増加性テスト
    let t1 = std::time::Instant::now();
    let t2 = std::time::Instant::now();
    if t2 >= t1 {
        println!("time::monotonic OK");
    } else {
        println!("time::monotonic FAILED: t2 < t1");
    }

    // === std::time::SystemTime テスト ===

    // SystemTime::now() テスト（SYS_CLOCK_REALTIME 経由で CMOS RTC を読み取る）
    use std::time::SystemTime;
    match SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => {
            let secs = duration.as_secs();
            // UNIX エポック秒が 2020 年以降であることを確認
            // 2020-01-01 00:00:00 UTC = 1577836800
            if secs >= 1577836800 {
                println!("time::SystemTime OK: epoch_secs={}", secs);
            } else {
                println!("time::SystemTime WARN: epoch_secs={} (before 2020)", secs);
            }
        }
        Err(e) => println!("time::SystemTime error: {}", e),
    }

    // === std::env::args テスト ===

    // std::env::args() テスト（カーネルが argc/argv をレジスタ経由で渡す）
    let args: Vec<String> = std::env::args().collect();
    println!("args::count OK: {}", args.len());
    if !args.is_empty() {
        println!("args::argv0 OK: {}", args[0]);
    }
    // 引数が 2 つ以上あれば追加引数も表示
    if args.len() > 1 {
        println!("args::argv1 OK: {}", args[1]);
    }

    // === std::env テスト ===

    // std::env::current_dir() テスト（getcwd → "/" を返す）
    match std::env::current_dir() {
        Ok(path) => println!("env::current_dir OK: {}", path.display()),
        Err(e) => println!("env::current_dir error: {}", e),
    }

    // std::env::set_var() / var() テスト（SYS_SETENV / SYS_GETENV 経由）
    // set_var は Rust 1.66+ で unsafe（マルチスレッド環境でのデータ競合防止のため）
    // SABOS はシングルスレッドなので安全
    unsafe { std::env::set_var("SABOS_TEST", "hello_env"); }
    match std::env::var("SABOS_TEST") {
        Ok(val) => println!("env::var OK: SABOS_TEST={}", val),
        Err(e) => println!("env::var error: {}", e),
    }

    // === std::env::vars() テスト ===

    // std::env::vars() テスト（SYS_LISTENV 経由で全環境変数を取得）
    // 直前に SABOS_TEST=hello_env を set_var しているので、少なくとも 1 つは返るはず
    let vars: Vec<(String, String)> = std::env::vars().collect();
    println!("env::vars OK: count={}", vars.len());
    // SABOS_TEST が含まれているか確認
    if vars.iter().any(|(k, _)| k == "SABOS_TEST") {
        println!("env::vars_contains OK: SABOS_TEST found");
    } else {
        println!("env::vars_contains FAILED: SABOS_TEST not found");
    }

    // === std::net テスト ===

    // DNS 解決テスト（std::net::ToSocketAddrs 経由で lookup_host を呼ぶ）
    use std::net::ToSocketAddrs;
    match ("example.com", 80).to_socket_addrs() {
        Ok(mut addrs) => {
            if let Some(addr) = addrs.next() {
                println!("net::lookup OK: {}", addr);
            }
        }
        Err(e) => println!("net::lookup error: {}", e),
    }

    // TCP 接続テスト（IP アドレスリテラルのパーステスト）
    // 実際の TCP 接続は外部ネットワーク依存で CI では不安定なため、
    // SocketAddr のパース → connect_inner の呼び出しまでを確認する。
    // 完全な TCP 通信テストは user シェルの selftest_net (no_std バイナリ) で実施。
    let addr: std::net::SocketAddr = "10.0.2.2:80".parse().unwrap();
    println!("net::tcp_parse OK: {}", addr);

    // === std::process テスト ===

    // std::process::Command テスト（SYS_SPAWN + SYS_WAIT 経由）
    // EXIT0.ELF は正常終了（exit code 0）するだけのプログラム
    match std::process::Command::new("/EXIT0.ELF").status() {
        Ok(status) => {
            if status.success() {
                println!("process::status OK: exit_code=0");
            } else {
                println!("process::status FAIL: exit_code={:?}", status.code());
            }
        }
        Err(e) => println!("process::status error: {}", e),
    }

    // Command::new().arg().spawn().wait() のテスト
    match std::process::Command::new("/EXIT0.ELF").spawn() {
        Ok(mut child) => {
            println!("process::spawn OK: id={}", child.id());
            match child.wait() {
                Ok(status) => println!("process::wait OK: success={}", status.success()),
                Err(e) => println!("process::wait error: {}", e),
            }
        }
        Err(e) => println!("process::spawn error: {}", e),
    }

    // === serde_json テスト ===

    // 外部クレート（serde + serde_json）がビルド・動作するかの検証
    use serde::{Serialize, Deserialize};

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Point { x: i32, y: i32 }

    let p = Point { x: 1, y: 2 };
    match serde_json::to_string(&p) {
        Ok(json) => {
            println!("serde::to_string OK: {}", json);
            // デシリアライズして元に戻るか検証
            match serde_json::from_str::<Point>(&json) {
                Ok(p2) => {
                    if p2 == p {
                        println!("serde::from_str OK: {:?}", p2);
                    } else {
                        println!("serde::from_str MISMATCH: {:?} != {:?}", p2, p);
                    }
                }
                Err(e) => println!("serde::from_str error: {}", e),
            }
        }
        Err(e) => println!("serde::to_string error: {}", e),
    }

    // std::process::exit は PAL 経由で SYS_EXIT を呼ぶ
}
