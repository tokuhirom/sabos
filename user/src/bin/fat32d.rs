// fat32d.rs — FAT32 ファイルシステムサービス (user space)
//
// IPC 経由でファイルシステム操作のリクエストを受け取り、
// UserBlockDevice (syscall 経由) で virtio-blk にアクセスする。
//
// カーネル内の Fat32IpcFs が VFS プロキシとして機能し、
// 各 VFS 操作を fat32d への IPC メッセージに変換する。
//
// netd.rs と同構造:
// - _start(): allocator 初期化 → SYS_FS_REGISTER 呼出 → IPC ループ
// - fat32d_loop(): ipc_recv → opcode パース → Fat32Fs<UserBlockDevice> で処理 → ipc_send

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator_fat32d.rs"]
mod allocator;
#[path = "../syscall_fat32d.rs"]
mod syscall_fat32d;
#[path = "../blockdev_user.rs"]
mod blockdev_user;

use alloc::string::String;
use alloc::vec::Vec;
use core::panic::PanicInfo;
use crate::syscall_fat32d as syscall;
use crate::blockdev_user::UserBlockDevice;
use sabos_fat32::{Fat32Fs, ATTR_DIRECTORY};

// ========================================
// IPC オペコード
// ========================================

/// ディレクトリ一覧の取得
const OPCODE_LIST_DIR: u32 = 1;
/// ファイル/ディレクトリのメタデータ取得
const OPCODE_STAT: u32 = 2;
/// ファイルの一部を offset ベースで読み取り（大容量ファイル対応）
const OPCODE_READ_FILE_CHUNK: u32 = 3;
/// ファイル作成/上書き
const OPCODE_CREATE_FILE: u32 = 4;
/// ファイル削除
const OPCODE_DELETE_FILE: u32 = 5;
/// ディレクトリ作成
const OPCODE_CREATE_DIR: u32 = 6;
/// ディレクトリ削除
const OPCODE_DELETE_DIR: u32 = 7;

/// IPC バッファサイズ（64 KiB）
///
/// 大きなファイル（ELF バイナリ）の転送を高速化するため、
/// 8 KiB から 64 KiB に拡大。チャンク数が 1/8 に減り、
/// IPC ラウンドトリップの回数を大幅に削減する。
/// fat32d のバッファは Vec（ヒープ）で確保するため、
/// スタックオーバーフローの心配はない。
const IPC_BUF_SIZE: usize = 65536;

/// IPC recv タイムアウト（実質無制限 — fat32d は常にリクエストを待ち続ける）
const IPC_RECV_TIMEOUT_MS: u64 = 0xFFFF_FFFF;

/// ファイルキャッシュ（大容量ファイルの分割読み取り用）
///
/// READ_FILE_CHUNK は offset ベースで同じファイルを何度もリクエストする。
/// 例: 2MB の ELF を 8KB チャンクで読むと約 260 回。
/// 毎回ディスクから全読みすると遅いので、最後に読んだファイルをキャッシュする。
struct FileCache {
    /// キャッシュ対象のデバイスインデックス
    dev: usize,
    /// キャッシュ対象のパス
    path: String,
    /// ファイルの全内容
    data: Vec<u8>,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();

    // カーネルに fat32d の登録を通知する。
    // これにより VFS の "/" と "/host" が Fat32IpcFs（IPC プロキシ）に切り替わり、
    // 以後のファイル操作は fat32d 経由になる。
    syscall::fs_register();

    fat32d_loop();
}

