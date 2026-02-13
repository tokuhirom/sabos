// fat32_ipc.rs — Fat32IpcFs: ユーザー空間 fat32d への IPC プロキシ
//
// マイクロカーネル化の一環として、FAT32 ファイルシステム操作を
// ユーザー空間の fat32d デーモンに委譲する VFS プロキシ。
//
// ## 動作原理
//
// VFS の "/" と "/host" をこのプロキシに remount することで、
// ファイル操作（read, list_dir, create_file 等）が fat32d への IPC に変換される。
//
// ```
// ユーザータスク → syscall → VFS → Fat32IpcFs → IPC → fat32d → UserBlockDevice → virtio-blk
// ```
//
// ## IPC プロトコル
//
// リクエスト: [opcode:4][len:4][dev:4][payload...]
// レスポンス: [opcode:4][status:4][data_len:4][data...]
//
// fat32d.rs と同じオペコード・フォーマットを使用する。

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;
use lazy_static::lazy_static;
use spin::Mutex;

use crate::vfs::{FileSystem, VfsDirEntry, VfsError, VfsNode, VfsNodeKind};

// ========================================
// IPC オペコード（fat32d.rs と一致させる）
// ========================================

const OPCODE_LIST_DIR: u32 = 1;
#[allow(dead_code)]
const OPCODE_STAT: u32 = 2;
const OPCODE_READ_FILE_CHUNK: u32 = 3;
const OPCODE_CREATE_FILE: u32 = 4;
const OPCODE_DELETE_FILE: u32 = 5;
const OPCODE_CREATE_DIR: u32 = 6;
const OPCODE_DELETE_DIR: u32 = 7;

/// IPC バッファサイズ（fat32d と同じ 64 KiB）
///
/// 大きなファイル（ELF バイナリ）の転送を高速化するため、
/// 8 KiB から 64 KiB に拡大。チャンク数が 1/8 に減り、
/// コンテキストスイッチの回数を大幅に削減する。
const IPC_BUF_SIZE: usize = 65536;

/// READ_FILE_CHUNK で要求する最大チャンクサイズ。
/// IPC レスポンスから total_size(4) + ヘッダ(12) を引いた分。
const CHUNK_SIZE: usize = IPC_BUF_SIZE - 16;

/// IPC タイムアウト（ミリ秒）。
/// fat32d がディスクから大きなファイルを読み込む場合があるので余裕を持たせる。
const IPC_TIMEOUT_MS: u64 = 30_000;

lazy_static! {
    /// fat32d のタスク ID。
    /// fat32d が SYS_FS_REGISTER を呼ぶと設定される。
    /// 0 の場合は fat32d 未登録。
    static ref FAT32D_TASK_ID: Mutex<u64> = Mutex::new(0);
}

/// fat32d を VFS に登録する。
///
/// SYS_FS_REGISTER syscall から呼ばれる。
/// fat32d のタスク ID を記録し、VFS の "/" と "/host" を Fat32IpcFs に remount する。
pub fn activate_fat32d(task_id: u64) {
    *FAT32D_TASK_ID.lock() = task_id;
    crate::kprintln!("fat32d registered (task {}), remounting VFS", task_id);

    // "/" を Fat32IpcFs（dev_index=0）に切り替え
    crate::vfs::remount("/", Box::new(|| {
        Box::new(Fat32IpcFs { dev_index: 0 })
    }));

    // 2 台目の virtio-blk がある場合は "/host" も切り替え
    let dev_count = crate::virtio_blk::device_count();
    if dev_count >= 2 {
        crate::vfs::remount("/host", Box::new(|| {
            Box::new(Fat32IpcFs { dev_index: 1 })
        }));
    }
}

/// ユーザー空間の fat32d への IPC プロキシファイルシステム。
///
/// FileSystem trait を実装し、各操作を fat32d への IPC メッセージに変換する。
/// dev_index でどの virtio-blk デバイスにアクセスするか指定する。
pub struct Fat32IpcFs {
    dev_index: usize,
}

// ========================================
// IPC 通信ヘルパー
// ========================================

