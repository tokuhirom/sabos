#![no_main]
#![no_std]

use core::fmt::Write;
use uefi::prelude::*;

#[entry]
fn main() -> Status {
    uefi::system::with_stdout(|stdout| {
        stdout.write_str("Hello, SABOS!\r\n").unwrap();
    });

    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
