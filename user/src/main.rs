// user/src/main.rs — SABOS ユーザープログラム
//
// カーネルとは独立した ELF バイナリとしてビルドされる。
// x86_64-unknown-none ターゲットで no_std/no_main。
// システムコールライブラリ（syscall モジュール）を使って
// カーネルとやり取りする。
//
// このバイナリは include_bytes! でカーネルに埋め込まれ、
// ELF パーサーがロードして Ring 3 で実行する。
//
// ## 機能
//
// - シェル: echo, help, clear, exit コマンドを提供

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

mod allocator;
mod fat32;
mod shell;
mod syscall;

use core::panic::PanicInfo;

/// エントリポイント: カーネルの ELF ローダーがここにジャンプする。
///
/// リンカスクリプトの ENTRY(_start) で指定。
/// Ring 3 で実行されるので、カーネル関数は呼べない。
/// システムコールを使ってカーネルとやり取りする。
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    // ユーザー空間シェルを起動
    shell::run();
}

/// パニックハンドラ（no_std 必須）。
/// ユーザープログラムがパニックしたら SYS_EXIT で終了する。
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
