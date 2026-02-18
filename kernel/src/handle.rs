// handle.rs — ユーザー空間に渡すファイルハンドル管理
//
// SABOS の設計方針に合わせて、FD のような整数ではなく
// 「不透明 + 偽造困難」な Handle を提供する。
//
// ## Capability-based Security
//
// Handle は (id, token) の 2 要素で構成され、
// token が一致しないと無効になる。
//
// 各 Handle には権限ビット (rights) が付与されており、
// 操作時に権限チェックを行う。権限は縮小のみ可能で拡大はできない
// （Capability の原則）。
//
// ## 権限ビット
//
// - HANDLE_RIGHT_READ:   ファイル内容の読み取り
// - HANDLE_RIGHT_WRITE:  ファイル内容の書き込み
// - HANDLE_RIGHT_SEEK:   ファイルポジションの変更
// - HANDLE_RIGHT_STAT:   メタデータの取得
// - HANDLE_RIGHT_ENUM:   ディレクトリ内のエントリ列挙
// - HANDLE_RIGHT_CREATE: ディレクトリ内にファイルを作成
// - HANDLE_RIGHT_DELETE: ディレクトリ内のファイルを削除
// - HANDLE_RIGHT_LOOKUP: 相対パスでファイルを開く（openat 用）
//
// ## ハンドルの種類
//
// - File: 通常のファイル（読み取り・書き込み）
// - Directory: ディレクトリ（列挙・作成・削除・lookup）

// 将来使用する権限ビットと関数の dead_code 警告を抑制
#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;
use spin::Mutex;

use crate::user_ptr::SyscallError;

// =================================================================
// 権限ビットの定義
// =================================================================

/// Handle の読み取り権限（ファイル内容を読む）
pub const HANDLE_RIGHT_READ: u32 = 0x0001;
/// Handle の書き込み権限（ファイル内容を書く）
pub const HANDLE_RIGHT_WRITE: u32 = 0x0002;
/// Handle のシーク権限（ファイルポジションを変更）
pub const HANDLE_RIGHT_SEEK: u32 = 0x0004;
/// Handle のメタデータ取得権限（サイズ等を取得）
pub const HANDLE_RIGHT_STAT: u32 = 0x0008;
/// Handle のディレクトリ列挙権限（ディレクトリ内のエントリ一覧）
pub const HANDLE_RIGHT_ENUM: u32 = 0x0010;
/// Handle のファイル作成権限（ディレクトリ内にファイルを作成）
pub const HANDLE_RIGHT_CREATE: u32 = 0x0020;
/// Handle のファイル削除権限（ディレクトリ内のファイルを削除）
pub const HANDLE_RIGHT_DELETE: u32 = 0x0040;
/// Handle の相対パス解決権限（openat でファイルを開く）
pub const HANDLE_RIGHT_LOOKUP: u32 = 0x0080;

/// 読み取り専用ファイル用の権限セット
pub const HANDLE_RIGHTS_FILE_READ: u32 = HANDLE_RIGHT_READ | HANDLE_RIGHT_SEEK | HANDLE_RIGHT_STAT;

/// 読み書き可能ファイル用の権限セット
pub const HANDLE_RIGHTS_FILE_RW: u32 = HANDLE_RIGHT_READ | HANDLE_RIGHT_WRITE | HANDLE_RIGHT_SEEK | HANDLE_RIGHT_STAT;

/// ディレクトリ用の権限セット（フルアクセス）
pub const HANDLE_RIGHTS_DIRECTORY: u32 = HANDLE_RIGHT_STAT | HANDLE_RIGHT_ENUM | HANDLE_RIGHT_CREATE | HANDLE_RIGHT_DELETE | HANDLE_RIGHT_LOOKUP;

/// ディレクトリ用の権限セット（読み取りのみ）
pub const HANDLE_RIGHTS_DIRECTORY_READ: u32 = HANDLE_RIGHT_STAT | HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP;

// =================================================================
// Handle 構造体
// =================================================================