/// fat32d に IPC リクエストを送信してレスポンスを受け取る。
///
/// # 引数
/// - `opcode`: IPC オペコード（OPCODE_LIST_DIR 等）
/// - `dev_index`: virtio-blk デバイスインデックス
/// - `extra`: オペコード固有のペイロード（dev:4 の後に続くデータ）
///
/// # 戻り値
/// レスポンスのデータ部分（ヘッダ除去済み）。
/// status != 0 の場合は VfsError を返す。
fn ipc_request(opcode: u32, dev_index: usize, extra: &[u8]) -> Result<Vec<u8>, VfsError> {
    let fat32d_id = *FAT32D_TASK_ID.lock();
    if fat32d_id == 0 {
        // fat32d 未登録 — カーネル内 FAT32 にフォールバック
        return Err(VfsError::IoError);
    }

    // 現在のタスク ID を IPC の送信元にする
    let sender = crate::scheduler::current_task_id();

    // リクエスト: [opcode:4][len:4][dev:4][extra...]
    let payload_len = 4 + extra.len(); // dev(4) + extra
    let mut req = Vec::with_capacity(8 + payload_len);
    req.extend_from_slice(&opcode.to_le_bytes());
    req.extend_from_slice(&(payload_len as u32).to_le_bytes());
    req.extend_from_slice(&(dev_index as u32).to_le_bytes());
    req.extend_from_slice(extra);

    // fat32d にリクエストを送信
    crate::ipc::send(sender, fat32d_id, req)
        .map_err(|_| VfsError::IoError)?;

    // fat32d からレスポンスを受信（送信元フィルタリング付き）
    //
    // recv_from を使って fat32d からのメッセージのみを受け取る。
    // 同じタスクが netd 等とも IPC 通信している場合、
    // 他プロセスからのレスポンスが混入するのを防ぐ。
    let resp = crate::ipc::recv_from(sender, fat32d_id, IPC_TIMEOUT_MS)
        .map_err(|_| VfsError::IoError)?;

    // レスポンス形式: [opcode:4][status:4][data_len:4][data...]
    if resp.data.len() < 12 {
        return Err(VfsError::IoError);
    }

    let status = i32::from_le_bytes([resp.data[4], resp.data[5], resp.data[6], resp.data[7]]);
    let data_len = u32::from_le_bytes([resp.data[8], resp.data[9], resp.data[10], resp.data[11]]) as usize;

    if status != 0 {
        return Err(VfsError::NotFound);
    }

    if resp.data.len() < 12 + data_len {
        return Err(VfsError::IoError);
    }

    Ok(resp.data[12..12 + data_len].to_vec())
}

// ========================================
// IPC 経由のファイルノード（open() の戻り値）
// ========================================

/// IPC 経由で読み取ったファイルデータをラップする VfsNode。
///
/// open() 時にファイル全体を読み込み、以後は in-memory で read() に応答する。
/// カーネル内 Fat32File と同じパターン。
struct IpcFile {
    data: Vec<u8>,
}

impl VfsNode for IpcFile {
    fn kind(&self) -> VfsNodeKind {
        VfsNodeKind::File
    }

    fn size(&self) -> usize {
        self.data.len()
    }

    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<usize, VfsError> {
        if offset >= self.data.len() {
            return Ok(0);
        }
        let remaining = self.data.len() - offset;
        let to_copy = core::cmp::min(remaining, buf.len());
        buf[..to_copy].copy_from_slice(&self.data[offset..offset + to_copy]);
        Ok(to_copy)
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::NotSupported)
    }
}

// ========================================
// FileSystem trait 実装
// ========================================

impl FileSystem for Fat32IpcFs {
    fn name(&self) -> &str {
        "fat32d-ipc"
    }

