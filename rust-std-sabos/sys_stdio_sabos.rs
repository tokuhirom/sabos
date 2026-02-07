// sys/stdio/sabos.rs — SABOS 標準入出力
//
// SYS_WRITE(1) / SYS_READ(0) を使った Stdout/Stdin 実装。
// println! マクロの出力がシリアルコンソールに表示されるようにする。

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};

/// SYS_WRITE(1) を呼んでコンソールに出力する
fn syscall_write(buf: &[u8]) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 1u64,   // SYS_WRITE
            in("rdi") buf.as_ptr() as u64,
            in("rsi") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// SYS_READ(0) を呼んでコンソールから読み取る
fn syscall_read(buf: &mut [u8]) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 0u64,   // SYS_READ
            in("rdi") buf.as_mut_ptr() as u64,
            in("rsi") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

pub struct Stdin;
pub struct Stdout;
pub type Stderr = Stdout;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let ret = syscall_read(buf);
        if (ret as i64) < 0 {
            Err(io::Error::other("read failed"))
        } else {
            Ok(ret as usize)
        }
    }

    fn read_buf(&mut self, mut cursor: BorrowedCursor<'_>) -> io::Result<()> {
        // BorrowedCursor を使う場合: unfilled 部分に読み込む
        let buf = cursor.ensure_init();
        let ret = syscall_read(buf.init_mut());
        if (ret as i64) < 0 {
            Err(io::Error::other("read failed"))
        } else {
            cursor.advance(ret as usize);
            Ok(())
        }
    }

    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        // 最初の非空バッファだけ読む（scatter read は未対応）
        for buf in bufs {
            if !buf.is_empty() {
                return self.read(buf);
            }
        }
        Ok(0)
    }

    fn is_read_vectored(&self) -> bool {
        false
    }
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let ret = syscall_write(buf);
        if (ret as i64) < 0 {
            Err(io::Error::other("write failed"))
        } else {
            Ok(ret as usize)
        }
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        // scatter write: 順に書き出す
        let mut total = 0;
        for buf in bufs {
            if !buf.is_empty() {
                let n = self.write(buf)?;
                total += n;
                if n < buf.len() {
                    break;
                }
            }
        }
        Ok(total)
    }

    fn is_write_vectored(&self) -> bool {
        false
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub const STDIN_BUF_SIZE: usize = 256;

pub fn is_ebadf(_err: &io::Error) -> bool {
    true
}

/// パニック時の出力先を提供する。
/// Some を返すことでパニックメッセージがコンソールに表示される。
pub fn panic_output() -> Option<impl io::Write> {
    Some(Stdout::new())
}