/// ユーザー空間に渡す不透明なハンドル
///
/// ユーザーは id と token のペアを保持し、システムコールで渡す。
/// カーネル側で token を検証し、一致しない場合は InvalidHandle エラーを返す。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Handle {
    /// テーブルのインデックス
    pub id: u64,
    /// 偽造防止用のトークン
    pub token: u64,
}

/// ハンドルの種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    /// 通常のファイル
    File,
    /// ディレクトリ
    Directory,
    /// パイプの読み取り端
    PipeRead,
    /// パイプの書き込み端
    PipeWrite,
}

/// ハンドルの中身（カーネル内）
struct HandleEntry {
    /// 偽造防止用のトークン
    token: u64,
    /// 権限ビット
    rights: u32,
    /// ハンドルの種類
    kind: HandleKind,
    /// ファイルシステム上のパス（openat の基準パスとして使用）
    path: String,
    /// ファイルデータ（File の場合）
    data: Vec<u8>,
    /// 現在のファイルポジション（File の場合）
    pos: usize,
    /// 書き込みがあったかどうか（close 時に FAT32 に書き戻す）
    dirty: bool,
    /// パイプ ID（PipeRead / PipeWrite の場合のみ使用）
    pipe_id: Option<usize>,
}

lazy_static! {
    static ref HANDLE_TABLE: Mutex<Vec<Option<HandleEntry>>> = Mutex::new(Vec::new());
}

/// token 生成用のカウンタ
static HANDLE_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

// =================================================================
// Handle の作成
// =================================================================

/// 新しいファイル Handle を作成して返す
///
/// # 引数
/// - `data`: ファイルの内容
/// - `rights`: 権限ビット
///
/// # 戻り値
/// 作成された Handle
pub fn create_handle(data: Vec<u8>, rights: u32) -> Handle {
    create_handle_with_path(data, rights, String::new())
}

/// パス情報付きでファイル Handle を作成する
///
/// # 引数
/// - `data`: ファイルの内容
/// - `rights`: 権限ビット
/// - `path`: ファイルのパス
///
/// # 戻り値
/// 作成された Handle
pub fn create_handle_with_path(data: Vec<u8>, rights: u32, path: String) -> Handle {
    let token = next_token();
    let entry = HandleEntry {
        token,
        rights,
        kind: HandleKind::File,
        path,
        data,
        pos: 0,
        dirty: false,
        pipe_id: None,
    };

    insert_entry(entry, token)
}

/// ディレクトリ Handle を作成する
///
/// # 引数
/// - `path`: ディレクトリのパス
/// - `rights`: 権限ビット
///
/// # 戻り値
/// 作成された Handle
pub fn create_directory_handle(path: String, rights: u32) -> Handle {
    let token = next_token();
    let entry = HandleEntry {
        token,
        rights,
        kind: HandleKind::Directory,
        path,
        data: Vec::new(),
        pos: 0,
        dirty: false,
        pipe_id: None,
    };

    insert_entry(entry, token)
}

