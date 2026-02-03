// fat16.rs — FAT16 ファイルシステムドライバ
//
// FAT (File Allocation Table) は DOS 時代から使われているシンプルなファイルシステム。
// FAT16 は最大 2GB のボリュームをサポートし、構造が単純で実装しやすい。
//
// ## ディスクレイアウト
//
//   [ブートセクタ (BPB)]  ← セクタ 0
//   [予約セクタ ...]       ← reserved_sectors 個分
//   [FAT #1]              ← num_fats × fat_size_16 セクタ
//   [FAT #2 (バックアップ)]
//   [ルートディレクトリ]    ← root_entry_count × 32 バイト
//   [データ領域]           ← クラスタ 2 から始まる
//
// ## BPB (BIOS Parameter Block)
//
// ブートセクタ（セクタ 0）の先頭に置かれるパラメータ群。
// ファイルシステムのジオメトリ（セクタサイズ、クラスタサイズ、FAT の位置等）を定義する。
//
// ## FAT テーブル
//
// FAT16 の FAT エントリは 16 ビット。各エントリはクラスタ番号に対応し、
// 次のクラスタ番号を指す（リンクリスト構造）。
//   0x0000 = 空きクラスタ
//   0x0002〜0xFFEF = 次のクラスタ番号
//   0xFFF0〜0xFFF6 = 予約
//   0xFFF7 = 不良クラスタ
//   0xFFF8〜0xFFFF = チェーン終端（End of Chain）
//
// ## ディレクトリエントリ
//
// 各エントリは 32 バイト固定長。ファイル名（8+3 形式）、属性、サイズ、
// 開始クラスタ番号を保持する。

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use crate::serial_println;
use crate::virtio_blk;

/// セクタサイズ（バイト数）。FAT16 の標準は 512 バイト。
const SECTOR_SIZE: usize = 512;

/// FAT16 のディレクトリエントリの属性フラグ。
/// これらは OR で組み合わせて使われる。
const ATTR_READ_ONLY: u8 = 0x01;
const _ATTR_HIDDEN: u8 = 0x02;
const _ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const _ATTR_ARCHIVE: u8 = 0x20;
/// LFN (Long File Name) エントリの属性マスク。
/// ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID がすべて立っている場合、
/// そのエントリは LFN エントリ（長いファイル名の一部）であり、通常のファイルではない。
const ATTR_LFN: u8 = 0x0F;

/// BPB (BIOS Parameter Block) の情報を保持する構造体。
/// ブートセクタ（セクタ 0）からパースした FAT16 のパラメータ。
#[derive(Debug)]
pub struct Fat16Bpb {
    /// 1セクタのバイト数（通常 512）
    pub bytes_per_sector: u16,
    /// 1クラスタのセクタ数（FAT16 では通常 4, 8, 16, 32, 64 等）
    pub sectors_per_cluster: u8,
    /// 予約セクタ数（ブートセクタを含む、通常 1）
    pub reserved_sectors: u16,
    /// FAT テーブルの数（通常 2 = オリジナル + バックアップ）
    pub num_fats: u8,
    /// ルートディレクトリのエントリ数（FAT16 固有、通常 512）
    pub root_entry_count: u16,
    /// 1 つの FAT テーブルが占めるセクタ数
    pub fat_size_16: u16,
    /// ボリューム全体のセクタ数（16ビット版、0 なら total_sectors_32 を使う）
    pub total_sectors_16: u16,
    /// ボリューム全体のセクタ数（32ビット版）
    pub total_sectors_32: u32,
}

/// ディレクトリエントリの情報を保持する構造体。
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// ファイル名（8.3 形式、トリム済み。例: "HELLO.TXT"）
    pub name: String,
    /// ファイル属性
    pub attr: u8,
    /// ファイルの先頭クラスタ番号
    pub first_cluster: u16,
    /// ファイルサイズ（バイト数）
    pub size: u32,
}

/// FAT16 ファイルシステムのドライバ構造体。
/// BPB 情報と計算済みのセクタオフセットを保持する。
pub struct Fat16 {
    /// BPB から読み取ったパラメータ
    pub bpb: Fat16Bpb,
    /// FAT テーブルの開始セクタ番号
    fat_start_sector: u32,
    /// ルートディレクトリの開始セクタ番号
    root_dir_start_sector: u32,
    /// ルートディレクトリが占めるセクタ数
    root_dir_sectors: u32,
    /// データ領域の開始セクタ番号（クラスタ 2 がここから始まる）
    data_start_sector: u32,
}

