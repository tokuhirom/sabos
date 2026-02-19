// syscall/handle.rs — ハンドル操作関連システムコール
//
// SYS_OPEN, SYS_HANDLE_READ/WRITE/CLOSE/STAT/SEEK/ENUM,
// SYS_OPENAT, SYS_HANDLE_CREATE_FILE/UNLINK/MKDIR,
// SYS_RESTRICT_RIGHTS, validate_entry_name, build_child_path

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use crate::user_ptr::SyscallError;
use super::{user_slice_from_args, user_ptr_from_arg};
use super::filesystem::list_dir_to_buffer;

/// SYS_OPEN: ファイルを開いて Handle を返す
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///   arg3 — Handle の書き込み先ポインタ（ユーザー空間）
///   arg4 — rights（READ/WRITE 等のビット）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_open(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let rights = arg4 as u32;

    // パスを取得
    let path_slice = user_slice_from_args(arg1, arg2)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // Handle の書き込み先
    let handle_ptr = user_ptr_from_arg::<Handle>(arg3)?;

    // VFS 経由で open（/proc への書き込みは VFS が ReadOnly を返す）
    let handle = open_path_to_handle(path, rights)?;
    handle_ptr.write(handle);
    Ok(0)
}

/// SYS_HANDLE_READ: Handle から読み取る
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_handle_read(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    // パイプの場合は WouldBlock で yield + retry してブロッキング読み取りにする。
    // パイプの writer がまだ生きているがデータがない状態では WouldBlock が返る。
    // ファイルの場合は即座に結果が返る（WouldBlock にはならない）。
    x86_64::instructions::interrupts::enable();
    loop {
        match crate::handle::read(&handle, buf) {
            Ok(n) => return Ok(n as u64),
            Err(SyscallError::WouldBlock) => {
                // パイプにデータがまだない → yield して再試行
                crate::scheduler::yield_now();
            }
            Err(e) => return Err(e),
        }
    }
}

/// SYS_HANDLE_WRITE: Handle に書き込む
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_handle_write(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_slice();

    let n = crate::handle::write(&handle, buf)?;
    Ok(n as u64)
}

/// SYS_HANDLE_CLOSE: Handle を閉じる
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_handle_close(arg1: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    crate::handle::close(&handle)?;
    Ok(0)
}

/// SYS_HANDLE_STAT: Handle のメタデータを取得する
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///   arg2 — HandleStat の書き込み先ポインタ（ユーザー空間）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_handle_stat(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleStat};

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let stat_ptr = user_ptr_from_arg::<HandleStat>(arg2)?;

    let stat = crate::handle::stat(&handle)?;
    stat_ptr.write(stat);
    Ok(0)
}

/// SYS_HANDLE_SEEK: Handle のファイルポジションを変更する
///
/// 引数:
///   arg1 — Handle のポインタ（ユーザー空間）
///   arg2 — offset（i64 として解釈、SEEK_CUR/SEEK_END で負の値あり）
///   arg3 — whence（0=SEEK_SET, 1=SEEK_CUR, 2=SEEK_END）
///
/// 戻り値:
///   新しいポジション（成功時）
///   負の値（エラー時）
pub(crate) fn sys_handle_seek(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let offset = arg2 as i64;
    let whence = arg3;

    let new_pos = crate::handle::seek(&handle, offset, whence)?;
    Ok(new_pos)
}