/// HandleEntry をテーブルに挿入する（内部ヘルパー）
fn insert_entry(entry: HandleEntry, token: u64) -> Handle {
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

/// 既存のハンドルを複製する（IPC 経由の Capability 委譲用）
///
/// 元ハンドルと同じ rights/kind/path/data を持つ新ハンドルを作成する。
/// token は新規生成、pos は 0 にリセットする。
///
/// # 引数
/// - `handle`: 複製元のハンドル
///
/// # 戻り値
/// 新しい Handle（独立した token を持つ）
///
/// # エラー
/// - `InvalidHandle`: ハンドルが無効
pub fn duplicate_handle(handle: &Handle) -> Result<Handle, SyscallError> {
    let table = HANDLE_TABLE.lock();
    let entry = get_entry(&table, handle)?;

    let new_token = next_token();
    let kind = entry.kind;
    let pipe_id = entry.pipe_id;
    let new_entry = HandleEntry {
        token: new_token,
        rights: entry.rights,
        kind,
        path: entry.path.clone(),
        data: entry.data.clone(),
        pos: 0,     // ポジションは先頭にリセット
        dirty: false,
        pipe_id,
    };

    drop(table); // ロックを解放してから insert_entry を呼ぶ

    // パイプの書き込み端を複製する場合は参照カウントをインクリメント
    if kind == HandleKind::PipeWrite {
        if let Some(pid) = pipe_id {
            crate::pipe::add_writer(pid);
        }
    }

    Ok(insert_entry(new_entry, new_token))
}

// =================================================================
// Handle の操作
// =================================================================

/// Handle から読み取る
///
/// # 引数
/// - `handle`: 読み取り元のハンドル
/// - `buf`: 読み取り先バッファ
///
/// # 戻り値
/// 読み取ったバイト数
///
/// # エラー
/// - `InvalidHandle`: ハンドルが無効
/// - `PermissionDenied`: READ 権限がない
pub fn read(handle: &Handle, buf: &mut [u8]) -> Result<usize, SyscallError> {
    let mut table = HANDLE_TABLE.lock();
    let entry = get_entry_mut(&mut table, handle)?;

    // 権限チェック
    if (entry.rights & HANDLE_RIGHT_READ) == 0 {
        return Err(SyscallError::PermissionDenied);
    }

    // パイプの読み取りはパイプモジュールに委譲
    if entry.kind == HandleKind::PipeRead {
        let pipe_id = entry.pipe_id.ok_or(SyscallError::InvalidHandle)?;
        drop(table); // パイプモジュールのロックを取る前にハンドルテーブルのロックを解放
        return match crate::pipe::read(pipe_id, buf) {
            Ok(n) => Ok(n),
            Err(crate::pipe::PipeError::WouldBlock) => Err(SyscallError::WouldBlock),
            Err(_) => Err(SyscallError::Other),
        };
    }

    // ファイルのみ読み取り可能
    if entry.kind != HandleKind::File {
        return Err(SyscallError::NotSupported);
    }

    if entry.pos >= entry.data.len() {
        return Ok(0);  // EOF
    }

    let remaining = entry.data.len() - entry.pos;
    let copy_len = core::cmp::min(remaining, buf.len());
    buf[..copy_len].copy_from_slice(&entry.data[entry.pos..entry.pos + copy_len]);
    entry.pos += copy_len;
    Ok(copy_len)
}

/// Handle に書き込む
///
/// インメモリの data バッファに書き込み、dirty フラグを立てる。
/// 実際の FAT32 への書き戻しは close() 時に行う（write-back 方式）。
///
/// # 引数
/// - `handle`: 書き込み先のハンドル
/// - `buf`: 書き込むデータ
///
/// # 戻り値
/// 書き込んだバイト数
///
/// # エラー
/// - `InvalidHandle`: ハンドルが無効
/// - `PermissionDenied`: WRITE 権限がない
/// - `NotSupported`: ファイル以外への書き込み
pub fn write(handle: &Handle, buf: &[u8]) -> Result<usize, SyscallError> {
    let mut table = HANDLE_TABLE.lock();
    let entry = get_entry_mut(&mut table, handle)?;

    // WRITE 権限チェック
    if (entry.rights & HANDLE_RIGHT_WRITE) == 0 {
        return Err(SyscallError::PermissionDenied);
    }

    // パイプの書き込みはパイプモジュールに委譲
    if entry.kind == HandleKind::PipeWrite {
        let pipe_id = entry.pipe_id.ok_or(SyscallError::InvalidHandle)?;
        drop(table); // パイプモジュールのロックを取る前にハンドルテーブルのロックを解放
        return match crate::pipe::write(pipe_id, buf) {
            Ok(n) => Ok(n),
            Err(crate::pipe::PipeError::BrokenPipe) => Err(SyscallError::BrokenPipe),
            Err(_) => Err(SyscallError::Other),
        };
    }

    // ファイルのみ書き込み可能
    if entry.kind != HandleKind::File {
        return Err(SyscallError::NotSupported);
    }

    // pos 位置に書き込み（必要なら data を拡張）
    let end = entry.pos + buf.len();
    if end > entry.data.len() {
        entry.data.resize(end, 0);
    }
    entry.data[entry.pos..end].copy_from_slice(buf);
    entry.pos = end;
    entry.dirty = true;
    Ok(buf.len())
}

/// Handle を閉じる
///
/// dirty フラグが立っているファイルは FAT32 に書き戻してから解放する。
/// 書き戻しは delete + create のパターン（既存の sys_file_write と同じ方式）。
///
/// # 引数
/// - `handle`: 閉じるハンドル
///
/// # 戻り値
/// 成功時は Ok(())
///
/// # エラー
/// - `InvalidHandle`: ハンドルが無効
pub fn close(handle: &Handle) -> Result<(), SyscallError> {
    let mut table = HANDLE_TABLE.lock();
    let entry = get_entry(&table, handle)?;

    // パイプの場合はパイプモジュールに閉鎖を委譲
    match entry.kind {
        HandleKind::PipeRead => {
            let pipe_id = entry.pipe_id.ok_or(SyscallError::InvalidHandle)?;
            table[handle.id as usize] = None;
            drop(table);
            crate::pipe::close_reader(pipe_id);
            return Ok(());
        }
        HandleKind::PipeWrite => {
            let pipe_id = entry.pipe_id.ok_or(SyscallError::InvalidHandle)?;
            table[handle.id as usize] = None;
            drop(table);
            crate::pipe::close_writer(pipe_id);
            return Ok(());
        }
        _ => {}
    }

    // dirty なファイルを FAT32 に書き戻す
    let needs_flush = entry.dirty && !entry.path.is_empty();
    if needs_flush {
        let path = entry.path.clone();
        let data = entry.data.clone();
        // テーブルからエントリを削除してからロックを解放
        // （FAT32 操作中にデッドロックしないように）
        table[handle.id as usize] = None;
        drop(table);

        // VFS 経由で書き戻し: 既存ファイルを削除してから新規作成
        let _ = crate::vfs::delete_file(&path); // 既存ファイルがなくてもエラーにしない
        crate::vfs::create_file(&path, &data).map_err(|_| SyscallError::Other)?;
        return Ok(());
    }

    // dirty でなければそのまま解放
    table[handle.id as usize] = None;
    Ok(())
}

// =================================================================
// Capability-based 権限操作
// =================================================================

/// ハンドルの権限を縮小する（Capability の原則）
///
/// 新しい権限は現在の権限の部分集合でなければならない。
/// 権限の拡大は許可されない（セキュリティの要）。
///
/// # 引数
/// - `handle`: 元のハンドル
/// - `new_rights`: 新しい権限（縮小のみ可）
///
/// # 戻り値
/// 権限を縮小した新しいハンドル
///
/// # エラー
/// - `InvalidHandle`: ハンドルが無効
/// - `PermissionDenied`: 権限の拡大を試みた場合
pub fn restrict_rights(handle: &Handle, new_rights: u32) -> Result<Handle, SyscallError> {
    let table = HANDLE_TABLE.lock();
    let entry = get_entry(&table, handle)?;

    // 権限の拡大を検出（new_rights に entry.rights にないビットがある）
    if (new_rights & !entry.rights) != 0 {
        return Err(SyscallError::PermissionDenied);
    }

    // 実際に適用される権限（縮小のみ）
    let restricted_rights = entry.rights & new_rights;

    // 新しいハンドルを作成（データをクローン）
    let new_token = next_token();
    let new_entry = HandleEntry {
        token: new_token,
        rights: restricted_rights,
        kind: entry.kind,
        path: entry.path.clone(),
        data: entry.data.clone(),
        pos: entry.pos,
        dirty: false,
        pipe_id: entry.pipe_id,
    };

    drop(table); // ロックを解放してから insert_entry を呼ぶ
    Ok(insert_entry(new_entry, new_token))
}

/// ハンドルの権限を取得する
///
/// # 引数
/// - `handle`: 対象のハンドル
///
/// # 戻り値
/// 権限ビット
///
/// # エラー
/// - `InvalidHandle`: ハンドルが無効
pub fn get_rights(handle: &Handle) -> Result<u32, SyscallError> {
    let table = HANDLE_TABLE.lock();
    let entry = get_entry(&table, handle)?;
    Ok(entry.rights)
}

/// ハンドルが指定の権限を持っているか確認する
///
/// # 引数
/// - `handle`: 対象のハンドル
/// - `required_rights`: 必要な権限ビット
///
/// # 戻り値
/// すべての権限を持っていれば Ok(()), なければ Err(PermissionDenied)
pub fn check_rights(handle: &Handle, required_rights: u32) -> Result<(), SyscallError> {
    let table = HANDLE_TABLE.lock();
    let entry = get_entry(&table, handle)?;

    if (entry.rights & required_rights) == required_rights {
        Ok(())
    } else {
        Err(SyscallError::PermissionDenied)
    }
}

/// ハンドルの種類を取得する
///
/// # 引数
/// - `handle`: 対象のハンドル
///
/// # 戻り値
/// HandleKind（File または Directory）
pub fn get_kind(handle: &Handle) -> Result<HandleKind, SyscallError> {
    let table = HANDLE_TABLE.lock();
    let entry = get_entry(&table, handle)?;
    Ok(entry.kind)
}

/// ハンドルのパスを取得する
///
/// # 引数
/// - `handle`: 対象のハンドル
///
/// # 戻り値
/// ファイルまたはディレクトリのパス
pub fn get_path(handle: &Handle) -> Result<String, SyscallError> {
    let table = HANDLE_TABLE.lock();
    let entry = get_entry(&table, handle)?;
    Ok(entry.path.clone())
}

/// ハンドルのファイルサイズを取得する
///
/// # 引数
/// - `handle`: 対象のハンドル
///
/// # 戻り値
/// ファイルサイズ（バイト）
///
/// # エラー
/// - `InvalidHandle`: ハンドルが無効
/// - `PermissionDenied`: STAT 権限がない
pub fn get_size(handle: &Handle) -> Result<usize, SyscallError> {
    let table = HANDLE_TABLE.lock();
    let entry = get_entry(&table, handle)?;

    // STAT 権限チェック
    if (entry.rights & HANDLE_RIGHT_STAT) == 0 {
        return Err(SyscallError::PermissionDenied);
    }

    Ok(entry.data.len())
}

/// ハンドルのメタデータ（stat 情報）
///
/// ファイルサイズ、種別、権限をまとめて返すための構造体。
/// ユーザー空間に直接コピーされるため #[repr(C)] で固定レイアウト。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HandleStat {
    /// ファイルサイズ（バイト）
    pub size: u64,
    /// ハンドルの種別（0 = File, 1 = Directory）
    pub kind: u64,
    /// 現在のハンドルの権限ビット
    pub rights: u64,
}

