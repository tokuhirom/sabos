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

    // === std::process::Command::output() パイプテスト ===

    // Command::output() テスト（SYS_PIPE + SYS_SPAWN_REDIRECTED 経由）
    // EXIT0.ELF は "exit0: ok\n" を stdout に出力して終了する。
    // output() はパイプで子プロセスの stdout をキャプチャする。
    match std::process::Command::new("/EXIT0.ELF").output() {
        Ok(output) => {
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            if stdout_str.contains("exit0: ok") {
                println!("process::output pipe OK");
            } else {
                println!("process::output pipe FAIL: stdout={:?}", stdout_str);
            }
        }
        Err(e) => println!("process::output error: {}", e),
    }

    // === std::thread テスト ===

    // std::thread::spawn() テスト（SYS_THREAD_CREATE + SYS_THREAD_JOIN 経由）
    // スレッドを作成し、共有変数を書き換えて join で終了を待つ。
    // NOTE: no_threads モードのため thread_local は共有される。
    //       AtomicBool で安全にデータをやり取りする。
    use std::sync::atomic::{AtomicBool, Ordering};
    static THREAD_DONE: AtomicBool = AtomicBool::new(false);

    let handle = std::thread::spawn(|| {
        // スレッド内で処理を実行
        THREAD_DONE.store(true, Ordering::SeqCst);
    });
    match handle.join() {
        Ok(()) => {
            if THREAD_DONE.load(Ordering::SeqCst) {
                println!("thread::spawn_join OK");
            } else {
                println!("thread::spawn_join FAILED: flag not set");
            }
        }
        Err(_) => println!("thread::spawn_join FAILED: join panicked"),
    }

    // スレッドで値を返すテスト
    let handle2 = std::thread::spawn(|| {
        42u64
    });
    match handle2.join() {
        Ok(val) => {
            if val == 42 {
                println!("thread::return_value OK: {}", val);
            } else {
                println!("thread::return_value FAILED: expected 42, got {}", val);
            }
        }
        Err(_) => println!("thread::return_value FAILED: join panicked"),
    }

    // yield_now テスト（クラッシュしなければ OK）
    std::thread::yield_now();
    println!("thread::yield_now OK");

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

    // === std::net テスト ===
    //
    // ネットワークテストは最後に実行する。
    // DNS lookup が netd との IPC タイムアウトでハングすると、
    // 以降のテストが実行されないため、他のテストを先に完了させる。

    // TCP アドレスパーステスト（ネットワーク通信不要）
    let addr: std::net::SocketAddr = "10.0.2.2:80".parse().unwrap();
    println!("net::tcp_parse OK: {}", addr);

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

    // === std::net::UdpSocket テスト ===
    //
    // UdpSocket::bind(0) でエフェメラルポートにバインドし、
    // DNS サーバー (10.0.2.3:53) に手動 DNS クエリを送って応答を受信する。
    {
        use std::net::UdpSocket;
        match UdpSocket::bind("0.0.0.0:0") {
            Ok(sock) => {
                println!("net::udp_bind OK");
                // read_timeout を設定（5 秒）
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));

                // DNS クエリを手動構築: example.com の A レコード
                let mut query = [0u8; 29];
                query[0] = 0xAB; query[1] = 0xCD; // ID
                query[2] = 0x01; query[3] = 0x00; // Flags: RD=1
                query[4] = 0x00; query[5] = 0x01; // QDCOUNT=1
                // QNAME: \x07example\x03com\x00
                query[12] = 7;
                query[13..20].copy_from_slice(b"example");
                query[20] = 3;
                query[21..24].copy_from_slice(b"com");
                query[24] = 0;
                query[25] = 0x00; query[26] = 0x01; // QTYPE=A
                query[27] = 0x00; query[28] = 0x01; // QCLASS=IN

                match sock.send_to(&query, "10.0.2.3:53") {
                    Ok(n) => println!("net::udp_send OK: {} bytes", n),
                    Err(e) => println!("net::udp_send error: {}", e),
                }

                let mut buf = [0u8; 512];
                match sock.recv_from(&mut buf) {
                    Ok((n, addr)) => {
                        if n >= 2 && buf[0] == 0xAB && buf[1] == 0xCD {
                            println!("net::udp_recv OK: {} bytes from {}", n, addr);
                        } else {
                            println!("net::udp_recv FAILED: unexpected response");
                        }
                    }
                    Err(e) => println!("net::udp_recv error: {}", e),
                }
            }
            Err(e) => {
                println!("net::udp_bind error: {}", e);
            }
        }
    }

    // std::process::exit は PAL 経由で SYS_EXIT を呼ぶ
}
