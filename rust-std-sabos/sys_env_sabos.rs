// sys/env/sabos.rs — SABOS 環境変数 PAL 実装
//
// SYS_GETENV(37) / SYS_SETENV(38) / SYS_LISTENV(39) を使って
// std::env::var() / set_var() / vars() を実装する。
// SABOS カーネルはタスクごとに環境変数テーブル（Vec<(String, String)>）を保持しており、
// spawn 時に親プロセスの環境変数が子プロセスにコピーされる。

pub use super::common::Env;
use crate::ffi::{OsStr, OsString};
use crate::io;
use crate::os::sabos::ffi::OsStrExt;
use crate::os::sabos::ffi::OsStringExt;

/// SYS_GETENV(37) を呼んで環境変数の値を取得する。
///
/// 引数:
///   rdi — key のポインタ
///   rsi — key の長さ
///   rdx — value バッファのポインタ
///   r10 — value バッファの長さ
///
/// 戻り値:
///   正の値 — value の長さ（バッファに書き込み済み）
///   -20 — key が見つからない（FileNotFound）
///   -4  — バッファが小さすぎる（BufferOverflow）
fn syscall_getenv(key: &[u8], buf: &mut [u8]) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 37u64,              // SYS_GETENV
            in("rdi") key.as_ptr() as u64,
            in("rsi") key.len() as u64,
            in("rdx") buf.as_mut_ptr() as u64,
            in("r10") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_SETENV(38) を呼んで環境変数を設定する。
///
/// 引数:
///   rdi — key のポインタ
///   rsi — key の長さ
///   rdx — value のポインタ
///   r10 — value の長さ
///
/// 戻り値:
///   0 — 成功
fn syscall_setenv(key: &[u8], value: &[u8]) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 38u64,              // SYS_SETENV
            in("rdi") key.as_ptr() as u64,
            in("rsi") key.len() as u64,
            in("rdx") value.as_ptr() as u64,
            in("r10") value.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_LISTENV(39) を呼んで全環境変数を取得する。
///
/// 引数:
///   rdi — バッファのポインタ
///   rsi — バッファの長さ
///
/// 戻り値:
///   正の値 — 書き込んだバイト数
///   -4  — バッファが小さすぎる（BufferOverflow）
fn syscall_listenv(buf: &mut [u8]) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 39u64,              // SYS_LISTENV
            in("rdi") buf.as_mut_ptr() as u64,
            in("rsi") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// 全環境変数のイテレータを返す。
/// SYS_LISTENV(39) を呼んで "KEY=VALUE\n" 形式のバッファを取得し、
/// パースして (OsString, OsString) のペアに変換する。
pub fn env() -> Env {
    // まず 1KB のバッファで試す
    let mut buf = vec![0u8; 1024];
    let ret = syscall_listenv(&mut buf);

    if ret == -4 {
        // バッファが小さすぎるので大きなバッファで再試行
        let mut big_buf = vec![0u8; 8192];
        let ret2 = syscall_listenv(&mut big_buf);
        if ret2 < 0 {
            return Env::new(Vec::new());
        }
        let len = ret2 as usize;
        return Env::new(parse_env_buf(&big_buf[..len]));
    }

    if ret < 0 {
        return Env::new(Vec::new());
    }

    let len = ret as usize;
    Env::new(parse_env_buf(&buf[..len]))
}

/// "KEY=VALUE\n" 形式のバッファをパースして (OsString, OsString) のペアに変換する。
fn parse_env_buf(data: &[u8]) -> Vec<(OsString, OsString)> {
    let mut result = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        // 最初の '=' で分割
        if let Some(eq_pos) = line.iter().position(|&b| b == b'=') {
            let key = &line[..eq_pos];
            let value = &line[eq_pos + 1..];
            result.push((
                OsStringExt::from_vec(key.to_vec()),
                OsStringExt::from_vec(value.to_vec()),
            ));
        }
    }
    result
}

/// 環境変数の値を取得する。
/// SYS_GETENV(37) を呼んで、指定した key の値を返す。
/// key が存在しない場合は None を返す。
pub fn getenv(key: &OsStr) -> Option<OsString> {
    let key_bytes = key.as_bytes();

    // まず 256 バイトのバッファで試す
    let mut buf = [0u8; 256];
    let ret = syscall_getenv(key_bytes, &mut buf);

    if ret == -20 {
        // FileNotFound: key が存在しない
        return None;
    }

    if ret == -4 {
        // BufferOverflow: バッファが小さすぎるので大きなバッファで再試行
        let mut big_buf = vec![0u8; 4096];
        let ret2 = syscall_getenv(key_bytes, &mut big_buf);
        if ret2 < 0 {
            return None;
        }
        let len = ret2 as usize;
        big_buf.truncate(len);
        return Some(OsStringExt::from_vec(big_buf));
    }

    if ret < 0 {
        // その他のエラー
        return None;
    }

    let len = ret as usize;
    Some(OsStringExt::from_vec(buf[..len].to_vec()))
}

/// 環境変数を設定する。
/// SYS_SETENV(38) を呼んで、指定した key に value を設定する。
pub unsafe fn setenv(key: &OsStr, value: &OsStr) -> io::Result<()> {
    let key_bytes = key.as_bytes();
    let val_bytes = value.as_bytes();

    let ret = syscall_setenv(key_bytes, val_bytes);
    if ret < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "SYS_SETENV failed",
        ));
    }
    Ok(())
}

/// 環境変数を削除する。
/// SYS_SETENV で空の値を設定することで擬似的に削除する。
/// 注: 実際には空文字列の値として残る。完全な削除は将来 SYS_UNSETENV で対応。
pub unsafe fn unsetenv(key: &OsStr) -> io::Result<()> {
    // 空の値を設定して「削除」扱いにする
    let key_bytes = key.as_bytes();
    let ret = syscall_setenv(key_bytes, b"");
    if ret < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "SYS_SETENV (unset) failed",
        ));
    }
    Ok(())
}
