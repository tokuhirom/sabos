// blockdev_user.rs — ユーザー空間ブロックデバイス
//
// SYS_BLOCK_READ / SYS_BLOCK_WRITE syscall 経由で
// カーネルの virtio-blk デバイスにアクセスする BlockDevice 実装。
// fat32d から使う。

use sabos_blockdev::{BlockDevice, BlockError};
use crate::syscall_fat32d as syscall;

/// ユーザー空間からカーネルの virtio-blk デバイスにアクセスするブロックデバイス。
///
/// dev_index でどの virtio-blk デバイスを使うか指定する。
/// - 0: disk.img（システムディスク）
/// - 1: hostfs.img（ホスト共有）
pub struct UserBlockDevice {
    pub dev_index: u64,
}

impl BlockDevice for UserBlockDevice {
    fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let result = syscall::block_read_dev(sector, buf, self.dev_index);
        if result < 0 {
            Err(BlockError::IoError)
        } else {
            Ok(())
        }
    }

    fn write_sector(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        let result = syscall::block_write_dev(sector, buf, self.dev_index);
        if result < 0 {
            Err(BlockError::IoError)
        } else {
            Ok(())
        }
    }
}
