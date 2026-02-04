// vfs.rs — 仮想ファイルシステム (VFS) 抽象層
//
// SABOS の VFS は Capability-based security を採用し、
// セキュア by Design を実現する。
//
// ## 設計原則
//
// 1. **統一インターフェース**: FAT16、procfs など異なるファイルシステムを
//    同じ trait で扱う
//
// 2. **パストラバーサル防止**: パスの正規化で ".." を構造的に禁止し、
//    サンドボックス脱出を防ぐ
//
// 3. **Capability-based**: ハンドルに権限を埋め込み、最小権限の原則を実現
//
// 4. **型安全**: Rust の型システムで不正なアクセスをコンパイル時に防止

// 将来の VFS 統合で使用するため、dead_code 警告を抑制
#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// VFS で発生するエラー
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsError {
    /// ファイルが見つからない
    NotFound,
    /// ディレクトリではない（ファイルに対してディレクトリ操作を試みた）
    NotADirectory,
    /// ファイルではない（ディレクトリに対してファイル操作を試みた）
    NotAFile,
    /// 書き込み禁止（読み取り専用ファイルシステム）
    ReadOnly,
    /// 権限不足
    PermissionDenied,
    /// パストラバーサル試行（".." を含むパス）
    PathTraversal,
    /// 不正なパス形式
    InvalidPath,
    /// ファイルが既に存在する
    AlreadyExists,
    /// ディスク容量不足
    NoSpace,
    /// I/O エラー
    IoError,
    /// 未対応の操作
    NotSupported,
}

/// VFS ノードの種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsNodeKind {
    /// 通常のファイル
    File,
    /// ディレクトリ
    Directory,
}

/// ディレクトリエントリの情報
///
/// list_dir() で返されるエントリの情報を表す。
#[derive(Debug, Clone)]
pub struct VfsDirEntry {
    /// エントリ名（パスではなくファイル名のみ）
    pub name: String,
    /// エントリの種類（ファイル or ディレクトリ）
    pub kind: VfsNodeKind,
    /// ファイルサイズ（ディレクトリの場合は 0）
    pub size: usize,
}

/// VFS ノード（ファイルまたはディレクトリ）
///
/// ファイルシステム上の個々のエントリを表す trait。
/// 読み取り・書き込み・メタデータ取得などの操作を提供する。
pub trait VfsNode: Send + Sync {
    /// ノードの種類を返す
    fn kind(&self) -> VfsNodeKind;

    /// ノードのサイズを返す（ファイルの場合はバイト数、ディレクトリは 0）
    fn size(&self) -> usize;

    /// 指定オフセットからデータを読み取る
    ///
    /// # 引数
    /// - `offset`: 読み取り開始位置（バイト）
    /// - `buf`: 読み取り先バッファ
    ///
    /// # 戻り値
    /// 実際に読み取ったバイト数。EOF に達した場合は 0。
    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<usize, VfsError>;

    /// 指定オフセットにデータを書き込む
    ///
    /// # 引数
    /// - `offset`: 書き込み開始位置（バイト）
    /// - `data`: 書き込むデータ
    ///
    /// # 戻り値
    /// 実際に書き込んだバイト数。
    fn write(&self, offset: usize, data: &[u8]) -> Result<usize, VfsError>;
}

/// ファイルシステムの抽象インターフェース
///
/// FAT16、procfs など各種ファイルシステムがこの trait を実装する。
pub trait FileSystem: Send + Sync {
    /// ファイルシステムの名前を返す（デバッグ用）
    fn name(&self) -> &str;

    /// パスで指定されたファイルを開く
    ///
    /// # 引数
    /// - `path`: ファイルシステム内の相対パス（"/" で始まらない）
    ///
    /// # 戻り値
    /// VfsNode trait オブジェクト
    fn open(&self, path: &str) -> Result<Box<dyn VfsNode>, VfsError>;

    /// ディレクトリ内のエントリ一覧を取得する
    ///
    /// # 引数
    /// - `path`: ディレクトリのパス（"" または "/" でルート）
    ///
    /// # 戻り値
    /// ディレクトリエントリのベクタ
    fn list_dir(&self, path: &str) -> Result<Vec<VfsDirEntry>, VfsError>;

    /// ファイルを作成する
    ///
    /// # 引数
    /// - `path`: 作成するファイルのパス
    /// - `data`: 初期データ
    ///
    /// # 戻り値
    /// 成功時は Ok(())
    fn create_file(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let _ = (path, data);
        Err(VfsError::ReadOnly)
    }

    /// ファイルを削除する
    ///
    /// # 引数
    /// - `path`: 削除するファイルのパス
    ///
    /// # 戻り値
    /// 成功時は Ok(())
    fn delete_file(&self, path: &str) -> Result<(), VfsError> {
        let _ = path;
        Err(VfsError::ReadOnly)
    }

    /// ディレクトリを作成する
    fn create_dir(&self, path: &str) -> Result<(), VfsError> {
        let _ = path;
        Err(VfsError::ReadOnly)
    }