/// SYS_OPENAT: ディレクトリハンドルからの相対パスでファイルを開く
///
/// Capability-based security の核心となるシステムコール。
/// ディレクトリハンドルが持つ権限の範囲内でのみファイルを開ける。
///
/// 引数:
///   arg1 — ディレクトリハンドルへのポインタ（ユーザー空間）
///   arg2 — 相対パスのポインタ（ユーザー空間、絶対パス禁止）
///   arg3 — パスの長さ
///   arg4 — 下位32ビット: 新しいハンドルの書き込み先ポインタ
///          上位32ビット: 要求する権限（親の権限以下に制限される）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - dir_handle が LOOKUP 権限を持つか確認
///   - path が "/" で始まっていたらエラー（絶対パス禁止）
///   - path に ".." が含まれていたらエラー（パストラバーサル防止）
///   - 新しいハンドルの権限 = requested_rights & dir_handle.rights
pub(crate) fn sys_openat(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_LOOKUP};

    // 注: 現在の実装では arg4 全体を new_handle_ptr として扱う。
    // rights は open_path_to_handle() 内で種別に応じてデフォルトを設定する。
    let new_handle_ptr_raw = arg4;

    // ディレクトリハンドルを取得
    let dir_handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let dir_handle = dir_handle_ptr.read();

    // 相対パスを取得
    let path_slice = user_slice_from_args(arg2, arg3)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 新しいハンドルの書き込み先
    let new_handle_ptr = user_ptr_from_arg::<Handle>(new_handle_ptr_raw)?;

    // ディレクトリハンドルの権限チェック（LOOKUP 権限が必要）
    crate::handle::check_rights(&dir_handle, HANDLE_RIGHT_LOOKUP)?;

    // ディレクトリハンドルの種類チェック
    let kind = crate::handle::get_kind(&dir_handle)?;
    if kind != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    // 相対パスの検証（絶対パスと ".." を禁止）
    crate::vfs::validate_relative_path(path).map_err(|e| {
        match e {
            crate::vfs::VfsError::PathTraversal => SyscallError::PathTraversal,
            crate::vfs::VfsError::InvalidPath => SyscallError::InvalidArgument,
            _ => SyscallError::Other,
        }
    })?;

    // ディレクトリのパスを取得
    let dir_path = crate::handle::get_path(&dir_handle)?;

    // フルパスを構築
    let full_path = if dir_path.is_empty() || dir_path == "/" {
        format!("/{}", path)
    } else {
        format!("{}/{}", dir_path, path)
    };

    // 親ディレクトリハンドルの権限を引き継ぐ（Capability-based security の原則）
    let parent_rights = crate::handle::get_rights(&dir_handle)?;

    // ファイル/ディレクトリを開く（親の権限を引き継ぐ）
    let handle = open_path_to_handle(&full_path, parent_rights)?;
    new_handle_ptr.write(handle);

    Ok(0)
}

/// SYS_HANDLE_ENUM: ディレクトリハンドルの内容を一覧
///
/// 引数:
///   arg1 — ディレクトリハンドルのポインタ（ユーザー空間）
///   arg2 — バッファのポインタ（ユーザー空間、エントリ名を改行区切りで書き込む）
///   arg3 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_handle_enum(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_ENUM};

    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    crate::handle::check_rights(&handle, HANDLE_RIGHT_ENUM)?;
    if crate::handle::get_kind(&handle)? != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    let path = crate::handle::get_path(&handle)?;
    let written = list_dir_to_buffer(&path, buf)?;
    Ok(written as u64)
}

/// SYS_HANDLE_CREATE_FILE: ディレクトリハンドル内にファイルを作成し、書き込み可能なハンドルを返す
///
/// 引数:
///   arg1 — ディレクトリハンドルのポインタ（ユーザー空間）
///   arg2 — ファイル名のポインタ（ユーザー空間）
///   arg3 — ファイル名の長さ
///   arg4 — 出力ハンドルの書き込み先ポインタ（ユーザー空間）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - ディレクトリハンドルに CREATE 権限が必要
///   - ファイル名に ".." や "/" は禁止
pub(crate) fn sys_handle_create_file(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_CREATE, HANDLE_RIGHTS_FILE_RW,
                        create_handle_with_path};

    // ディレクトリハンドルを取得
    let dir_handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let dir_handle = dir_handle_ptr.read();

    // ファイル名を取得
    let name_slice = user_slice_from_args(arg2, arg3)?;
    let name = name_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 出力ハンドルの書き込み先
    let out_handle_ptr = user_ptr_from_arg::<Handle>(arg4)?;

    // 権限チェック（CREATE 権限が必要）
    crate::handle::check_rights(&dir_handle, HANDLE_RIGHT_CREATE)?;

    // ディレクトリハンドルの種類チェック
    if crate::handle::get_kind(&dir_handle)? != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    // ファイル名の検証（".." や "/" を禁止）
    validate_entry_name(name)?;

    // ディレクトリのパスを取得してフルパスを構築
    let dir_path = crate::handle::get_path(&dir_handle)?;
    let full_path = build_child_path(&dir_path, name);

    // VFS 経由でファイルを作成（/proc は VFS が ReadOnly を返す）
    let _ = crate::vfs::delete_file(&full_path); // 既存ファイルの削除（なくてもOK）
    crate::vfs::create_file(&full_path, &[]).map_err(crate::vfs::vfs_error_to_syscall)?;

    // RW 権限付きハンドルを作成して返す
    let handle = create_handle_with_path(Vec::new(), HANDLE_RIGHTS_FILE_RW, String::from(&*full_path));
    out_handle_ptr.write(handle);

    Ok(0)
}

