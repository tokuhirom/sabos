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
use alloc::vec::Vec;
use crate::serial_println;
use crate::virtio_blk;

/// セクタサイズ（バイト数）。FAT16 の標準は 512 バイト。
const SECTOR_SIZE: usize = 512;

/// FAT16 のディレクトリエントリの属性フラグ。
/// これらは OR で組み合わせて使われる。
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

        let total_sectors = if bpb.total_sectors_16 != 0 {
            bpb.total_sectors_16 as u32
        } else {
            bpb.total_sectors_32
        };

        serial_println!(
            "FAT16: bps={}, spc={}, reserved={}, fats={}, root_entries={}, fat_size={}, total_sectors={}",
            bpb.bytes_per_sector, bpb.sectors_per_cluster, bpb.reserved_sectors,
            bpb.num_fats, bpb.root_entry_count, bpb.fat_size_16, total_sectors
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

            // ディレクトリエントリをパース
            if let Some(result) = self.parse_dir_entries(&buf, &mut entries)? {
                return Ok(result);
            }
        }

        Ok(entries)
    }

    /// サブディレクトリのエントリ一覧を返す。
    ///
    /// サブディレクトリはルートディレクトリと違い、データ領域にクラスタチェーンとして配置される。
    /// first_cluster: サブディレクトリの先頭クラスタ番号（DirEntry.first_cluster から取得）
    pub fn list_subdir(&self, first_cluster: u16) -> Result<Vec<DirEntry>, &'static str> {
        let mut entries = Vec::new();
        let mut buf = [0u8; SECTOR_SIZE];
        let mut cluster = first_cluster;

        // クラスタチェーンを辿ってディレクトリエントリを読む
        while cluster >= 2 && cluster <= 0xFFEF {
            let first_sector_of_cluster = self.data_start_sector
                + ((cluster as u32) - 2) * (self.bpb.sectors_per_cluster as u32);

            // クラスタ内の各セクタを読む
            for sect_offset in 0..(self.bpb.sectors_per_cluster as u32) {
                let sector = first_sector_of_cluster + sect_offset;
                {
                    let mut drv = virtio_blk::VIRTIO_BLK.lock();
                    let drv = drv.as_mut().ok_or("virtio-blk not available")?;
                    drv.read_sector(sector as u64, &mut buf)?;
                }

                // ディレクトリエントリをパース
                if let Some(result) = self.parse_dir_entries(&buf, &mut entries)? {
                    return Ok(result);
                }
            }

            // FAT テーブルから次のクラスタ番号を読む
            cluster = self.read_fat_entry(cluster)?;
        }

        Ok(entries)
    }

    /// セクタバッファからディレクトリエントリをパースして entries に追加する。
    /// 終端マーカー (0x00) を見つけたら Some(entries) を返して早期終了を示す。
    /// まだ続く場合は None を返す。
    fn parse_dir_entries(
        &self,
        buf: &[u8; SECTOR_SIZE],
        entries: &mut Vec<DirEntry>,
    ) -> Result<Option<Vec<DirEntry>>, &'static str> {
        let entries_per_sector = SECTOR_SIZE / 32;
        for i in 0..entries_per_sector {
            let offset = i * 32;
            let first_byte = buf[offset];

            // 0x00: ここ以降にエントリはない（ディレクトリ終端）
            if first_byte == 0x00 {
                return Ok(Some(entries.clone()));
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

        Ok(None)
    }

    /// パスを解析して、対象のディレクトリのエントリ一覧を返す。
    ///
    /// path: "/" 区切りのパス。"/" または "" ならルートディレクトリ。
    ///       例: "/", "/SUBDIR", "/SUBDIR/NESTED"
    pub fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, &'static str> {
        let path = path.trim();

        // ルートディレクトリの場合
        if path.is_empty() || path == "/" {
            return self.list_root_dir();
        }

        // パスを "/" で分割してディレクトリを辿る
        let path = path.trim_start_matches('/');
        let mut current_entries = self.list_root_dir()?;

        for component in path.split('/') {
            if component.is_empty() {
                continue;
            }

            // 大文字に変換して検索
            let component_upper: String = component.chars()
                .map(|c| c.to_ascii_uppercase())
                .collect();

            // 現在のディレクトリから該当エントリを探す
            let entry = current_entries
                .iter()
                .find(|e| e.name == component_upper)
                .ok_or("Directory not found")?;

            // ディレクトリでなければエラー
            if entry.attr & ATTR_DIRECTORY == 0 {
                return Err("Not a directory");
            }

            // サブディレクトリのエントリ一覧を取得
            current_entries = self.list_subdir(entry.first_cluster)?;
        }

        Ok(current_entries)
    }

    /// パスを解析して、対象のファイルまたはディレクトリのエントリを探す。
    ///
    /// path: "/" 区切りのパス。例: "/HELLO.TXT", "/SUBDIR/FILE.ELF"
    pub fn find_entry(&self, path: &str) -> Result<DirEntry, &'static str> {
        let path = path.trim().trim_start_matches('/');
        if path.is_empty() {
            return Err("Empty path");
        }

        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return Err("Empty path");
        }

        // 最後の要素がファイル名、それ以前がディレクトリパス
        let (dir_parts, filename) = parts.split_at(parts.len() - 1);

        // ディレクトリを辿る
        let mut current_entries = self.list_root_dir()?;
        for component in dir_parts {
            let component_upper: String = component.chars()
                .map(|c| c.to_ascii_uppercase())
                .collect();

            let entry = current_entries
                .iter()
                .find(|e| e.name == component_upper)
                .ok_or("Directory not found")?;

            if entry.attr & ATTR_DIRECTORY == 0 {
                return Err("Not a directory");
            }

            current_entries = self.list_subdir(entry.first_cluster)?;
        }

        // ファイル/ディレクトリを探す
        let filename_upper: String = filename[0].chars()
            .map(|c| c.to_ascii_uppercase())
            .collect();

        current_entries
            .into_iter()
            .find(|e| e.name == filename_upper)
            .ok_or("File not found")
    }

    /// 指定したパスのファイルを読み取る。
    ///
    /// path: "/" 区切りのパス。例: "HELLO.TXT", "/SUBDIR/FILE.ELF"
    /// 戻り値: ファイルの内容のバイト列
    ///
    /// FAT16 のファイル読み取り手順:
    ///   1. パスを辿ってファイルエントリを探す
    ///   2. 先頭クラスタ番号を取得
    ///   3. FAT テーブルを辿ってクラスタチェーンを追跡
    ///   4. 各クラスタのデータをセクタ単位で読み取る
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, &'static str> {
        // パスからファイルエントリを探す
        let entry = self.find_entry(path)?;

        if entry.attr & ATTR_DIRECTORY != 0 {
            return Err("Cannot read directory");
        }

        serial_println!(
            "FAT16: reading file '{}', cluster={}, size={}",
            entry.name, entry.first_cluster, entry.size
        );

        self.read_file_data(&entry)
    }

    /// DirEntry からファイルデータを読み取る（内部関数）。
    fn read_file_data(&self, entry: &DirEntry) -> Result<Vec<u8>, &'static str> {
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

    // ========================================
    // 書き込み機能
    // ========================================

    /// セクタにデータを書き込む（内部用ヘルパー）。
    fn write_sector(&self, sector: u32, buf: &[u8; SECTOR_SIZE]) -> Result<(), &'static str> {
        let mut drv = virtio_blk::VIRTIO_BLK.lock();
        let drv = drv.as_mut().ok_or("virtio-blk not available")?;
        drv.write_sector(sector as u64, buf)
    }

    /// FAT テーブルのエントリを書き込む。
    ///
    /// FAT16 では各エントリは 16 ビット。
    /// FAT #1 と FAT #2（バックアップ）の両方を更新する。
    fn write_fat_entry(&self, cluster: u16, value: u16) -> Result<(), &'static str> {
        let fat_offset = (cluster as u32) * 2;
        let offset_in_sector = (fat_offset % (self.bpb.bytes_per_sector as u32)) as usize;

        // FAT #1 と FAT #2 の両方を更新
        for fat_num in 0..self.bpb.num_fats {
            let fat_sector = self.fat_start_sector
                + (fat_num as u32) * (self.bpb.fat_size_16 as u32)
                + fat_offset / (self.bpb.bytes_per_sector as u32);

            // セクタを読み込んで更新
            let mut buf = [0u8; SECTOR_SIZE];
            {
                let mut drv = virtio_blk::VIRTIO_BLK.lock();
                let drv = drv.as_mut().ok_or("virtio-blk not available")?;
                drv.read_sector(fat_sector as u64, &mut buf)?;
            }

            let bytes = value.to_le_bytes();
            buf[offset_in_sector] = bytes[0];
            buf[offset_in_sector + 1] = bytes[1];

            self.write_sector(fat_sector, &buf)?;
        }

        Ok(())
    }

    /// 空きクラスタを探す。
    ///
    /// FAT テーブルをスキャンして、値が 0x0000（空き）のエントリを探す。
    /// クラスタ 2 から検索開始（クラスタ 0, 1 は予約済み）。
    fn find_free_cluster(&self) -> Result<u16, &'static str> {
        let mut buf = [0u8; SECTOR_SIZE];
        let entries_per_sector = SECTOR_SIZE / 2; // 各エントリは 2 バイト

        for sect_offset in 0..(self.bpb.fat_size_16 as u32) {
            let sector = self.fat_start_sector + sect_offset;
            {
                let mut drv = virtio_blk::VIRTIO_BLK.lock();
                let drv = drv.as_mut().ok_or("virtio-blk not available")?;
                drv.read_sector(sector as u64, &mut buf)?;
            }

            for i in 0..entries_per_sector {
                let cluster = (sect_offset as usize) * entries_per_sector + i;
                if cluster < 2 {
                    continue; // クラスタ 0, 1 は予約済み
                }
                if cluster > 0xFFEF {
                    break; // FAT16 のクラスタ番号の上限
                }

                let entry = u16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]);
                if entry == 0x0000 {
                    return Ok(cluster as u16);
                }
            }
        }

        Err("No free clusters available")
    }

    /// 必要な数のクラスタを確保し、チェーンを作成する。
    ///
    /// 戻り値: チェーンの先頭クラスタ番号
    fn allocate_clusters(&self, count: usize) -> Result<u16, &'static str> {
        if count == 0 {
            return Err("Cannot allocate 0 clusters");
        }

        let mut clusters = Vec::with_capacity(count);

        // 必要な数の空きクラスタを見つける
        for _ in 0..count {
            let cluster = self.find_free_cluster()?;
            // 一時的にマークして重複を避ける（後でチェーン化）
            self.write_fat_entry(cluster, 0xFFFF)?; // 終端マーク
            clusters.push(cluster);
        }

        // クラスタをチェーン化
        for i in 0..(count - 1) {
            self.write_fat_entry(clusters[i], clusters[i + 1])?;
        }
        // 最後のクラスタは終端マーク（既に 0xFFFF）

        Ok(clusters[0])
    }

    /// クラスタチェーンを解放する（ファイル削除用）。
    fn free_cluster_chain(&self, first_cluster: u16) -> Result<(), &'static str> {
        let mut cluster = first_cluster;
        while cluster >= 2 && cluster <= 0xFFEF {
            let next = self.read_fat_entry(cluster)?;
            self.write_fat_entry(cluster, 0x0000)?; // 空きにする
            cluster = next;
        }
        Ok(())
    }

    /// ルートディレクトリに新しいエントリを追加する。
    ///
    /// 空きエントリ（先頭バイトが 0x00 または 0xE5）を探して書き込む。
    fn add_root_dir_entry(&self, entry: &DirEntry) -> Result<(), &'static str> {
        let mut buf = [0u8; SECTOR_SIZE];

        for sect_offset in 0..self.root_dir_sectors {
            let sector = self.root_dir_start_sector + sect_offset;
            {
                let mut drv = virtio_blk::VIRTIO_BLK.lock();
                let drv = drv.as_mut().ok_or("virtio-blk not available")?;
                drv.read_sector(sector as u64, &mut buf)?;
            }

            let entries_per_sector = SECTOR_SIZE / 32;
            for i in 0..entries_per_sector {
                let offset = i * 32;
                let first_byte = buf[offset];

                // 空きエントリを見つけた
                if first_byte == 0x00 || first_byte == 0xE5 {
                    // 8.3 形式のファイル名を作成
                    let (name_part, ext_part) = self.split_filename(&entry.name);

                    // ファイル名（8バイト、スペースパディング）
                    for j in 0..8 {
                        buf[offset + j] = if j < name_part.len() {
                            name_part.as_bytes()[j]
                        } else {
                            b' '
                        };
                    }
                    // 拡張子（3バイト、スペースパディング）
                    for j in 0..3 {
                        buf[offset + 8 + j] = if j < ext_part.len() {
                            ext_part.as_bytes()[j]
                        } else {
                            b' '
                        };
                    }

                    // 属性
                    buf[offset + 11] = entry.attr;
                    // 予約フィールド（0クリア）
                    for j in 12..26 {
                        buf[offset + j] = 0;
                    }
                    // 先頭クラスタ番号（リトルエンディアン）
                    let cluster_bytes = entry.first_cluster.to_le_bytes();
                    buf[offset + 26] = cluster_bytes[0];
                    buf[offset + 27] = cluster_bytes[1];
                    // ファイルサイズ（リトルエンディアン）
                    let size_bytes = entry.size.to_le_bytes();
                    buf[offset + 28] = size_bytes[0];
                    buf[offset + 29] = size_bytes[1];
                    buf[offset + 30] = size_bytes[2];
                    buf[offset + 31] = size_bytes[3];

                    self.write_sector(sector, &buf)?;
                    return Ok(());
                }
            }
        }

        Err("Root directory is full")
    }

    /// ファイル名を名前部分と拡張子部分に分割する（8.3 形式用）。
    fn split_filename(&self, name: &str) -> (String, String) {
        if let Some(dot_pos) = name.rfind('.') {
            let name_part: String = name[..dot_pos].chars().take(8).collect();
            let ext_part: String = name[dot_pos + 1..].chars().take(3).collect();
            (name_part, ext_part)
        } else {
            (name.chars().take(8).collect(), String::new())
        }
    }

    /// ファイルを作成してデータを書き込む。
    ///
    /// path: ファイルパス（現在はルートディレクトリのみ対応）
    /// data: 書き込むデータ
    ///
    /// 注意: 同名ファイルが存在する場合はエラーを返す（上書きは未対応）。
    pub fn create_file(&self, filename: &str, data: &[u8]) -> Result<(), &'static str> {
        // ファイル名を大文字に変換
        let filename_upper: String = filename
            .trim()
            .trim_start_matches('/')
            .chars()
            .map(|c| c.to_ascii_uppercase())
            .collect();

        // 同名ファイルが存在しないか確認
        let entries = self.list_root_dir()?;
        if entries.iter().any(|e| e.name == filename_upper) {
            return Err("File already exists");
        }

        // 必要なクラスタ数を計算
        let bytes_per_cluster =
            (self.bpb.sectors_per_cluster as usize) * (self.bpb.bytes_per_sector as usize);
        let cluster_count = if data.is_empty() {
            1 // 空ファイルでも最低 1 クラスタ
        } else {
            (data.len() + bytes_per_cluster - 1) / bytes_per_cluster
        };

        serial_println!(
            "FAT16: creating file '{}', size={}, clusters={}",
            filename_upper, data.len(), cluster_count
        );

        // クラスタを確保
        let first_cluster = self.allocate_clusters(cluster_count)?;

        // データをクラスタに書き込む
        let mut remaining = data;
        let mut cluster = first_cluster;
        let mut sector_buf = [0u8; SECTOR_SIZE];

        while !remaining.is_empty() && cluster >= 2 && cluster <= 0xFFEF {
            let first_sector_of_cluster = self.data_start_sector
                + ((cluster as u32) - 2) * (self.bpb.sectors_per_cluster as u32);

            for sect_offset in 0..(self.bpb.sectors_per_cluster as u32) {
                if remaining.is_empty() {
                    break;
                }

                let sector = first_sector_of_cluster + sect_offset;
                let to_write = remaining.len().min(SECTOR_SIZE);

                // セクタバッファをクリアしてデータをコピー
                sector_buf.fill(0);
                sector_buf[..to_write].copy_from_slice(&remaining[..to_write]);

                self.write_sector(sector, &sector_buf)?;
                remaining = &remaining[to_write..];
            }

            if !remaining.is_empty() {
                cluster = self.read_fat_entry(cluster)?;
            }
        }

        // ディレクトリエントリを作成
        let entry = DirEntry {
            name: filename_upper,
            attr: 0, // 通常ファイル
            first_cluster,
            size: data.len() as u32,
        };
        self.add_root_dir_entry(&entry)?;

        serial_println!("FAT16: file created successfully, first_cluster={}", first_cluster);
        Ok(())
    }

    /// ファイルを削除する。
    ///
    /// ディレクトリエントリを削除済みマーク (0xE5) にし、
    /// クラスタチェーンを解放する。
    pub fn delete_file(&self, filename: &str) -> Result<(), &'static str> {
        let filename_upper: String = filename
            .trim()
            .trim_start_matches('/')
            .chars()
            .map(|c| c.to_ascii_uppercase())
            .collect();

        let mut buf = [0u8; SECTOR_SIZE];

        for sect_offset in 0..self.root_dir_sectors {
            let sector = self.root_dir_start_sector + sect_offset;
            {
                let mut drv = virtio_blk::VIRTIO_BLK.lock();
                let drv = drv.as_mut().ok_or("virtio-blk not available")?;
                drv.read_sector(sector as u64, &mut buf)?;
            }

            let entries_per_sector = SECTOR_SIZE / 32;
            for i in 0..entries_per_sector {
                let offset = i * 32;
                let first_byte = buf[offset];

                if first_byte == 0x00 {
                    // ディレクトリ終端
                    return Err("File not found");
                }
                if first_byte == 0xE5 {
                    continue; // 削除済み
                }

                let attr = buf[offset + 11];
                if attr == ATTR_LFN || attr & ATTR_VOLUME_ID != 0 {
                    continue;
                }

                // ファイル名をパース
                let name_part = core::str::from_utf8(&buf[offset..offset + 8])
                    .unwrap_or("")
                    .trim_end();
                let ext_part = core::str::from_utf8(&buf[offset + 8..offset + 11])
                    .unwrap_or("")
                    .trim_end();
                let name = if ext_part.is_empty() {
                    String::from(name_part)
                } else {
                    let mut s = String::from(name_part);
                    s.push('.');
                    s.push_str(ext_part);
                    s
                };

                if name == filename_upper {
                    // クラスタチェーンを解放
                    let first_cluster =
                        u16::from_le_bytes([buf[offset + 26], buf[offset + 27]]);
                    if first_cluster >= 2 {
                        self.free_cluster_chain(first_cluster)?;
                    }

                    // ディレクトリエントリを削除済みマーク
                    buf[offset] = 0xE5;
                    self.write_sector(sector, &buf)?;

                    serial_println!("FAT16: file '{}' deleted", filename_upper);
                    return Ok(());
                }
            }
        }

        Err("File not found")
    }
}
