// procfs.rs — /proc 疑似ファイルシステム
//
// procfs はカーネル内部情報を「表示するための疑似ファイルシステム」。
// 実際のディスク上にはファイルが存在せず、読み取り時に動的に内容を生成する。
//
// ## 設計原則（CLAUDE.md より）
//
// - procfs は書き込み禁止
// - /proc はカーネル内部情報を「表示するための疑似ファイルシステム」として扱う
// - 変更操作（write/delete）は将来も許可しない方針
// - procfs の出力は JSON 形式に統一する
//
// ## 対応ファイル
//
// - /proc/meminfo: メモリ情報（JSON 形式）
// - /proc/tasks: タスク一覧（JSON 形式）

// VFS trait 実装は将来の統合で使用するため、dead_code 警告を抑制
#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

use alloc::string::String;

use crate::vfs::{FileSystem, VfsNode, VfsNodeKind, VfsDirEntry, VfsError};

/// procfs のルートパス
pub const PROC_ROOT: &str = "/proc";
/// メモリ情報ファイルのパス
const PROC_MEMINFO: &str = "meminfo";
/// タスク一覧ファイルのパス
const PROC_TASKS: &str = "tasks";

/// procfs ファイルシステム
pub struct ProcFs;

impl ProcFs {
    /// ProcFs インスタンスを作成する
    pub fn new() -> Self {
        Self
    }
}

/// procfs のファイルノード
///
/// 読み取り時に動的に内容を生成する。
pub struct ProcNode {
    /// ファイルの内容（遅延生成）
    data: Vec<u8>,
}