impl Fat16 {
    /// virtio-blk デバイスからセクタ 0 を読み取り、FAT16 ファイルシステムを初期化する。
    ///
    /// BPB をパースして各領域のセクタオフセットを計算する。
    pub fn new() -> Result<Self, &'static str> {
        // セクタ 0（ブートセクタ）を読み取る
        let mut buf = [0u8; SECTOR_SIZE];
        {
            let mut drv = virtio_blk::VIRTIO_BLK.lock();
            let drv = drv.as_mut().ok_or("virtio-blk not available")?;
            drv.read_sector(0, &mut buf)?;
        }

        // BPB のシグネチャを確認（セクタ末尾が 0x55, 0xAA）
        if buf[510] != 0x55 || buf[511] != 0xAA {
            return Err("Invalid boot sector signature");
        }

        // BPB フィールドをパースする。
        // FAT16 の BPB は固定オフセットにフィールドが配置されている。
        let bytes_per_sector = u16::from_le_bytes([buf[11], buf[12]]);
        let sectors_per_cluster = buf[13];
        let reserved_sectors = u16::from_le_bytes([buf[14], buf[15]]);
        let num_fats = buf[16];
        let root_entry_count = u16::from_le_bytes([buf[17], buf[18]]);
        let fat_size_16 = u16::from_le_bytes([buf[22], buf[23]]);
        let total_sectors_16 = u16::from_le_bytes([buf[19], buf[20]]);
        let total_sectors_32 = u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);

        let bpb = Fat16Bpb {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            root_entry_count,
            fat_size_16,
            total_sectors_16,
            total_sectors_32,
        };

        serial_println!(
            "FAT16: bps={}, spc={}, reserved={}, fats={}, root_entries={}, fat_size={}",
            bpb.bytes_per_sector, bpb.sectors_per_cluster, bpb.reserved_sectors,
            bpb.num_fats, bpb.root_entry_count, bpb.fat_size_16
        );

        // 各領域の開始セクタを計算する
        let fat_start_sector = reserved_sectors as u32;
        let root_dir_start_sector = fat_start_sector + (num_fats as u32) * (fat_size_16 as u32);
        // ルートディレクトリは固定サイズ: root_entry_count × 32 バイト
        let root_dir_sectors =
            ((root_entry_count as u32) * 32 + (bytes_per_sector as u32) - 1) / (bytes_per_sector as u32);
        let data_start_sector = root_dir_start_sector + root_dir_sectors;

        serial_println!(
            "FAT16: fat_start={}, root_start={}, root_sects={}, data_start={}",
            fat_start_sector, root_dir_start_sector, root_dir_sectors, data_start_sector
        );

        Ok(Fat16 {
            bpb,
            fat_start_sector,
            root_dir_start_sector,
            root_dir_sectors,
            data_start_sector,
        })
    }

    /// ルートディレクトリのエントリ一覧を返す。
    ///
    /// FAT16 のルートディレクトリは固定位置・固定サイズで、
    /// データ領域の直前に配置される（FAT32 とは異なる）。
    pub fn list_root_dir(&self) -> Result<Vec<DirEntry>, &'static str> {
        let mut entries = Vec::new();
        let mut buf = [0u8; SECTOR_SIZE];

        // ルートディレクトリの各セクタを読む
        for sect_offset in 0..self.root_dir_sectors {
            let sector = self.root_dir_start_sector + sect_offset;
            {
                let mut drv = virtio_blk::VIRTIO_BLK.lock();
                let drv = drv.as_mut().ok_or("virtio-blk not available")?;
                drv.read_sector(sector as u64, &mut buf)?;
            }

            // 各セクタには SECTOR_SIZE / 32 個のディレクトリエントリが入る
            let entries_per_sector = SECTOR_SIZE / 32;
            for i in 0..entries_per_sector {
                let offset = i * 32;
                let first_byte = buf[offset];

                // 0x00: ここ以降にエントリはない（ディレクトリ終端）
                if first_byte == 0x00 {
                    return Ok(entries);
                }

                // 0xE5: 削除済みエントリ（スキップ）
                if first_byte == 0xE5 {
                    continue;
                }

                let attr = buf[offset + 11];

                // LFN エントリはスキップ（長いファイル名の一部で、今回は対応しない）
                if attr == ATTR_LFN {
                    continue;
                }

                // ボリュームラベルはスキップ
                if attr & ATTR_VOLUME_ID != 0 {
                    continue;
                }

                // ファイル名を 8.3 形式からパース
                // [0..8] = ファイル名（右パディングがスペース）
                // [8..11] = 拡張子（右パディングがスペース）
                let name_part = core::str::from_utf8(&buf[offset..offset + 8])
                    .unwrap_or("????????")
                    .trim_end();
                let ext_part = core::str::from_utf8(&buf[offset + 8..offset + 11])
                    .unwrap_or("???")
                    .trim_end();

                let name = if ext_part.is_empty() {
                    String::from(name_part)
                } else {
                    let mut s = String::from(name_part);
                    s.push('.');
                    s.push_str(ext_part);
                    s
                };

                // 先頭クラスタ番号（16ビット）
                let first_cluster =
                    u16::from_le_bytes([buf[offset + 26], buf[offset + 27]]);
                // ファイルサイズ（32ビット）
                let size = u32::from_le_bytes([
                    buf[offset + 28],
                    buf[offset + 29],
                    buf[offset + 30],
                    buf[offset + 31],
                ]);

                entries.push(DirEntry {
                    name,
                    attr,
                    first_cluster,
                    size,
                });
            }
        }

        Ok(entries)
    }

    /// 指定したファイル名のファイルをルートディレクトリから探して内容を読み取る。
    ///
    /// filename: 大文字の 8.3 形式ファイル名（例: "HELLO.TXT"）
    /// 戻り値: ファイルの内容のバイト列
    ///
    /// FAT16 のファイル読み取り手順:
    ///   1. ルートディレクトリからファイルエントリを探す
    ///   2. 先頭クラスタ番号を取得
    ///   3. FAT テーブルを辿ってクラスタチェーンを追跡
    ///   4. 各クラスタのデータをセクタ単位で読み取る
    pub fn read_file(&self, filename: &str) -> Result<Vec<u8>, &'static str> {
        // ルートディレクトリからファイルを探す
        let entries = self.list_root_dir()?;
        let entry = entries
            .iter()
            .find(|e| e.name == filename)
            .ok_or("File not found")?;

        if entry.attr & ATTR_DIRECTORY != 0 {
            return Err("Cannot read directory");
        }

        serial_println!(
            "FAT16: reading file '{}', cluster={}, size={}",
            entry.name, entry.first_cluster, entry.size
        );

        let bytes_per_cluster =
            (self.bpb.sectors_per_cluster as u32) * (self.bpb.bytes_per_sector as u32);
        let mut data = Vec::with_capacity(entry.size as usize);
        let mut remaining = entry.size as usize;
        let mut cluster = entry.first_cluster;

        // クラスタチェーンを辿ってデータを読む
        while cluster >= 2 && cluster <= 0xFFEF && remaining > 0 {
            // クラスタ番号 → セクタ番号の変換
            // データ領域はクラスタ 2 から始まるので、(cluster - 2) × sectors_per_cluster を加える
            let first_sector_of_cluster = self.data_start_sector
                + ((cluster as u32) - 2) * (self.bpb.sectors_per_cluster as u32);

            // クラスタ内の各セクタを読む
            for sect_offset in 0..(self.bpb.sectors_per_cluster as u32) {
                if remaining == 0 {
                    break;
                }

                let sector = first_sector_of_cluster + sect_offset;
                let mut buf = [0u8; SECTOR_SIZE];
                {
                    let mut drv = virtio_blk::VIRTIO_BLK.lock();
                    let drv = drv.as_mut().ok_or("virtio-blk not available")?;
                    drv.read_sector(sector as u64, &mut buf)?;
                }

                let to_copy = remaining.min(SECTOR_SIZE);
                data.extend_from_slice(&buf[..to_copy]);
                remaining -= to_copy;
            }

            // FAT テーブルから次のクラスタ番号を読む
            cluster = self.read_fat_entry(cluster)?;
        }

        Ok(data)
    }

    /// FAT テーブルから指定クラスタの次のクラスタ番号を読み取る。
    ///
    /// FAT16 の各エントリは 16 ビット（2 バイト）。
    /// FAT テーブルのオフセット = cluster * 2。
    fn read_fat_entry(&self, cluster: u16) -> Result<u16, &'static str> {
        // FAT エントリのバイトオフセット
        let fat_offset = (cluster as u32) * 2;
        // FAT 内のセクタ番号
        let fat_sector = self.fat_start_sector + fat_offset / (self.bpb.bytes_per_sector as u32);
        // セクタ内のバイトオフセット
        let offset_in_sector = (fat_offset % (self.bpb.bytes_per_sector as u32)) as usize;

        let mut buf = [0u8; SECTOR_SIZE];
        {
            let mut drv = virtio_blk::VIRTIO_BLK.lock();
            let drv = drv.as_mut().ok_or("virtio-blk not available")?;
            drv.read_sector(fat_sector as u64, &mut buf)?;
        }

        Ok(u16::from_le_bytes([
            buf[offset_in_sector],
            buf[offset_in_sector + 1],
        ]))
    }
}
