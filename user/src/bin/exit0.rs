// exit0.rs — exec テスト用の最小ユーザープログラム
//
// すぐに終了するだけの ELF。exec の同期実行テストで使う。

#![no_std]
#![no_main]

#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    syscall::write_str("exit0: ok\n");
    syscall::exit();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
