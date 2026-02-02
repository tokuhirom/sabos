// user/src/main.rs — SABOS ユーザープログラム
//
// カーネルとは独立した ELF バイナリとしてビルドされる。
// x86_64-unknown-none ターゲットで no_std/no_main。
// int 0x80 でシステムコール (SYS_WRITE, SYS_EXIT) を呼んで
// カーネルとやり取りする。
//
// このバイナリは include_bytes! でカーネルに埋め込まれ、
// ELF パーサーがロードして Ring 3 で実行する。

#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;

/// システムコール番号の定義（カーネルの syscall.rs と一致させる）
const SYS_WRITE: u64 = 1;
const SYS_EXIT: u64 = 60;

/// SYS_WRITE システムコール: カーネルコンソールに文字列を出力する。
///
/// int 0x80 のレジスタ規約（Linux 風）:
///   rax = システムコール番号 (1 = SYS_WRITE)
///   rdi = バッファのポインタ
///   rsi = バッファの長さ（バイト数）
fn sys_write(buf: &[u8]) {
    let ptr = buf.as_ptr() as u64;
    let len = buf.len() as u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") SYS_WRITE,
            in("rdi") ptr,
            in("rsi") len,
            lateout("rax") _,
            // int 0x80 で上書きされる可能性があるレジスタを clobber 指定
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
}

/// SYS_EXIT システムコール: ユーザープログラムを終了する。
///
/// カーネル側で保存したスタック (SAVED_RSP/SAVED_RBP) を復元して
/// run_in_usermode() / run_elf_process() の呼び出し元に return する。
fn sys_exit() -> ! {
    unsafe {
        asm!(
            "int 0x80",
            in("rax") SYS_EXIT,
            options(noreturn),
        );
    }
}

/// エントリポイント: カーネルの ELF ローダーがここにジャンプする。
///
/// リンカスクリプトの ENTRY(_start) で指定。
/// Ring 3 で実行されるので、カーネル関数は呼べない。
/// システムコールを使ってカーネルとやり取りする。
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    sys_write(b"Hello from ELF binary!\n");
    sys_exit();
}

/// パニックハンドラ（no_std 必須）。
/// ユーザープログラムがパニックしたら SYS_EXIT で終了する。
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    sys_exit();
}