/// メインの IPC ループ。
///
/// netd と同パターン: ipc_recv でリクエストを待ち、
/// opcode に応じて Fat32Fs の操作を実行し、結果を ipc_send で返す。
fn fat32d_loop() -> ! {
    // バッファをヒープに確保（64 KiB × 2 はスタックに載せられない）
    let mut buf = alloc::vec![0u8; IPC_BUF_SIZE];
    let mut resp = alloc::vec![0u8; IPC_BUF_SIZE];
    let mut sender: u64 = 0;

    // デバイスインデックス 0（disk.img）の Fat32Fs を初期化
    let mut fs0 = Fat32Fs::new_with_device(UserBlockDevice { dev_index: 0 })
        .expect("fat32d: failed to init Fat32Fs for dev 0");

    // デバイスインデックス 1（hostfs.img）の Fat32Fs を初期化（失敗しても続行）
    let mut fs1 = Fat32Fs::new_with_device(UserBlockDevice { dev_index: 1 }).ok();

    // 分割読み取り用のファイルキャッシュ
    let mut cache: Option<FileCache> = None;

    loop {
        let n = syscall::ipc_recv(&mut sender, &mut buf, IPC_RECV_TIMEOUT_MS);
        if n < 0 {
            continue;
        }
        let n = n as usize;

        // リクエスト形式: [opcode:4][len:4][payload]
        if n < 8 {
            continue;
        }

        let opcode = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if 8 + len > n {
            continue;
        }
        let payload = &buf[8..8 + len];

        let mut resp_len = 0usize;
        let mut status: i32 = 0;

        // payload の先頭 4 バイトは dev_index（どの virtio-blk デバイスか）
        if payload.len() < 4 {
            status = -1;
        } else {
            let dev = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
            let rest = &payload[4..];

            // デバイスに対応する Fat32Fs を取得
            let fs: Option<&mut Fat32Fs<UserBlockDevice>> = match dev {
                0 => Some(&mut fs0),
                1 => fs1.as_mut(),
                _ => None,
            };

            if let Some(fs) = fs {
                match opcode {
                    OPCODE_LIST_DIR => {
                        handle_list_dir(fs, rest, &mut resp, &mut resp_len, &mut status);
                    }
                    OPCODE_STAT => {
                        handle_stat(fs, rest, &mut resp, &mut resp_len, &mut status);
                    }
                    OPCODE_READ_FILE_CHUNK => {
                        handle_read_file_chunk(
                            fs, dev, rest, &mut resp, &mut resp_len, &mut status, &mut cache,
                        );
                    }
                    OPCODE_CREATE_FILE => {
                        handle_create_file(fs, rest, &mut status);
                        // 書き込み操作後はキャッシュを無効化
                        cache = None;
                    }
                    OPCODE_DELETE_FILE => {
                        handle_delete_file(fs, rest, &mut status);
                        cache = None;
                    }
                    OPCODE_CREATE_DIR => {
                        handle_create_dir(fs, rest, &mut status);
                    }
                    OPCODE_DELETE_DIR => {
                        handle_delete_dir(fs, rest, &mut status);
                    }
                    _ => {
                        status = -1;
                    }
                }
            } else {
                // 不明なデバイスインデックス
                status = -2;
            }
        }

        // レスポンス: [opcode:4][status:4][data_len:4][data]
        resp[0..4].copy_from_slice(&opcode.to_le_bytes());
        resp[4..8].copy_from_slice(&status.to_le_bytes());
        resp[8..12].copy_from_slice(&(resp_len as u32).to_le_bytes());

        let total = 12 + resp_len;
        let _ = syscall::ipc_send(sender, &resp[..total]);
    }
}

// ========================================
// オペコードハンドラ
// ========================================

/// LIST_DIR: ディレクトリ一覧を返す
///
/// rest: path（UTF-8 文字列）
/// response data: [name_len:2][name][kind:1][size:4]... の繰り返し
///
/// kind: 0 = ファイル, 1 = ディレクトリ
fn handle_list_dir(
    fs: &mut Fat32Fs<UserBlockDevice>,
    rest: &[u8],
    resp: &mut [u8],
    resp_len: &mut usize,
    status: &mut i32,
) {
    let path = core::str::from_utf8(rest).unwrap_or("");
    match fs.list_dir(path) {
        Ok(entries) => {
            // レスポンスデータ領域は resp[12..] から
            let mut off = 12usize;
            for entry in &entries {
                let name_bytes = entry.name.as_bytes();
                let name_len = name_bytes.len();
                let kind: u8 = if entry.attr & ATTR_DIRECTORY != 0 { 1 } else { 0 };
                // 各エントリ: name_len(2) + name + kind(1) + size(4) = 7 + name_len
                let entry_size = 2 + name_len + 1 + 4;
                if off + entry_size > resp.len() {
                    break; // バッファ満杯 — 収まる分だけ返す
                }
                resp[off..off + 2].copy_from_slice(&(name_len as u16).to_le_bytes());
                off += 2;
                resp[off..off + name_len].copy_from_slice(name_bytes);
                off += name_len;
                resp[off] = kind;
                off += 1;
                resp[off..off + 4].copy_from_slice(&entry.size.to_le_bytes());
                off += 4;
            }
            *resp_len = off - 12;
        }
        Err(_) => {
            *status = -1;
        }
    }
}

/// STAT: ファイル/ディレクトリのメタデータを返す
///
/// rest: path（UTF-8 文字列）
/// response data: [size:4][attr:1]
fn handle_stat(
    fs: &mut Fat32Fs<UserBlockDevice>,
    rest: &[u8],
    resp: &mut [u8],
    resp_len: &mut usize,
    status: &mut i32,
) {
    let path = core::str::from_utf8(rest).unwrap_or("");

    // ルートディレクトリは特別扱い（親ディレクトリから探せないため）
    if path == "/" || path.is_empty() {
        resp[12..16].copy_from_slice(&0u32.to_le_bytes());
        resp[16] = ATTR_DIRECTORY;
        *resp_len = 5;
        return;
    }

    // パスを親ディレクトリ + ファイル名に分割して list_dir で探す
    if let Some((size, attr)) = stat_entry(fs, path) {
        resp[12..16].copy_from_slice(&size.to_le_bytes());
        resp[16] = attr;
        *resp_len = 5;
    } else {
        *status = -1;
    }
}