    fn open(&self, path: &str) -> Result<Box<dyn VfsNode>, VfsError> {
        if path == "/" || path.is_empty() {
            return Err(VfsError::NotAFile);
        }

        // ファイル全体を read_file() で読み込んで IpcFile にラップ
        let data = self.read_file(path)?;
        Ok(Box::new(IpcFile { data }))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<VfsDirEntry>, VfsError> {
        // extra: path（UTF-8 文字列）
        let path_str = if path.is_empty() { "/" } else { path };
        let data = ipc_request(OPCODE_LIST_DIR, self.dev_index, path_str.as_bytes())?;

        // レスポンスデータ: [name_len:2][name][kind:1][size:4]... の繰り返し
        let mut entries = Vec::new();
        let mut off = 0usize;

        while off + 7 <= data.len() {
            let name_len = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
            off += 2;

            if off + name_len + 5 > data.len() {
                break;
            }

            let name = core::str::from_utf8(&data[off..off + name_len])
                .unwrap_or("")
                .to_string();
            off += name_len;

            let kind = if data[off] == 1 {
                VfsNodeKind::Directory
            } else {
                VfsNodeKind::File
            };
            off += 1;

            let size = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
            off += 4;

            entries.push(VfsDirEntry { name, kind, size });
        }

        Ok(entries)
    }

    fn create_file(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let path_bytes = path.as_bytes();

        // extra: [path_len:4][path][data]
        // IPC バッファサイズの制約: リクエスト全体が IPC_BUF_SIZE に収まる必要がある
        // リクエスト = opcode(4) + len(4) + dev(4) + path_len(4) + path + data
        let total_size = 16 + path_bytes.len() + data.len();
        if total_size > IPC_BUF_SIZE {
            // データが大きすぎる場合は IoError を返す
            // 大きなファイルはハンドルシステム経由で書き込むべき
            return Err(VfsError::IoError);
        }

        let mut extra = Vec::with_capacity(4 + path_bytes.len() + data.len());
        extra.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
        extra.extend_from_slice(path_bytes);
        extra.extend_from_slice(data);

        ipc_request(OPCODE_CREATE_FILE, self.dev_index, &extra)?;
        Ok(())
    }

    fn delete_file(&self, path: &str) -> Result<(), VfsError> {
        ipc_request(OPCODE_DELETE_FILE, self.dev_index, path.as_bytes())?;
        Ok(())
    }

    fn create_dir(&self, path: &str) -> Result<(), VfsError> {
        ipc_request(OPCODE_CREATE_DIR, self.dev_index, path.as_bytes())?;
        Ok(())
    }

    fn delete_dir(&self, path: &str) -> Result<(), VfsError> {
        ipc_request(OPCODE_DELETE_DIR, self.dev_index, path.as_bytes())?;
        Ok(())
    }

    /// ファイル全体を分割読み取りで取得する（Fat32IpcFs 最適化版）。
    ///
    /// READ_FILE_CHUNK オペコードで offset ベースの分割転送を行う。
    /// IPC バッファ 64 KiB の制約があるため、大きなファイル（ELF バイナリ等）は
    /// 複数回の IPC ラウンドトリップで転送する。
    fn read_file(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let path_bytes = path.as_bytes();
        let mut result = Vec::new();
        let mut offset: u32 = 0;
        let mut expected_total: u32 = 0;

        loop {
            // extra: [offset:4][max_len:4][path]
            let mut extra = Vec::with_capacity(8 + path_bytes.len());
            extra.extend_from_slice(&offset.to_le_bytes());
            extra.extend_from_slice(&(CHUNK_SIZE as u32).to_le_bytes());
            extra.extend_from_slice(path_bytes);

            let data = ipc_request(OPCODE_READ_FILE_CHUNK, self.dev_index, &extra)?;

            // レスポンスデータ: [total_size:4][chunk_data]
            if data.len() < 4 {
                return Err(VfsError::IoError);
            }

            let total_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);

            // 初回は total_size 分のバッファを予約
            if offset == 0 {
                expected_total = total_size;
                result.reserve(total_size as usize);
            }

            let chunk = &data[4..];
            if chunk.is_empty() {
                break;
            }

            result.extend_from_slice(chunk);
            offset += chunk.len() as u32;

            // 全データを受信したら終了
            if offset >= total_size {
                break;
            }
        }

        // デバッグ: 読み込み結果のサイズ検証
        if result.len() != expected_total as usize {
            crate::kprintln!(
                "[fat32d-ipc] WARNING: read_file({}) size mismatch: expected={}, got={}",
                path, expected_total, result.len()
            );
        }

        Ok(result)
    }
}