/// ハンドルのメタデータを取得する
///
/// STAT 権限が必要。ファイルサイズ・種別・権限ビットをまとめて返す。
///
/// # 引数
/// - `handle`: 対象のハンドル
///
/// # 戻り値
/// HandleStat 構造体
///
/// # エラー
/// - `InvalidHandle`: ハンドルが無効
/// - `PermissionDenied`: STAT 権限がない
pub fn stat(handle: &Handle) -> Result<HandleStat, SyscallError> {
    let table = HANDLE_TABLE.lock();
    let entry = get_entry(&table, handle)?;

    // STAT 権限チェック
    if (entry.rights & HANDLE_RIGHT_STAT) == 0 {
        return Err(SyscallError::PermissionDenied);
    }

    Ok(HandleStat {
        size: entry.data.len() as u64,
        kind: match entry.kind {
            HandleKind::File => 0,
            HandleKind::Directory => 1,
            HandleKind::PipeRead => 2,
            HandleKind::PipeWrite => 3,
        },
        rights: entry.rights as u64,
    })
}

/// シーク方向の定数: ファイル先頭からの絶対位置
pub const SEEK_SET: u64 = 0;
/// シーク方向の定数: 現在位置からの相対オフセット
pub const SEEK_CUR: u64 = 1;
/// シーク方向の定数: ファイル末尾からの相対オフセット
pub const SEEK_END: u64 = 2;

