// sys/pipe/sabos.rs — SABOS パイプ PAL 実装
//
// カーネルのパイプ基盤（SYS_PIPE, Handle-based I/O）を使って
// std::process::Command の stdout/stdin キャプチャを実現する。
//
// ## 設計
//
// - Pipe は (handle_id, handle_token) のペアを保持する
// - SYS_PIPE(5) で read/write ハンドルペアを作成
// - SYS_HANDLE_READ(71) / SYS_HANDLE_WRITE(72) でデータ転送
// - SYS_HANDLE_CLOSE(73) で後始末
// - read が WouldBlock(-60) を返したら SYS_YIELD(32) して再試行
// - read が 0 を返したら EOF

use crate::io;

/// カーネルハンドル（id + token ペア）を保持するパイプ端点。
///
/// カーネル側では Handle { id: u64, token: u64 } として管理される。
/// token は偽造防止のためのランダム値で、syscall 時に検証される。
pub struct Pipe {
    handle_id: u64,
    handle_token: u64,
}

/// カーネルの Handle 構造体のレイアウト（SYS_PIPE がユーザー空間に書き込む形式）
#[repr(C)]
struct KernelHandle {
    id: u64,
    token: u64,
}

/// パイプを作成し、(read_end, write_end) のペアを返す。
///
/// SYS_PIPE(5) を呼び出し、カーネルが read/write ハンドルを
/// ユーザー空間のポインタに書き込む。
pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    let mut read_handle = KernelHandle { id: 0, token: 0 };
    let mut write_handle = KernelHandle { id: 0, token: 0 };

    let ret = syscall_pipe(&mut read_handle, &mut write_handle);
    if ret < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "SYS_PIPE failed",
        ));
    }

    let read_pipe = Pipe {
        handle_id: read_handle.id,
        handle_token: read_handle.token,
    };
    let write_pipe = Pipe {
        handle_id: write_handle.id,
        handle_token: write_handle.token,
    };

    Ok((read_pipe, write_pipe))
}

impl Pipe {
    /// パイプから読み取る。
    ///
    /// SYS_HANDLE_READ(71) を使用。
    /// - 正の値: 読み取ったバイト数
    /// - 0: EOF（書き込み端が全て閉じられた）
    /// - -60 (WouldBlock): データがまだない → SYS_YIELD して再試行
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let ret = syscall_handle_read(
                self.handle_id,
                self.handle_token,
                buf.as_mut_ptr(),
                buf.len(),
            );
            if ret == -60 {
                // WouldBlock: データがまだない。CPU を譲って再試行する。
                syscall_yield();
                continue;
            }
            if ret < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "SYS_HANDLE_READ failed on pipe",
                ));
            }
            return Ok(ret as usize);
        }
    }

    /// パイプに書き込む。
    ///
    /// SYS_HANDLE_WRITE(72) を使用。
    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let ret = syscall_handle_write(
            self.handle_id,
            self.handle_token,
            buf.as_ptr(),
            buf.len(),
        );
        if ret < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "SYS_HANDLE_WRITE failed on pipe",
            ));
        }
        Ok(ret as usize)
    }

    /// ハンドル ID を返す（process モジュールが SpawnRedirectArgs を構築するために使用）
    pub fn handle_id(&self) -> u64 {
        self.handle_id
    }

    /// ハンドルトークンを返す（process モジュールが SpawnRedirectArgs を構築するために使用）
    pub fn handle_token(&self) -> u64 {
        self.handle_token
    }

    /// EOF までパイプのデータを全て読み取り、vec に追加する。
    ///
    /// read() が 0 を返すまでループする。
    /// 子プロセスの stdout をキャプチャする output() で使用。
    /// 読み取った総バイト数を返す。
    pub fn read_to_end(&self, vec: &mut Vec<u8>) -> io::Result<usize> {
        let mut buf = [0u8; 4096];
        let mut total = 0;
        loop {
            let n = self.read(&mut buf)?;
            if n == 0 {
                // EOF — 書き込み端が閉じられた
                break;
            }
            vec.extend_from_slice(&buf[..n]);
            total += n;
        }
        Ok(total)
    }

    // --- 未サポート機能 ---

    pub fn read_buf(&self, _buf: crate::io::BorrowedCursor<'_>) -> io::Result<()> {
        Err(io::Error::UNSUPPORTED_PLATFORM)
    }

    pub fn read_vectored(&self, _bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        Err(io::Error::UNSUPPORTED_PLATFORM)
    }

    #[inline]
    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn write_vectored(&self, _bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        Err(io::Error::UNSUPPORTED_PLATFORM)
    }

    #[inline]
    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        // パイプハンドルの複製は未サポート
        Err(io::Error::UNSUPPORTED_PLATFORM)
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SYS_HANDLE_CLOSE(73) でハンドルを閉じる。
        // 書き込み端を close すると、読み取り側が EOF を受け取る。
        let _ = syscall_handle_close(self.handle_id, self.handle_token);
    }
}

impl crate::fmt::Debug for Pipe {
    fn fmt(&self, f: &mut crate::fmt::Formatter<'_>) -> crate::fmt::Result {
        f.debug_struct("Pipe")
            .field("handle_id", &self.handle_id)
            .field("handle_token", &self.handle_token)
            .finish()
    }
}

////////////////////////////////////////////////////////////////////////////////
// syscall ヘルパー（インラインアセンブリ）
////////////////////////////////////////////////////////////////////////////////

/// SYS_PIPE(5): パイプハンドルペアを作成する。
///
/// 引数:
///   rdi — read_handle のポインタ（ユーザー空間）
///   rsi — write_handle のポインタ（ユーザー空間）
///
/// 戻り値:
///   0 — 成功
///   負の値 — エラー
fn syscall_pipe(read_handle: &mut KernelHandle, write_handle: &mut KernelHandle) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 5u64,                // SYS_PIPE
            in("rdi") read_handle as *mut KernelHandle as u64,
            in("rsi") write_handle as *mut KernelHandle as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_HANDLE_READ(71): ハンドルからデータを読み取る。
///
/// ハンドルは (id, token) ペアで指定する。
/// カーネルは Handle { id, token } を構造体ポインタ経由で受け取る。
fn syscall_handle_read(handle_id: u64, handle_token: u64, buf: *mut u8, len: usize) -> i64 {
    let handle = KernelHandle { id: handle_id, token: handle_token };
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 71u64,               // SYS_HANDLE_READ
            in("rdi") &handle as *const KernelHandle as u64,
            in("rsi") buf as u64,
            in("rdx") len as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_HANDLE_WRITE(72): ハンドルにデータを書き込む。
fn syscall_handle_write(handle_id: u64, handle_token: u64, buf: *const u8, len: usize) -> i64 {
    let handle = KernelHandle { id: handle_id, token: handle_token };
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 72u64,               // SYS_HANDLE_WRITE
            in("rdi") &handle as *const KernelHandle as u64,
            in("rsi") buf as u64,
            in("rdx") len as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_HANDLE_CLOSE(73): ハンドルを閉じる。
fn syscall_handle_close(handle_id: u64, handle_token: u64) -> i64 {
    let handle = KernelHandle { id: handle_id, token: handle_token };
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 73u64,               // SYS_HANDLE_CLOSE
            in("rdi") &handle as *const KernelHandle as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_YIELD(32): CPU を譲る。
fn syscall_yield() {
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 32u64,               // SYS_YIELD
            lateout("rax") _,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
}
