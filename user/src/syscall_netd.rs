// syscall_netd.rs — netd 用の最小システムコール
//
// netd に必要な最小セットだけを定義する。

use core::arch::asm;

// ネットワーク (40-49)
pub const SYS_NET_SEND_FRAME: u64 = 45; // net_send_frame(buf_ptr, len)
pub const SYS_NET_RECV_FRAME: u64 = 46; // net_recv_frame(buf_ptr, len, timeout_ms)
pub const SYS_NET_GET_MAC: u64 = 47;    // net_get_mac(buf_ptr, len)

// IPC (90-99)
pub const SYS_IPC_SEND: u64 = 90;     // ipc_send(dest_task_id, buf_ptr, len)
pub const SYS_IPC_RECV: u64 = 91;     // ipc_recv(sender_ptr, buf_ptr, buf_len, timeout_ms)

// 終了
pub const SYS_EXIT: u64 = 60;

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
unsafe fn syscall2(nr: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") arg1,
            in("rsi") arg2,
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

pub fn exit() -> ! {
    unsafe {
        syscall0(SYS_EXIT);
    }
    loop {}
}

pub fn net_send_frame(frame: &[u8]) -> SyscallResult {
    let ptr = frame.as_ptr() as u64;
    let len = frame.len() as u64;
    unsafe { syscall2(SYS_NET_SEND_FRAME, ptr, len) as i64 }
}

pub fn net_recv_frame(buf: &mut [u8], timeout_ms: u64) -> SyscallResult {
    let ptr = buf.as_mut_ptr() as u64;
    let len = buf.len() as u64;
    unsafe { syscall3(SYS_NET_RECV_FRAME, ptr, len, timeout_ms) as i64 }
}

pub fn net_get_mac(buf: &mut [u8; 6]) -> SyscallResult {
    let ptr = buf.as_mut_ptr() as u64;
    let len = 6u64;
    unsafe { syscall2(SYS_NET_GET_MAC, ptr, len) as i64 }
}

pub fn ipc_send(dest_task_id: u64, buf: &[u8]) -> SyscallResult {
    let buf_ptr = buf.as_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_IPC_SEND, dest_task_id, buf_ptr, buf_len) as i64 }
}

pub fn ipc_recv(sender_out: &mut u64, buf: &mut [u8], timeout_ms: u64) -> SyscallResult {
    let sender_ptr = sender_out as *mut u64 as u64;
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall4(SYS_IPC_RECV, sender_ptr, buf_ptr, buf_len, timeout_ms) as i64 }
}
