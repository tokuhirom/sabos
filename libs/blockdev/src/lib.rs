#![no_std]

/// ブロックデバイスの抽象インターフェース。
///
/// 512 バイトセクタを前提にしている。
pub trait BlockDevice {
    /// 1 セクタ読み取り
    fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    /// 1 セクタ書き込み
    fn write_sector(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError>;
}

/// ブロックデバイスエラー
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    IoError,
    InvalidArgument,
}
