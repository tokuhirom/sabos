// fat32.rs — FAT32 ファイルシステムドライバ（カーネル統合層）
//
// コアロジック（Fat32Fs<D>、DirEntry 等）は libs/fat32 に分離済み。
// このファイルにはカーネル固有の部分（KernelBlockDevice、VFS 実装）だけを残す。

use alloc::boxed::Box;
use alloc::vec::Vec;

use sabos_blockdev::{BlockDevice, BlockError};

// sabos-fat32 ライブラリから再エクスポート
pub use sabos_fat32::{
    Fat32Fs, DirEntry, ATTR_DIRECTORY,
};

use crate::vfs::{FileSystem, VfsDirEntry, VfsError, VfsNode, VfsNodeKind};

/// カーネル用のブロックデバイス。
/// dev_index で使用する virtio-blk デバイスを指定する。
/// 0 = 最初のデバイス（disk.img）、1 = 2台目（ホスト共有用）、...
#[derive(Clone, Copy)]
pub struct KernelBlockDevice {
    pub dev_index: usize,
}

impl BlockDevice for KernelBlockDevice {
    fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let mut devs = crate::virtio_blk::VIRTIO_BLKS.lock();
        if let Some(d) = devs.get_mut(self.dev_index) {
            d.read_sector(sector, buf).map_err(|_| BlockError::IoError)
        } else {
            Err(BlockError::IoError)
        }
    }

    fn write_sector(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        let mut devs = crate::virtio_blk::VIRTIO_BLKS.lock();
        if let Some(d) = devs.get_mut(self.dev_index) {
            d.write_sector(sector, buf).map_err(|_| BlockError::IoError)
        } else {
            Err(BlockError::IoError)
        }
    }
}

/// カーネル用の FAT32 ドライバ（ニュータイプラッパー）。
///
/// Fat32Fs<KernelBlockDevice> を包み、カーネル固有のコンストラクタを提供する。
/// Deref により Fat32Fs のメソッドに直接アクセスできる。
pub struct Fat32 {
    inner: Fat32Fs<KernelBlockDevice>,
}

impl core::ops::Deref for Fat32 {
    type Target = Fat32Fs<KernelBlockDevice>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl core::ops::DerefMut for Fat32 {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Fat32 {
    /// デバイスインデックス 0（最初の virtio-blk）で Fat32 を初期化する。
    pub fn new() -> Result<Self, &'static str> {
        Self::new_with_index(0)
    }

    /// 指定したデバイスインデックスの virtio-blk で Fat32 を初期化する。
    pub fn new_with_index(dev_index: usize) -> Result<Self, &'static str> {
        let inner = Fat32Fs::new_with_device(KernelBlockDevice { dev_index })?;
        Ok(Fat32 { inner })
    }

    /// VFS マネージャのファクトリ関数から呼ばれる（デバイスインデックス 0）。
    pub fn new_fs() -> Self {
        Fat32::new().expect("Fat32::new_fs: virtio-blk not initialized")
    }

    /// 指定デバイスインデックスの VFS マネージャ用ファクトリ関数。
    #[allow(dead_code)]
    pub fn new_fs_with_index(dev_index: usize) -> Self {
        Fat32::new_with_index(dev_index).expect("Fat32::new_fs_with_index: virtio-blk not initialized")
    }

    /// 内部の dev_index を取得する
    fn dev_index(&self) -> usize {
        self.inner.dev.dev_index
    }
}

// =================================================================
// VFS 実装
// =================================================================

/// Fat32Fs のメソッドを明示的に呼ぶヘルパー。
/// FileSystem trait と Fat32Fs のメソッド名が衝突するため、
/// 完全修飾パスで Fat32Fs のメソッドを呼ぶ。
fn fat32_read_file(fs: &mut Fat32Fs<KernelBlockDevice>, path: &str) -> Result<Vec<u8>, &'static str> {
    fs.read_file(path)
}

