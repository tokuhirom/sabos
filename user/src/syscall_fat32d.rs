// syscall_fat32d.rs — fat32d 用の最小システムコール
//
// fat32d に必要な最小セットだけを定義する。
// netd の syscall_netd.rs と同パターン。

use core::arch::asm;

/// システムコール番号の定義
///
/// sabos-syscall クレートで一元管理している。
/// 番号の追加・変更は libs/sabos-syscall/src/lib.rs で行うこと。
pub use sabos_syscall::{
    SYS_BLOCK_READ, SYS_BLOCK_WRITE,
    SYS_IPC_SEND, SYS_IPC_RECV,
    SYS_FS_REGISTER,
    SYS_EXIT,
};

pub type SyscallResult = i64;

#[inline]
unsafe fn syscall0(nr: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

#[inline]
unsafe fn syscall3(nr: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

#[inline]
unsafe fn syscall4(nr: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// プロセスを終了する
pub fn exit() -> ! {
    unsafe { syscall0(SYS_EXIT); }
    loop {}
}

/// カーネルに fat32d のファイルシステムサービス登録を通知する。
///
/// カーネルは呼び出し元のタスク ID を fat32d として記録し、
/// VFS の "/" と "/host" を Fat32IpcFs（IPC プロキシ）に切り替える。
pub fn fs_register() -> SyscallResult {
    unsafe { syscall0(SYS_FS_REGISTER) as i64 }
}

/// ブロックデバイスからセクタ読み取り（dev_index 指定版）
pub fn block_read_dev(sector: u64, buf: &mut [u8], dev_index: u64) -> SyscallResult {
    let ptr = buf.as_mut_ptr() as u64;
    let len = buf.len() as u64;
    unsafe { syscall4(SYS_BLOCK_READ, sector, ptr, len, dev_index) as i64 }
}

/// ブロックデバイスへセクタ書き込み（dev_index 指定版）
pub fn block_write_dev(sector: u64, buf: &[u8], dev_index: u64) -> SyscallResult {
    let ptr = buf.as_ptr() as u64;
    let len = buf.len() as u64;
    unsafe { syscall4(SYS_BLOCK_WRITE, sector, ptr, len, dev_index) as i64 }
}

/// IPC メッセージ送信
pub fn ipc_send(dest_task_id: u64, buf: &[u8]) -> SyscallResult {
    let buf_ptr = buf.as_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_IPC_SEND, dest_task_id, buf_ptr, buf_len) as i64 }
}

/// IPC メッセージ受信
pub fn ipc_recv(sender_out: &mut u64, buf: &mut [u8], timeout_ms: u64) -> SyscallResult {
    let sender_ptr = sender_out as *mut u64 as u64;
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall4(SYS_IPC_RECV, sender_ptr, buf_ptr, buf_len, timeout_ms) as i64 }
}