/// READ_FILE_CHUNK: offset ベースでファイルの一部を返す
///
/// rest: [offset:4][max_len:4] + path（UTF-8 文字列）
/// response data: [total_size:4] + chunk_data
///
/// 大容量ファイル（ELF バイナリ等）を IPC バッファサイズの制約内で
/// 分割転送するためのオペコード。カーネル側の Fat32IpcFs が
/// offset を 0 から始めて total_size に達するまでループ呼び出しする。
fn handle_read_file_chunk(
    fs: &mut Fat32Fs<UserBlockDevice>,
    dev: usize,
    rest: &[u8],
    resp: &mut [u8],
    resp_len: &mut usize,
    status: &mut i32,
    cache: &mut Option<FileCache>,
) {
    if rest.len() < 8 {
        *status = -1;
        return;
    }

    let offset = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    let max_len = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
    let path = core::str::from_utf8(&rest[8..]).unwrap_or("");

    // キャッシュヒット判定: 同じデバイス・同じパスならディスク読み直しを省略
    let cache_hit = cache.as_ref().is_some_and(|c| c.dev == dev && c.path == path);

    if !cache_hit {
        // キャッシュミス: ファイル全体を読み込んでキャッシュ
        match fs.read_file(path) {
            Ok(data) => {
                *cache = Some(FileCache {
                    dev,
                    path: String::from(path),
                    data,
                });
            }
            Err(_) => {
                *status = -1;
                return;
            }
        }
    }

    // キャッシュからチャンクを切り出して返す
    let cached = cache.as_ref().unwrap();
    let total_size = cached.data.len();

    // レスポンス: [total_size:4][chunk_data]
    resp[12..16].copy_from_slice(&(total_size as u32).to_le_bytes());

    let available = if offset < total_size { total_size - offset } else { 0 };
    let chunk_len = core::cmp::min(available, max_len);
    let chunk_len = core::cmp::min(chunk_len, resp.len() - 16);

    if chunk_len > 0 {
        resp[16..16 + chunk_len].copy_from_slice(&cached.data[offset..offset + chunk_len]);
    }
    *resp_len = 4 + chunk_len;
}

/// CREATE_FILE: ファイル作成/上書き
///
/// rest: [path_len:4] + path + data
fn handle_create_file(
    fs: &mut Fat32Fs<UserBlockDevice>,
    rest: &[u8],
    status: &mut i32,
) {
    if rest.len() < 4 {
        *status = -1;
        return;
    }
    let path_len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    if rest.len() < 4 + path_len {
        *status = -1;
        return;
    }
    let path = core::str::from_utf8(&rest[4..4 + path_len]).unwrap_or("");
    let data = &rest[4 + path_len..];

    if fs.create_file(path, data).is_err() {
        *status = -1;
    }
}

/// DELETE_FILE: ファイル削除
///
/// rest: path（UTF-8 文字列）
fn handle_delete_file(
    fs: &mut Fat32Fs<UserBlockDevice>,
    rest: &[u8],
    status: &mut i32,
) {
    let path = core::str::from_utf8(rest).unwrap_or("");
    if fs.delete_file(path).is_err() {
        *status = -1;
    }
}

/// CREATE_DIR: ディレクトリ作成
///
/// rest: path（UTF-8 文字列）
fn handle_create_dir(
    fs: &mut Fat32Fs<UserBlockDevice>,
    rest: &[u8],
    status: &mut i32,
) {
    let path = core::str::from_utf8(rest).unwrap_or("");
    if fs.create_dir(path).is_err() {
        *status = -1;
    }
}

/// DELETE_DIR: ディレクトリ削除
///
/// rest: path（UTF-8 文字列）
fn handle_delete_dir(
    fs: &mut Fat32Fs<UserBlockDevice>,
    rest: &[u8],
    status: &mut i32,
) {
    let path = core::str::from_utf8(rest).unwrap_or("");
    if fs.delete_dir(path).is_err() {
        *status = -1;
    }
}

// ========================================
// ヘルパー
// ========================================

/// パスからファイル/ディレクトリのメタデータ（size, attr）を取得する。
///
/// Fat32Fs に直接の stat メソッドがないので、
/// 親ディレクトリを list_dir して名前が一致するエントリを探す。
fn stat_entry(fs: &mut Fat32Fs<UserBlockDevice>, path: &str) -> Option<(u32, u8)> {
    // パスの末尾スラッシュを除去
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }

    // 最後の '/' で親ディレクトリとファイル名に分割
    let (parent, name) = if let Some(pos) = path.rfind('/') {
        (&path[..pos], &path[pos + 1..])
    } else {
        ("/", path)
    };
    let parent = if parent.is_empty() { "/" } else { parent };

    // 親ディレクトリを list_dir して名前が一致するエントリを探す
    if let Ok(entries) = fs.list_dir(parent) {
        for entry in entries {
            if entry.name.eq_ignore_ascii_case(name) {
                return Some((entry.size, entry.attr));
            }
        }
    }
    None
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