    /// ディレクトリを削除する
    fn delete_dir(&self, path: &str) -> Result<(), VfsError> {
        let _ = path;
        Err(VfsError::ReadOnly)
    }
}

// =================================================================
// パス正規化とトラバーサル防止
// =================================================================

/// パスを正規化し、トラバーサルを防止する
///
/// # 処理内容
/// - 先頭の "/" を正規化
/// - 連続する "/" を 1 つに
/// - "." を除去
/// - ".." を禁止（エラーを返す）
/// - 末尾の "/" を除去
///
/// # 引数
/// - `path`: 正規化するパス
///
/// # 戻り値
/// 正規化されたパス。先頭に "/" が付く。
///
/// # エラー
/// - `VfsError::PathTraversal`: ".." が含まれている場合
/// - `VfsError::InvalidPath`: 空のパスの場合
///
/// # セキュリティ
/// この関数は ".." を含むパスを拒否することで、
/// サンドボックス脱出（パストラバーサル攻撃）を構造的に防止する。
pub fn normalize_path(path: &str) -> Result<String, VfsError> {
    // 空文字列は "/" として扱う
    if path.is_empty() {
        return Ok(String::from("/"));
    }

    let mut result = String::with_capacity(path.len() + 1);
    result.push('/');

    // "/" で分割してコンポーネントを処理
    for component in path.split('/') {
        // 空のコンポーネントはスキップ（連続する "/" の結果）
        if component.is_empty() {
            continue;
        }

        // "." は現在のディレクトリを意味するのでスキップ
        if component == "." {
            continue;
        }

        // ".." はパストラバーサル攻撃の可能性があるので拒否
        // SABOS では ".." を一切許可しない（セキュア by Design）
        if component == ".." {
            return Err(VfsError::PathTraversal);
        }

        // 有効なコンポーネントを追加
        if result.len() > 1 {
            result.push('/');
        }
        result.push_str(component);
    }

    Ok(result)
}

/// パスを相対パスとして検証する
///
/// openat() 相当の操作で使用する。
/// 絶対パス（"/" で始まる）や ".." を含むパスを拒否する。
///
/// # 引数
/// - `path`: 検証するパス
///
/// # 戻り値
/// 検証済みの相対パス（そのまま返す）
///
/// # エラー
/// - `VfsError::InvalidPath`: 絶対パスの場合
/// - `VfsError::PathTraversal`: ".." が含まれている場合
pub fn validate_relative_path(path: &str) -> Result<&str, VfsError> {
    // 空のパスはエラー
    if path.is_empty() {
        return Err(VfsError::InvalidPath);
    }

    // "/" で始まる絶対パスは拒否
    if path.starts_with('/') {
        return Err(VfsError::InvalidPath);
    }

    // ".." を含むパスは拒否（パストラバーサル防止）
    for component in path.split('/') {
        if component == ".." {
            return Err(VfsError::PathTraversal);
        }
    }

    Ok(path)
}

// =================================================================
// テスト
// =================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path_basic() {
        assert_eq!(normalize_path("").unwrap(), "/");
        assert_eq!(normalize_path("/").unwrap(), "/");
        assert_eq!(normalize_path("/foo").unwrap(), "/foo");
        assert_eq!(normalize_path("/foo/bar").unwrap(), "/foo/bar");
    }

    #[test]
    fn test_normalize_path_dots() {
        assert_eq!(normalize_path("/./foo").unwrap(), "/foo");
        assert_eq!(normalize_path("/foo/./bar").unwrap(), "/foo/bar");
        assert_eq!(normalize_path("/foo/.").unwrap(), "/foo");
    }

    #[test]
    fn test_normalize_path_slashes() {
        assert_eq!(normalize_path("//foo").unwrap(), "/foo");
        assert_eq!(normalize_path("/foo//bar").unwrap(), "/foo/bar");
        assert_eq!(normalize_path("/foo/").unwrap(), "/foo");
        assert_eq!(normalize_path("/foo//").unwrap(), "/foo");
    }

    #[test]
    fn test_normalize_path_traversal() {
        assert_eq!(normalize_path("/..").unwrap_err(), VfsError::PathTraversal);
        assert_eq!(normalize_path("/../etc").unwrap_err(), VfsError::PathTraversal);
        assert_eq!(normalize_path("/foo/../bar").unwrap_err(), VfsError::PathTraversal);
        assert_eq!(normalize_path("/foo/bar/..").unwrap_err(), VfsError::PathTraversal);
    }

    #[test]
    fn test_validate_relative_path() {
        assert!(validate_relative_path("foo").is_ok());
        assert!(validate_relative_path("foo/bar").is_ok());
        assert!(validate_relative_path("foo/bar.txt").is_ok());

        assert_eq!(validate_relative_path("").unwrap_err(), VfsError::InvalidPath);
        assert_eq!(validate_relative_path("/foo").unwrap_err(), VfsError::InvalidPath);
        assert_eq!(validate_relative_path("../foo").unwrap_err(), VfsError::PathTraversal);
        assert_eq!(validate_relative_path("foo/../bar").unwrap_err(), VfsError::PathTraversal);
    }
}
