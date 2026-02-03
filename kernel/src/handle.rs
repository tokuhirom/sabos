// handle.rs — ユーザー空間に渡すファイルハンドル管理
//
// SABOS の設計方針に合わせて、FD のような整数ではなく
// 「不透明 + 偽造困難」な Handle を提供する。
//
// Handle は (id, token) の 2 要素で構成され、
// token が一致しないと無効になる。
//
// 今は読み取り専用の最小実装:
// - open で内容を Vec に読み込んでハンドル化
// - read は Vec の pos から読み取る
// - close でテーブルから解放
//
// 将来的には:
// - Handle の権限ビット（READ/WRITE/ENUM/EXEC）を拡張
// - IPC で Handle を移譲する
// - ストリーム/デバイスにも対応する

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;
use spin::Mutex;

use crate::user_ptr::SyscallError;

/// Handle の読み取り権限
pub const HANDLE_RIGHT_READ: u32 = 0x01;
/// Handle の書き込み権限
pub const HANDLE_RIGHT_WRITE: u32 = 0x02;

/// ユーザー空間に渡す不透明なハンドル
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Handle {
    /// テーブルのインデックス
    pub id: u64,
    /// 偽造防止用のトークン
    pub token: u64,
}

/// ハンドルの中身（カーネル内）
struct HandleEntry {
    token: u64,
    rights: u32,
    kind: HandleKind,
}

/// 今は「メモリ上のファイル」だけ扱う
enum HandleKind {
    File {
        data: Vec<u8>,
        pos: usize,
    },
}

lazy_static! {
    static ref HANDLE_TABLE: Mutex<Vec<Option<HandleEntry>>> = Mutex::new(Vec::new());
}

/// token 生成用のカウンタ
static HANDLE_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// 新しい Handle を作成して返す
pub fn create_handle(data: Vec<u8>, rights: u32) -> Handle {
    let token = next_token();
    let entry = HandleEntry {
        token,
        rights,
        kind: HandleKind::File { data, pos: 0 },
    };

    let mut table = HANDLE_TABLE.lock();

    // 空きスロットを再利用
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(entry);
            return Handle {
                id: i as u64,
                token,
            };
        }
    }

    // 末尾に追加
    let id = table.len();
    table.push(Some(entry));
    Handle {
        id: id as u64,
        token,
    }
}

/// Handle から読み取る
pub fn read(handle: &Handle, buf: &mut [u8]) -> Result<usize, SyscallError> {
    let mut table = HANDLE_TABLE.lock();
    let entry = get_entry_mut(&mut table, handle)?;

    // 権限チェック
    if (entry.rights & HANDLE_RIGHT_READ) == 0 {
        return Err(SyscallError::ReadOnly);
    }

    match &mut entry.kind {
        HandleKind::File { data, pos } => {
            if *pos >= data.len() {
                return Ok(0);  // EOF
            }

            let remaining = data.len() - *pos;
            let copy_len = core::cmp::min(remaining, buf.len());
            buf[..copy_len].copy_from_slice(&data[*pos..*pos + copy_len]);
            *pos += copy_len;
            Ok(copy_len)
        }
    }
}

/// Handle に書き込む（今は読み取り専用）
pub fn write(_handle: &Handle, _buf: &[u8]) -> Result<usize, SyscallError> {
    Err(SyscallError::ReadOnly)
}

/// Handle を閉じる
pub fn close(handle: &Handle) -> Result<(), SyscallError> {
    let mut table = HANDLE_TABLE.lock();
    let _ = get_entry(&table, handle)?;

    // token が一致したら解放
    table[handle.id as usize] = None;
    Ok(())
}

/// Handle の内容を取得（参照）
fn get_entry<'a>(
    table: &'a Vec<Option<HandleEntry>>,
    handle: &Handle,
) -> Result<&'a HandleEntry, SyscallError> {
    let idx = handle.id as usize;
    if idx >= table.len() {
        return Err(SyscallError::InvalidHandle);
    }
    match &table[idx] {
        Some(entry) if entry.token == handle.token => Ok(entry),
        _ => Err(SyscallError::InvalidHandle),
    }
}

/// Handle の内容を取得（可変参照）
fn get_entry_mut<'a>(
    table: &'a mut Vec<Option<HandleEntry>>,
    handle: &Handle,
) -> Result<&'a mut HandleEntry, SyscallError> {
    let idx = handle.id as usize;
    if idx >= table.len() {
        return Err(SyscallError::InvalidHandle);
    }
    match &mut table[idx] {
        Some(entry) if entry.token == handle.token => Ok(entry),
        _ => Err(SyscallError::InvalidHandle),
    }
}

/// token を生成（単調カウンタ + 定数）
fn next_token() -> u64 {
    let n = HANDLE_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    // 擬似乱数的に見えるように定数で混ぜる
    n.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}
