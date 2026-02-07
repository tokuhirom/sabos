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

    // std::process::exit は PAL 経由で SYS_EXIT を呼ぶ
}