/// ファイルポジションを変更する
///
/// SEEK 権限が必要。whence に基づいて新しい pos を計算する。
/// 範囲外になった場合は 0 にクランプ or ファイルサイズにクランプする。
///
/// # 引数
/// - `handle`: 対象のハンドル
/// - `offset`: オフセット値（i64、負の値あり）
/// - `whence`: シーク方向（SEEK_SET / SEEK_CUR / SEEK_END）
///
/// # 戻り値
/// 新しいファイルポジション
///
/// # エラー
/// - `InvalidHandle`: ハンドルが無効
/// - `PermissionDenied`: SEEK 権限がない
/// - `NotSupported`: ファイル以外へのシーク
/// - `InvalidArgument`: 不正な whence 値
pub fn seek(handle: &Handle, offset: i64, whence: u64) -> Result<u64, SyscallError> {
    let mut table = HANDLE_TABLE.lock();
    let entry = get_entry_mut(&mut table, handle)?;

    // SEEK 権限チェック
    if (entry.rights & HANDLE_RIGHT_SEEK) == 0 {
        return Err(SyscallError::PermissionDenied);
    }

    // ファイルのみシーク可能
    if entry.kind != HandleKind::File {
        return Err(SyscallError::NotSupported);
    }

    let size = entry.data.len() as i64;
    let base = match whence {
        SEEK_SET => 0i64,
        SEEK_CUR => entry.pos as i64,
        SEEK_END => size,
        _ => return Err(SyscallError::InvalidArgument),
    };

    // 新しいポジションを計算（範囲外は 0 〜 size にクランプ）
    let new_pos = (base + offset).max(0).min(size) as usize;
    entry.pos = new_pos;
    Ok(new_pos as u64)
}

