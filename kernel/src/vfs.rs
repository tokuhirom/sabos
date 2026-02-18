// vfs.rs — 仮想ファイルシステム (VFS) 抽象層
//
// SABOS の VFS は Capability-based security を採用し、
// セキュア by Design を実現する。
//
// ## 設計原則
//
// 1. **統一インターフェース**: FAT32、procfs など異なるファイルシステムを
//    同じ trait で扱う
//
// 2. **パストラバーサル防止**: パスの正規化で ".." を構造的に禁止し、
//    サンドボックス脱出を防ぐ
//
// 3. **Capability-based**: ハンドルに権限を埋め込み、最小権限の原則を実現
//
// 4. **型安全**: Rust の型システムで不正なアクセスをコンパイル時に防止
//
// ## VFS マネージャ
//
// VfsManager はマウントテーブル（BTreeMap）を保持し、パスの最長一致で
// 適切なファイルシステムにルーティングする。
// 例: "/proc/meminfo" → "/proc" マウントにマッチ → ProcFs に "meminfo" として委譲
//
// デッドロック対策:
// MountEntry はファクトリ関数（Box<dyn Fn() -> Box<dyn FileSystem>>）を保持する。
// resolve() ではファクトリ関数を取得後すぐに VFS の Mutex を解放し、
// その後 FileSystem インスタンスを生成してメソッドを呼ぶ。

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use lazy_static::lazy_static;
use spin::Mutex;
use crate::user_ptr::SyscallError;

/// VFS で発生するエラー
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // 一部のバリアントは VFS マネージャの将来の拡張で使用予定
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
    #[allow(dead_code)] // ハンドルシステムが write-back 方式のため現在は未使用
    fn write(&self, offset: usize, data: &[u8]) -> Result<usize, VfsError>;
}

/// ファイルシステムの抽象インターフェース
///
/// FAT32、procfs など各種ファイルシステムがこの trait を実装する。
pub trait FileSystem: Send + Sync {
    /// ファイルシステムの名前を返す（デバッグ用）
    #[allow(dead_code)]
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

    /// ファイルの全内容を一括読み取り（効率化用）
    ///
    /// デフォルト実装は open() → read() を繰り返すが、
    /// ファイルシステム固有の実装で二重コピーを避けることができる。
    fn read_file(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let node = self.open(path)?;
        let size = node.size();
        if size == 0 {
            let mut data = Vec::with_capacity(256);
            let mut buf = [0u8; 4096];
            let mut offset = 0;
            loop {
                let n = node.read(offset, &mut buf)?;
                if n == 0 { break; }
                data.extend_from_slice(&buf[..n]);
                offset += n;
            }
            return Ok(data);
        }
        let mut data = alloc::vec![0u8; size];
        let mut offset = 0;
        loop {
            let n = node.read(offset, &mut data[offset..])?;
            if n == 0 { break; }
            offset += n;
        }
        data.truncate(offset);
        Ok(data)
    }
}

// =================================================================
// VFS マネージャ（マウントテーブル）
// =================================================================

/// マウントエントリ
///
/// ファクトリ関数を保持し、resolve() 時に毎回 FileSystem インスタンスを生成する。
/// これにより VFS の Mutex を保持したまま FileSystem を操作する必要がなくなり、
/// デッドロックを回避できる。
struct MountEntry {
    /// マウントポイント（例: "/", "/proc"）
    mount_point: String,
    /// FileSystem のファクトリ関数
    factory: Box<dyn Fn() -> Box<dyn FileSystem> + Send + Sync>,
}

/// VFS マネージャ
///
/// マウントテーブルを管理し、パスに基づいて適切な FileSystem にルーティングする。
/// BTreeMap を使うことで最長一致を効率的に行える。
struct VfsManager {
    /// マウントポイント → MountEntry のマップ
    /// キーはマウントポイント（例: "/", "/proc"）
    mounts: BTreeMap<String, MountEntry>,
}

impl VfsManager {
    /// 新しい VfsManager を作成する
    fn new() -> Self {
        Self {
            mounts: BTreeMap::new(),
        }
    }

    /// ファイルシステムをマウントする
    ///
    /// # 引数
    /// - `mount_point`: マウントポイント（例: "/", "/proc"）
    /// - `factory`: FileSystem のファクトリ関数
    fn mount(
        &mut self,
        mount_point: &str,
        factory: Box<dyn Fn() -> Box<dyn FileSystem> + Send + Sync>,
    ) {
        let mp = String::from(mount_point);
        self.mounts.insert(mp.clone(), MountEntry {
            mount_point: mp,
            factory,
        });
    }

