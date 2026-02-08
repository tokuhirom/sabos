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

    // std::process::exit は PAL 経由で SYS_EXIT を呼ぶ
}