fn fat32_list_dir(fs: &mut Fat32Fs<KernelBlockDevice>, path: &str) -> Result<Vec<DirEntry>, &'static str> {
    fs.list_dir(path)
}

fn fat32_create_file(fs: &mut Fat32Fs<KernelBlockDevice>, path: &str, data: &[u8]) -> Result<(), &'static str> {
    fs.create_file(path, data)
}

fn fat32_delete_file(fs: &mut Fat32Fs<KernelBlockDevice>, path: &str) -> Result<(), &'static str> {
    fs.delete_file(path)
}

fn fat32_create_dir(fs: &mut Fat32Fs<KernelBlockDevice>, path: &str) -> Result<(), &'static str> {
    fs.create_dir(path)
}

fn fat32_delete_dir(fs: &mut Fat32Fs<KernelBlockDevice>, path: &str) -> Result<(), &'static str> {
    fs.delete_dir(path)
}

struct Fat32File {
    data: Vec<u8>,
}

impl VfsNode for Fat32File {
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

impl FileSystem for Fat32 {
    fn name(&self) -> &str {
        "fat32"
    }

    fn open(&self, path: &str) -> Result<Box<dyn VfsNode>, VfsError> {
        let mut fs = Fat32::new_with_index(self.dev_index()).map_err(|_| VfsError::IoError)?;
        if path == "/" || path.is_empty() {
            return Err(VfsError::NotAFile);
        }
        let data = fat32_read_file(&mut fs.inner, path)
            .map_err(|_| VfsError::NotFound)?;
        Ok(Box::new(Fat32File { data }))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<VfsDirEntry>, VfsError> {
        let mut fs = Fat32::new_with_index(self.dev_index()).map_err(|_| VfsError::IoError)?;
        let entries = fat32_list_dir(&mut fs.inner, path)
            .map_err(|_| VfsError::NotFound)?;
        Ok(entries
            .into_iter()
            .map(|e| VfsDirEntry {
                name: e.name,
                kind: if e.attr & ATTR_DIRECTORY != 0 {
                    VfsNodeKind::Directory
                } else {
                    VfsNodeKind::File
                },
                size: e.size as usize,
            })
            .collect())
    }

    fn create_file(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let mut fs = Fat32::new_with_index(self.dev_index()).map_err(|_| VfsError::IoError)?;
        fat32_create_file(&mut fs.inner, path, data)
            .map_err(|_| VfsError::IoError)
    }

    fn delete_file(&self, path: &str) -> Result<(), VfsError> {
        let mut fs = Fat32::new_with_index(self.dev_index()).map_err(|_| VfsError::IoError)?;
        fat32_delete_file(&mut fs.inner, path)
            .map_err(|_| VfsError::NotFound)
    }

    fn create_dir(&self, path: &str) -> Result<(), VfsError> {
        let mut fs = Fat32::new_with_index(self.dev_index()).map_err(|_| VfsError::IoError)?;
        fat32_create_dir(&mut fs.inner, path)
            .map_err(|_| VfsError::IoError)
    }

    fn delete_dir(&self, path: &str) -> Result<(), VfsError> {
        let mut fs = Fat32::new_with_index(self.dev_index()).map_err(|_| VfsError::IoError)?;
        fat32_delete_dir(&mut fs.inner, path)
            .map_err(|_| VfsError::IoError)
    }

    /// ファイルの全内容を一括読み取り（Fat32 最適化版）
    ///
    /// open() → VfsNode::read() を使うと二重にメモリを確保してしまうため、
    /// Fat32 の read_file() を直接呼んでコピーを 1 回に抑える。
    fn read_file(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let mut fs = Fat32::new_with_index(self.dev_index()).map_err(|_| VfsError::IoError)?;
        fat32_read_file(&mut fs.inner, path)
            .map_err(|_| VfsError::NotFound)
    }
}