    /// パスを解決して (ファクトリ関数のクローン, 相対パス) を返す
    ///
    /// マウントテーブルの最長一致でファイルシステムを決定し、
    /// マウントポイントのプレフィックスを除去した相対パスを返す。
    ///
    /// # 引数
    /// - `normalized_path`: normalize_path() 済みの絶対パス
    ///
    /// # 戻り値
    /// (FileSystem インスタンス, マウントポイント除去後の相対パス)
    fn resolve(&self, normalized_path: &str) -> Result<(Box<dyn FileSystem>, String), VfsError> {
        // 最長一致: BTreeMap を逆順に走査して最初にマッチするものを見つける
        let mut best_match: Option<&MountEntry> = None;
        let mut best_len = 0;

        for (key, entry) in &self.mounts {
            if normalized_path == key.as_str()
                || normalized_path.starts_with(&format!("{}/", key))
                || key == "/"
            {
                if key.len() > best_len {
                    best_len = key.len();
                    best_match = Some(entry);
                }
            }
        }

        let entry = best_match.ok_or(VfsError::NotFound)?;

        // プレフィックスを除去して相対パスを生成
        let relative = if entry.mount_point == "/" {
            // ルートマウント: 先頭の "/" を除去
            &normalized_path[1..]
        } else if normalized_path == entry.mount_point {
            // マウントポイントそのもの: 空文字列（ルートディレクトリ）
            ""
        } else {
            // プレフィックスを除去（例: "/proc/meminfo" → "meminfo"）
            &normalized_path[entry.mount_point.len() + 1..]
        };

        // ファクトリ関数で FileSystem インスタンスを生成
        let fs = (entry.factory)();
        Ok((fs, String::from(relative)))
    }

    /// マウントポイントの一覧を返す（ルート "/" を除く）
    fn mount_points(&self) -> Vec<String> {
        self.mounts
            .keys()
            .filter(|k| k.as_str() != "/")
            .cloned()
            .collect()
    }
}

lazy_static! {
    /// グローバル VFS マネージャ
    static ref VFS: Mutex<VfsManager> = Mutex::new(VfsManager::new());
}

// =================================================================
// Public API
// =================================================================

/// VFS を初期化する
///
/// "/" に FAT32、"/proc" に ProcFs をマウントする。
/// virtio_blk::init() の後に呼び出すこと。
pub fn init() {
    let mut vfs = VFS.lock();
    vfs.mount("/", Box::new(|| {
        Box::new(crate::fat32::Fat32::new_fs())
    }));
    vfs.mount("/proc", Box::new(|| {
        Box::new(crate::procfs::ProcFs::new())
    }));

    // 2 台目の virtio-blk デバイスがあれば "/host" にマウントする。
    // QEMU で `-drive if=virtio,format=raw,file=fat:rw:hostfs/` を指定すると
    // ホストのビルドディレクトリが FAT32 として公開される。
    let dev_count = crate::virtio_blk::device_count();
    if dev_count >= 2 {
        vfs.mount("/host", Box::new(|| {
            Box::new(crate::fat32::Fat32::new_fs_with_index(1))
        }));
    }

    // virtio-9p デバイスがあれば "/9p" にマウントする。
    // QEMU の `-virtfs` オプションでホストのディレクトリをリアルタイム共有する。
    // FAT32 (hostfs.img) と違い、ホスト側の変更が即座にゲストに反映される。
    let has_9p = crate::virtio_9p::is_available();
    if has_9p {
        vfs.mount("/9p", Box::new(|| {
            Box::new(crate::virtio_9p::V9pFs::new())
        }));
    }

    // AHCI デバイスがあれば "/ahci" にマウントする。
    // 実機では onboard SATA ディスクが AHCI 経由で見える。
    // QEMU でも `-device ahci` + `-device ide-hd` でテスト可能。
    let ahci_count = crate::ahci::device_count();
    if ahci_count >= 1 {
        vfs.mount("/ahci", Box::new(|| {
            Box::new(crate::fat32::Fat32::new_fs_with_backend(
                crate::fat32::BlockBackend::Ahci(0),
            ))
        }));
    }

    // NVMe デバイスがあれば "/nvme" にマウントする。
    // 実機では PCIe 接続の NVMe SSD が NVMe ドライバ経由で見える。
    // QEMU でも `-device nvme` でテスト可能。
    let nvme_count = crate::nvme::device_count();
    if nvme_count >= 1 {
        vfs.mount("/nvme", Box::new(|| {
            Box::new(crate::fat32::Fat32::new_fs_with_backend(
                crate::fat32::BlockBackend::Nvme(0),
            ))
        }));
    }

    // 初期化結果をログ出力
    let mut msg = alloc::string::String::from("VFS initialized: / -> fat32, /proc -> procfs");
    if dev_count >= 2 {
        msg.push_str(", /host -> fat32[1]");
    }
    if has_9p {
        msg.push_str(", /9p -> 9p");
    }
    if ahci_count >= 1 {
        msg.push_str(", /ahci -> fat32[ahci0]");
    }
    if nvme_count >= 1 {
        msg.push_str(", /nvme -> fat32[nvme0]");
    }
    crate::kprintln!("{}", msg);
}

