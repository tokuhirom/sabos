// syscall/filesystem.rs — ファイルシステム関連システムコール
//
// SYS_FILE_DELETE/WRITE, SYS_DIR_CREATE/REMOVE/LIST,
// SYS_FS_STAT, list_dir_to_buffer, open_path_to_handle

use alloc::format;
use crate::user_ptr::SyscallError;
use super::user_slice_from_args;

/// SYS_FILE_DELETE: ファイルを削除
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_file_delete(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // VFS 経由でファイルを削除（/proc は VFS が ReadOnly を返す）
    crate::vfs::delete_file(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// SYS_FILE_WRITE: ファイルを作成/上書き
///
/// 既にファイルが存在する場合は削除してから作成する。
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///   arg3 — データのポインタ（ユーザー空間）
///   arg4 — データの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_file_write(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // データを取得
    let data_slice = user_slice_from_args(arg3, arg4)?;
    let data = data_slice.as_slice();

    // VFS 経由でファイルを作成/上書き（/proc は VFS が ReadOnly を返す）
    // create_file は既存ファイルがあるとエラーになるので、先に削除を試みる
    let _ = crate::vfs::delete_file(path); // 既存ファイルの削除（なくてもOK）
    crate::vfs::create_file(path, data).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// SYS_DIR_CREATE: ディレクトリを作成
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_dir_create(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // VFS 経由でディレクトリを作成（/proc は VFS が ReadOnly を返す）
    crate::vfs::create_dir(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// SYS_DIR_REMOVE: ディレクトリを削除
///
/// 空のディレクトリのみ削除可能。
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_dir_remove(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // VFS 経由でディレクトリを削除（/proc は VFS が ReadOnly を返す）
    crate::vfs::delete_dir(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// SYS_FS_STAT: ファイルシステム統計情報を取得
///
/// JSON 形式でファイルシステムの使用状況をバッファに書き込む。
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_fs_stat(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    let mut fat32 = crate::fat32::Fat32::new().map_err(|_| SyscallError::Other)?;
    let total_clusters = fat32.total_clusters();
    let free_clusters = fat32.free_clusters().map_err(|_| SyscallError::Other)?;
    let cluster_bytes = fat32.cluster_bytes() as u64;
    let total_bytes = total_clusters as u64 * cluster_bytes;
    let free_bytes = free_clusters as u64 * cluster_bytes;
    let used_bytes = total_bytes.saturating_sub(free_bytes);

    // JSON 形式で書き込む
    let json = format!(
        "{{\"fs\":\"fat32\",\"total_bytes\":{},\"used_bytes\":{},\"free_bytes\":{},\"cluster_bytes\":{},\"total_clusters\":{},\"free_clusters\":{}}}",
        total_bytes, used_bytes, free_bytes, cluster_bytes, total_clusters, free_clusters
    );

    let json_bytes = json.as_bytes();
    if json_bytes.len() > buf.len() {
        return Err(SyscallError::BufferOverflow);
    }
    buf[..json_bytes.len()].copy_from_slice(json_bytes);

    Ok(json_bytes.len() as u64)
}

/// SYS_DIR_LIST: ディレクトリの内容を一覧
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）"/" ならルート
///   arg2 — パスの長さ
///   arg3 — バッファのポインタ（ユーザー空間、エントリ名を改行区切りで書き込む）
///   arg4 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式:
///   ファイル名を改行区切りで出力。ディレクトリには末尾に "/" を付ける。
pub(crate) fn sys_dir_list(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // バッファを取得
    let buf_slice = user_slice_from_args(arg3, arg4)?;
    let buf = buf_slice.as_mut_slice();

    let written = list_dir_to_buffer(path, buf)?;
    Ok(written as u64)
}

/// ディレクトリ一覧をバッファに書き込む（共通ヘルパー）
pub(crate) fn list_dir_to_buffer(path: &str, buf: &mut [u8]) -> Result<usize, SyscallError> {
    // VFS 経由でディレクトリ一覧を取得
    // VFS が自動的に /proc へのルーティングやマウントポイントの追加を行う
    let entries = crate::vfs::list_dir(path).map_err(crate::vfs::vfs_error_to_syscall)?;

    let mut offset = 0;
    for entry in entries {
        let name = &entry.name;
        let is_dir = entry.kind == crate::vfs::VfsNodeKind::Directory;

        // 名前のバイト数 + 改行 (+ "/" for directories)
        let needed = name.len() + if is_dir { 2 } else { 1 };
        if offset + needed > buf.len() {
            break;  // バッファがいっぱい
        }

        // 名前をコピー
        buf[offset..offset + name.len()].copy_from_slice(name.as_bytes());
        offset += name.len();

        // ディレクトリなら "/" を追加
        if is_dir {
            buf[offset] = b'/';
            offset += 1;
        }

        // 改行を追加
        buf[offset] = b'\n';
        offset += 1;
    }

    Ok(offset)
}

/// selftest 用のテストエントリポイント
///
/// list_dir_to_buffer のテスト用ラッパー。
/// shell.rs の selftest から呼ぶため pub にしている。
pub fn list_dir_to_buffer_for_test(path: &str, buf: &mut [u8]) -> Result<usize, SyscallError> {
    list_dir_to_buffer(path, buf)
}