impl ProcNode {
    /// 新しい ProcNode を作成する
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl VfsNode for ProcNode {
    fn kind(&self) -> VfsNodeKind {
        VfsNodeKind::File
    }

    fn size(&self) -> usize {
        self.data.len()
    }

    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<usize, VfsError> {
        if offset >= self.data.len() {
            return Ok(0); // EOF
        }

        let remaining = self.data.len() - offset;
        let to_copy = core::cmp::min(remaining, buf.len());
        buf[..to_copy].copy_from_slice(&self.data[offset..offset + to_copy]);
        Ok(to_copy)
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, VfsError> {
        // procfs は書き込み禁止
        Err(VfsError::ReadOnly)
    }
}

impl FileSystem for ProcFs {
    fn name(&self) -> &str {
        "procfs"
    }

    fn open(&self, path: &str) -> Result<Box<dyn VfsNode>, VfsError> {
        // パスを正規化（先頭の "/" を除去）
        let path = path.trim().trim_start_matches('/');

        // ファイルの内容を生成
        let data = match path {
            PROC_MEMINFO | "proc/meminfo" => generate_meminfo(),
            PROC_TASKS | "proc/tasks" => generate_tasks(),
            "" | "proc" | "proc/" => return Err(VfsError::NotAFile),
            _ => return Err(VfsError::NotFound),
        };

        Ok(Box::new(ProcNode::new(data)))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<VfsDirEntry>, VfsError> {
        // パスを正規化
        let path = path.trim().trim_start_matches('/');

        // ルートディレクトリのみサポート
        if !path.is_empty() && path != "proc" && path != "proc/" {
            return Err(VfsError::NotFound);
        }

        // procfs のファイル一覧
        let entries = vec![
            VfsDirEntry {
                name: String::from("meminfo"),
                kind: VfsNodeKind::File,
                size: 0, // 動的生成なのでサイズは不明
            },
            VfsDirEntry {
                name: String::from("tasks"),
                kind: VfsNodeKind::File,
                size: 0,
            },
        ];

        Ok(entries)
    }

    fn create_file(&self, _path: &str, _data: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn delete_file(&self, _path: &str) -> Result<(), VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn create_dir(&self, _path: &str) -> Result<(), VfsError> {
        Err(VfsError::ReadOnly)
    }

    fn delete_dir(&self, _path: &str) -> Result<(), VfsError> {
        Err(VfsError::ReadOnly)
    }
}

// =================================================================
// ファイル内容の生成
// =================================================================

/// メモリ情報を JSON 形式で生成する
fn generate_meminfo() -> Vec<u8> {
    use crate::memory::FRAME_ALLOCATOR;
    use crate::allocator;
    use crate::scheduler;

    // メモリ情報を取得
    let fa = FRAME_ALLOCATOR.lock();
    let total = fa.total_frames();
    let allocated = fa.allocated_count();
    let free = fa.free_frames();
    let invalid_deallocs = fa.invalid_dealloc_count();
    drop(fa); // ロックを早めに解放
    let processes = scheduler::process_mem_list();
    let heap_start = allocator::heap_start();
    let heap_size = allocator::heap_size();
    let heap_source = if allocator::heap_from_conventional() {
        "conventional"
    } else {
        "bss_fallback"
    };

    // JSON 形式で書き込む
    let mut buf = Vec::with_capacity(256);
    let mut writer = VecWriter::new(&mut buf);
    let _ = write!(
        writer,
        "{{\"total_frames\":{},\"allocated_frames\":{},\"free_frames\":{},\"free_kib\":{},\"heap_start\":{},\"heap_size\":{},\"heap_source\":\"{}\",\"processes\":[",
        total,
        allocated,
        free,
        free * 4,
        heap_start,
        heap_size,
        heap_source
    );
    for (i, p) in processes.iter().enumerate() {
        if i != 0 {
            let _ = write!(writer, ",");
        }
        let type_str = if p.is_user_process { "user" } else { "kernel" };
        let _ = write!(writer, "{{\"id\":{},\"type\":\"", p.id);
        let _ = writer.write_str(type_str);
        let _ = write!(writer, "\",\"name\":\"");
        let _ = write_json_string(&mut writer, p.name.as_str());
        let _ = write!(writer, "\",\"user_frames\":{}}}", p.user_frames);
    }
    let _ = write!(writer, "],\"invalid_deallocs\":{}}}\n", invalid_deallocs);

    buf
}

/// タスク一覧を JSON 形式で生成する
fn generate_tasks() -> Vec<u8> {
    use crate::scheduler::{self, TaskState};

    // タスク一覧を取得
    let tasks = scheduler::task_list();

    // JSON 形式で書き込む
    let mut buf = Vec::with_capacity(512);
    let mut writer = VecWriter::new(&mut buf);

    let _ = write!(writer, "{{\"tasks\":[");
    for (i, t) in tasks.iter().enumerate() {
        let state_str = match t.state {
            TaskState::Ready => "Ready",
            TaskState::Running => "Running",
            TaskState::Sleeping(_) => "Sleeping",
            TaskState::Finished => "Finished",
        };
        let type_str = if t.is_user_process { "user" } else { "kernel" };
        if i != 0 {
            let _ = write!(writer, ",");
        }
        let _ = write!(writer, "{{\"id\":{},\"state\":\"", t.id);
        let _ = writer.write_str(state_str);
        let _ = write!(writer, "\",\"type\":\"");
        let _ = writer.write_str(type_str);
        let _ = write!(writer, "\",\"name\":\"");
        let _ = write_json_string(&mut writer, t.name.as_str());
        let _ = write!(writer, "\"}}");
    }
    let _ = write!(writer, "]}}\n");

    buf
}

// =================================================================
// ユーティリティ
// =================================================================

/// Vec<u8> に書き込むための Write 実装
struct VecWriter<'a> {
    buf: &'a mut Vec<u8>,
}

impl<'a> VecWriter<'a> {
    fn new(buf: &'a mut Vec<u8>) -> Self {
        Self { buf }
    }
}

impl<'a> core::fmt::Write for VecWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.buf.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// JSON 文字列用のエスケープ付き書き込み
fn write_json_string(writer: &mut VecWriter<'_>, s: &str) -> core::fmt::Result {
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        match ch {
            '\\' => {
                let _ = writer.write_str("\\\\");
            }
            '"' => {
                let _ = writer.write_str("\\\"");
            }
            '\n' => {
                let _ = writer.write_str("\\n");
            }
            '\r' => {
                let _ = writer.write_str("\\r");
            }
            '\t' => {
                let _ = writer.write_str("\\t");
            }
            _ => {
                let encoded = ch.encode_utf8(&mut buf);
                let _ = writer.write_str(encoded);
            }
        }
    }
    Ok(())
}

// =================================================================
// syscall.rs から使用するためのヘルパー関数
// =================================================================

/// procfs のファイルを読み取る（syscall.rs からの後方互換性のため）
///
/// 対象ファイルが存在しない場合は FileNotFound を返す。
pub fn procfs_read(path: &str, buf: &mut [u8]) -> Result<usize, crate::user_ptr::SyscallError> {
    // パスを正規化
    let path = path.trim().trim_start_matches('/');
    let path = path.trim_start_matches("proc/");

    // データを生成
    let data = match path {
        "meminfo" => generate_meminfo(),
        "tasks" => generate_tasks(),
        _ => return Err(crate::user_ptr::SyscallError::FileNotFound),
    };

    // バッファにコピー
    let to_copy = core::cmp::min(data.len(), buf.len());
    buf[..to_copy].copy_from_slice(&data[..to_copy]);
    Ok(to_copy)
}

/// procfs のディレクトリ一覧を取得する（syscall.rs からの後方互換性のため）
pub fn procfs_list_dir(path: &str, buf: &mut [u8]) -> Result<usize, crate::user_ptr::SyscallError> {
    // パスを正規化
    let path = path.trim().trim_start_matches('/');

    if path != "proc" && path != "proc/" && !path.is_empty() {
        return Err(crate::user_ptr::SyscallError::FileNotFound);
    }

    let mut offset = 0;
    let entries: [&[u8]; 2] = [b"meminfo", b"tasks"];

    for name in entries {
        let needed = name.len() + 1;
        if offset + needed > buf.len() {
            break;
        }

        buf[offset..offset + name.len()].copy_from_slice(name);
        offset += name.len();
        buf[offset] = b'\n';
        offset += 1;
    }

    Ok(offset)
}