// =================================================================
// 内部ヘルパー関数
// =================================================================

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

// =================================================================
// パイプハンドルの作成
// =================================================================

/// パイプを作成し、読み取り用と書き込み用の Handle ペアを返す
///
/// 内部で pipe::create() を呼んで pipe_id を取得し、
/// PipeRead / PipeWrite の 2 つのハンドルを作成する。
///
/// # 戻り値
/// (read_handle, write_handle) のタプル
pub fn create_pipe_handles() -> (Handle, Handle) {
    let pipe_id = crate::pipe::create();

    // 読み取り用ハンドル
    let read_token = next_token();
    let read_entry = HandleEntry {
        token: read_token,
        rights: HANDLE_RIGHT_READ,
        kind: HandleKind::PipeRead,
        path: String::new(),
        data: Vec::new(),
        pos: 0,
        dirty: false,
        pipe_id: Some(pipe_id),
    };
    let read_handle = insert_entry(read_entry, read_token);

    // 書き込み用ハンドル
    let write_token = next_token();
    let write_entry = HandleEntry {
        token: write_token,
        rights: HANDLE_RIGHT_WRITE,
        kind: HandleKind::PipeWrite,
        path: String::new(),
        data: Vec::new(),
        pos: 0,
        dirty: false,
        pipe_id: Some(pipe_id),
    };
    let write_handle = insert_entry(write_entry, write_token);

    (read_handle, write_handle)
}

/// token を生成（単調カウンタ + 定数）
fn next_token() -> u64 {
    let n = HANDLE_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    // 擬似乱数的に見えるように定数で混ぜる
    n.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}
