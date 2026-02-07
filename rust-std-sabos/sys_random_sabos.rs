// sys/random/sabos.rs — SABOS 乱数生成
//
// SYS_GETRANDOM(27) を使ってランダムバイトを生成する。
// RDRAND 命令ベースのハードウェア乱数生成器を利用。

/// SYS_GETRANDOM(27) を呼んでランダムバイトをバッファに書き込む
pub fn fill_bytes(buf: &mut [u8]) {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 27u64,   // SYS_GETRANDOM
            in("rdi") buf.as_mut_ptr() as u64,
            in("rsi") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    // エラーの場合はパニック（乱数生成は必須機能）
    if (ret as i64) < 0 {
        panic!("SYS_GETRANDOM failed");
    }
}