/// ファイルを開く
///
/// パスを正規化し、適切なファイルシステムにルーティングして VfsNode を返す。
///
/// # 引数
/// - `path`: 絶対パス（例: "/HELLO.TXT", "/proc/meminfo"）
///
/// # 戻り値
/// VfsNode trait オブジェクト
pub fn open(path: &str) -> Result<Box<dyn VfsNode>, VfsError> {
    let normalized = normalize_path(path)?;
    let vfs = VFS.lock();
    let (fs, relative) = vfs.resolve(&normalized)?;
    drop(vfs); // デッドロック防止: VFS のロックを解放してから FileSystem を操作
    fs.open(&relative)
}

/// ディレクトリ一覧を取得する
///
/// ルートディレクトリ ("/") の場合は、マウントポイントを仮想エントリとして追加する。
/// 例: "/proc" がマウントされていれば "proc/" がエントリに追加される。
///
/// # 引数
/// - `path`: ディレクトリの絶対パス
///
/// # 戻り値
/// VfsDirEntry のベクタ
pub fn list_dir(path: &str) -> Result<Vec<VfsDirEntry>, VfsError> {
    let normalized = normalize_path(path)?;
    let vfs = VFS.lock();
    let (fs, relative) = vfs.resolve(&normalized)?;
    // ルートディレクトリの場合はマウントポイントを追加するため、一覧を取得
    let mount_points = if normalized == "/" {
        vfs.mount_points()
    } else {
        Vec::new()
    };
    drop(vfs); // デッドロック防止

    let mut entries = fs.list_dir(&relative)?;

    // ルートディレクトリの場合はマウントポイントを仮想ディレクトリエントリとして追加
    for mp in mount_points {
        // マウントポイントから名前を抽出（例: "/proc" → "proc"）
        let name = mp.trim_start_matches('/');
        // 既にエントリに存在しないか確認（FAT32 に proc ディレクトリがある場合を考慮）
        if !entries.iter().any(|e| e.name.eq_ignore_ascii_case(name)) {
            entries.push(VfsDirEntry {
                name: String::from(name),
                kind: VfsNodeKind::Directory,
                size: 0,
            });
        }
    }

    Ok(entries)
}

/// ファイルを作成する
///
/// # 引数
/// - `path`: 作成するファイルの絶対パス
/// - `data`: 初期データ
pub fn create_file(path: &str, data: &[u8]) -> Result<(), VfsError> {
    let normalized = normalize_path(path)?;
    let vfs = VFS.lock();
    let (fs, relative) = vfs.resolve(&normalized)?;
    drop(vfs);
    fs.create_file(&relative, data)
}

/// ファイルを削除する
///
/// # 引数
/// - `path`: 削除するファイルの絶対パス
pub fn delete_file(path: &str) -> Result<(), VfsError> {
    let normalized = normalize_path(path)?;
    let vfs = VFS.lock();
    let (fs, relative) = vfs.resolve(&normalized)?;
    drop(vfs);
    fs.delete_file(&relative)
}

/// ディレクトリを作成する
///
/// # 引数
/// - `path`: 作成するディレクトリの絶対パス
pub fn create_dir(path: &str) -> Result<(), VfsError> {
    let normalized = normalize_path(path)?;
    let vfs = VFS.lock();
    let (fs, relative) = vfs.resolve(&normalized)?;
    drop(vfs);
    fs.create_dir(&relative)
}

/// ディレクトリを削除する
///
/// # 引数
/// - `path`: 削除するディレクトリの絶対パス
pub fn delete_dir(path: &str) -> Result<(), VfsError> {
    let normalized = normalize_path(path)?;
    let vfs = VFS.lock();
    let (fs, relative) = vfs.resolve(&normalized)?;
    drop(vfs);
    fs.delete_dir(&relative)
}

/// ファイルの全内容を読み取る（便利関数）
///
/// FileSystem の read_file() メソッドを直接呼ぶ。
/// FileSystem 実装が最適化版を提供していればそちらが使われる。
/// （例: Fat32 は open() → VfsNode::read() の二重コピーを避ける最適化版を持つ）
///
/// # 引数
/// - `path`: 絶対パス
///
/// # 戻り値
/// ファイルの全データ
pub fn read_file(path: &str) -> Result<Vec<u8>, VfsError> {
    let normalized = normalize_path(path)?;
    let vfs = VFS.lock();
    let (fs, relative) = vfs.resolve(&normalized)?;
    drop(vfs); // デッドロック防止
    fs.read_file(&relative)
}

/// VfsError を SyscallError に変換するヘルパー
pub fn vfs_error_to_syscall(e: VfsError) -> SyscallError {
    match e {
        VfsError::NotFound => SyscallError::FileNotFound,
        VfsError::NotADirectory => SyscallError::InvalidArgument,
        VfsError::NotAFile => SyscallError::InvalidArgument,
        VfsError::ReadOnly => SyscallError::ReadOnly,
        VfsError::PermissionDenied => SyscallError::PermissionDenied,
        VfsError::PathTraversal => SyscallError::PathTraversal,
        VfsError::InvalidPath => SyscallError::InvalidArgument,
        VfsError::AlreadyExists => SyscallError::Other,
        VfsError::NoSpace => SyscallError::Other,
        VfsError::IoError => SyscallError::Other,
        VfsError::NotSupported => SyscallError::NotSupported,
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