/// SYS_HANDLE_UNLINK: ディレクトリハンドル内のファイルまたは空ディレクトリを削除
///
/// 引数:
///   arg1 — ディレクトリハンドルのポインタ（ユーザー空間）
///   arg2 — ファイル名のポインタ（ユーザー空間）
///   arg3 — ファイル名の長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - ディレクトリハンドルに DELETE 権限が必要
///   - ファイル名に ".." や "/" は禁止
pub(crate) fn sys_handle_unlink(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_DELETE};

    // ディレクトリハンドルを取得
    let dir_handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let dir_handle = dir_handle_ptr.read();

    // ファイル名を取得
    let name_slice = user_slice_from_args(arg2, arg3)?;
    let name = name_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 権限チェック（DELETE 権限が必要）
    crate::handle::check_rights(&dir_handle, HANDLE_RIGHT_DELETE)?;

    // ディレクトリハンドルの種類チェック
    if crate::handle::get_kind(&dir_handle)? != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    // ファイル名の検証
    validate_entry_name(name)?;

    // フルパスを構築
    let dir_path = crate::handle::get_path(&dir_handle)?;
    let full_path = build_child_path(&dir_path, name);

    // VFS 経由で削除（ファイルを先に試し、失敗したらディレクトリとして削除）
    // /proc は VFS が ReadOnly を返す
    if crate::vfs::delete_file(&full_path).is_err() {
        crate::vfs::delete_dir(&full_path).map_err(crate::vfs::vfs_error_to_syscall)?;
    }

    Ok(0)
}

/// SYS_HANDLE_MKDIR: ディレクトリハンドル内にサブディレクトリを作成
///
/// 引数:
///   arg1 — ディレクトリハンドルのポインタ（ユーザー空間）
///   arg2 — ディレクトリ名のポインタ（ユーザー空間）
///   arg3 — ディレクトリ名の長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - ディレクトリハンドルに CREATE 権限が必要
///   - ディレクトリ名に ".." や "/" は禁止
pub(crate) fn sys_handle_mkdir(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::{Handle, HandleKind, HANDLE_RIGHT_CREATE};

    // ディレクトリハンドルを取得
    let dir_handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let dir_handle = dir_handle_ptr.read();

    // ディレクトリ名を取得
    let name_slice = user_slice_from_args(arg2, arg3)?;
    let name = name_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // 権限チェック（CREATE 権限が必要）
    crate::handle::check_rights(&dir_handle, HANDLE_RIGHT_CREATE)?;

    // ディレクトリハンドルの種類チェック
    if crate::handle::get_kind(&dir_handle)? != HandleKind::Directory {
        return Err(SyscallError::InvalidArgument);
    }

    // ディレクトリ名の検証
    validate_entry_name(name)?;

    // フルパスを構築
    let dir_path = crate::handle::get_path(&dir_handle)?;
    let full_path = build_child_path(&dir_path, name);

    // VFS 経由でディレクトリを作成（/proc は VFS が ReadOnly を返す）
    crate::vfs::create_dir(&full_path).map_err(crate::vfs::vfs_error_to_syscall)?;

    Ok(0)
}

/// ファイル/ディレクトリ名の検証ヘルパー
///
/// ".." や "/" を含む名前はパストラバーサル攻撃の原因になるため拒否する。
/// 空の名前も拒否する。
pub(crate) fn validate_entry_name(name: &str) -> Result<(), SyscallError> {
    if name.is_empty() {
        return Err(SyscallError::InvalidArgument);
    }
    if name.contains('/') || name.contains('\\') {
        return Err(SyscallError::InvalidArgument);
    }
    if name == ".." || name == "." {
        return Err(SyscallError::PathTraversal);
    }
    Ok(())
}

/// 親ディレクトリのパスと子の名前からフルパスを構築するヘルパー
pub(crate) fn build_child_path(dir_path: &str, name: &str) -> String {
    if dir_path.is_empty() || dir_path == "/" {
        format!("/{}", name)
    } else {
        format!("{}/{}", dir_path, name)
    }
}

/// SYS_RESTRICT_RIGHTS: ハンドルの権限を縮小する
///
/// Capability-based security の重要な操作。
/// 権限は縮小のみ可能で、拡大はできない。
///
/// 引数:
///   arg1 — 元のハンドルへのポインタ（ユーザー空間）
///   arg2 — 新しい権限ビット（縮小のみ可）
///   arg3 — 新しいハンドルの書き込み先ポインタ（ユーザー空間）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
///
/// セキュリティ:
///   - new_rights は元のハンドルの rights の部分集合でなければならない
///   - 権限の拡大を試みた場合は PermissionDenied エラー
pub(crate) fn sys_restrict_rights(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    use crate::handle::Handle;

    let new_rights = arg2 as u32;

    // 元のハンドルを取得
    let handle_ptr = user_ptr_from_arg::<Handle>(arg1)?;
    let handle = handle_ptr.read();

    // 新しいハンドルの書き込み先
    let new_handle_ptr = user_ptr_from_arg::<Handle>(arg3)?;

    // 権限を縮小した新しいハンドルを作成
    let new_handle = crate::handle::restrict_rights(&handle, new_rights)?;
    new_handle_ptr.write(new_handle);

    Ok(0)
}

