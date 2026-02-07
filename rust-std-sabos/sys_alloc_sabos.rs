// sys/alloc/sabos.rs — SABOS 用システムアロケータ
//
// SYS_MMAP(28) / SYS_MUNMAP(29) を使った GlobalAlloc 実装。
// ユーザーが #[global_allocator] を指定すれば System は使われないが、
// std のビルドには型として必要。

use crate::alloc::{GlobalAlloc, Layout, System};

/// SYS_MMAP(28) を呼んで匿名ページを確保する
unsafe fn syscall_mmap(addr_hint: u64, len: u64, prot: u64, flags: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 28u64,   // SYS_MMAP
            in("rdi") addr_hint,
            in("rsi") len,
            in("rdx") prot,
            in("r10") flags,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// SYS_MUNMAP(29) を呼んでページマッピングを解除する
unsafe fn syscall_munmap(addr: u64, len: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 29u64,   // SYS_MUNMAP
            in("rdi") addr,
            in("rsi") len,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// MMAP のプロテクションフラグ
const MMAP_PROT_READ: u64 = 0x1;
const MMAP_PROT_WRITE: u64 = 0x2;
/// MMAP のフラグ: 匿名マッピング
const MMAP_FLAG_ANONYMOUS: u64 = 0x1;

#[stable(feature = "alloc_system_type", since = "1.28.0")]
unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // サイズをアラインメント以上にし、4KiB 単位に切り上げる
        let size = layout.size().max(layout.align());
        let size = (size + 0xFFF) & !0xFFF;
        let addr = unsafe {
            syscall_mmap(
                0,
                size as u64,
                MMAP_PROT_READ | MMAP_PROT_WRITE,
                MMAP_FLAG_ANONYMOUS,
            )
        };
        // エラー時（負の値）は null を返す
        if (addr as i64) < 0 {
            core::ptr::null_mut()
        } else {
            addr as *mut u8
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size().max(layout.align());
        let size = (size + 0xFFF) & !0xFFF;
        unsafe {
            syscall_munmap(ptr as u64, size as u64);
        }
    }
}
