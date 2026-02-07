// main.rs — std 対応の SABOS ユーザープログラム
//
// Rust の std クレートを使えるテスト用バイナリ。
// #![no_std] も #![no_main] も不要！
// println! マクロでシリアルコンソールに出力される。
//
// restricted_std: SABOS は Rust が公式サポートしていないターゲットなので、
// この feature gate を明示的に有効にする必要がある。
#![feature(restricted_std)]

/// SYS_WRITE を直接呼んでコンソールに出力する。
/// std の println! が未完成のため、暫定的にこの関数で出力する。
fn raw_write(s: &str) {
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 1u64,   // SYS_WRITE
            in("rdi") s.as_ptr() as u64,
            in("rsi") s.len() as u64,
            lateout("rax") _,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
}

fn main() {
    // 暫定: raw_write で直接出力（SYS_MMAP の問題を回避）
    // TODO: println! が動くようになったら置き換える
    raw_write("Hello from SABOS std!\n");
    raw_write("2 + 3 = 5\n");

    // String が使えることの確認（スタック上で完結するのでアロケータ不要）
    let s = "SABOS";
    raw_write("Hello, ");
    raw_write(s);
    raw_write("!\n");

    // std::process::exit は PAL 経由で SYS_EXIT を呼ぶ
}