/// パスから Handle を作成する
pub(crate) fn open_path_to_handle(path: &str, rights: u32) -> Result<crate::handle::Handle, SyscallError> {
    use crate::handle::{
        create_directory_handle, create_handle_with_path, HANDLE_RIGHT_ENUM, HANDLE_RIGHT_LOOKUP,
        HANDLE_RIGHT_READ, HANDLE_RIGHT_WRITE, HANDLE_RIGHTS_DIRECTORY_READ, HANDLE_RIGHTS_FILE_READ,
        HANDLE_RIGHTS_FILE_RW,
    };

    // ルートディレクトリは特別扱い
    if path == "/" || path.is_empty() {
        let dir_rights = if rights == 0 { HANDLE_RIGHTS_DIRECTORY_READ } else { rights };
        if (dir_rights & (HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP)) == 0 {
            return Err(SyscallError::InvalidArgument);
        }
        return Ok(create_directory_handle(String::from("/"), dir_rights));
    }

    // WRITE 権限付きの場合: 新規ファイル作成も許可する
    let has_write = (rights & HANDLE_RIGHT_WRITE) != 0;

    // パスを正規化してマウント先の FS を判定する
    let normalized = crate::vfs::normalize_path(path)
        .map_err(crate::vfs::vfs_error_to_syscall)?;
    let is_procfs = normalized.starts_with("/proc");

    // procfs 配下に WRITE 権限でオープンは不可
    if has_write && is_procfs {
        return Err(SyscallError::ReadOnly);
    }

    // VFS 経由でファイル/ディレクトリを開く
    match crate::vfs::open(path) {
        Ok(node) => {
            let kind = node.kind();
            match kind {
                crate::vfs::VfsNodeKind::Directory => {
                    // ディレクトリは書き込みオープン不可
                    if has_write {
                        return Err(SyscallError::NotSupported);
                    }
                    let dir_rights = if rights == 0 { HANDLE_RIGHTS_DIRECTORY_READ } else { rights };
                    if (dir_rights & (HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP)) == 0 {
                        return Err(SyscallError::InvalidArgument);
                    }
                    return Ok(create_directory_handle(String::from(path), dir_rights));
                }
                crate::vfs::VfsNodeKind::File => {
                    // ファイルデータを読み取る
                    let data = crate::vfs::read_file(path)
                        .map_err(crate::vfs::vfs_error_to_syscall)?;
                    let file_rights = if rights == 0 {
                        if has_write { HANDLE_RIGHTS_FILE_RW } else { HANDLE_RIGHTS_FILE_READ }
                    } else {
                        rights
                    };
                    if !has_write && (file_rights & HANDLE_RIGHT_READ) == 0 {
                        return Err(SyscallError::InvalidArgument);
                    }
                    return Ok(create_handle_with_path(data, file_rights, String::from(path)));
                }
            }
        }
        Err(crate::vfs::VfsError::NotAFile) => {
            // ディレクトリとして扱う（VFS がディレクトリに open() して NotAFile が返された場合）
            if has_write {
                return Err(SyscallError::NotSupported);
            }
            let dir_rights = if rights == 0 { HANDLE_RIGHTS_DIRECTORY_READ } else { rights };
            if (dir_rights & (HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP)) == 0 {
                return Err(SyscallError::InvalidArgument);
            }
            return Ok(create_directory_handle(String::from(path), dir_rights));
        }
        Err(crate::vfs::VfsError::NotFound) => {
            // ファイルが見つからない場合
            if has_write {
                // WRITE 権限付きなら新規ファイルとして空データでハンドル作成
                let file_rights = if rights == 0 { HANDLE_RIGHTS_FILE_RW } else { rights };
                Ok(create_handle_with_path(Vec::new(), file_rights, String::from(path)))
            } else {
                // WRITE なしならファイル未発見エラー
                Err(SyscallError::FileNotFound)
            }
        }
        Err(e) => Err(crate::vfs::vfs_error_to_syscall(e)),
    }
}
