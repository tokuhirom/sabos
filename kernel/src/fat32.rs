// fat32.rs — FAT32 ファイルシステムドライバ
//
// FAT16 実装をベースに、FAT32 の読み書き・削除・サブディレクトリ・LFN を実装する。
// 1 クラスタ = N セクタ（512 バイト/セクタ前提）。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use sabos_blockdev::{BlockDevice, BlockError};
use sabos_fat_core::{
    decode_lfn_entries, lfn_checksum, make_short_name, parse_bpb, parse_fsinfo, parse_lfn_part,
    write_fsinfo, FatType, FsInfo, ATTR_LFN, LfnPart,
};

use crate::vfs::{FileSystem, VfsDirEntry, VfsError, VfsNode, VfsNodeKind};

const SECTOR_SIZE: usize = 512;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const FAT32_EOC_MIN: u32 = 0x0FFFFFF8;

/// ディレクトリエントリ
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub short_name: [u8; 11],
    pub attr: u8,
    pub first_cluster: u32,
    pub size: u32,
}

/// カーネル用のブロックデバイス
#[derive(Clone, Copy)]
pub(crate) struct KernelBlockDevice;

impl BlockDevice for KernelBlockDevice {
    fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let mut drv = crate::virtio_blk::VIRTIO_BLK.lock();
        if let Some(ref mut d) = *drv {
            d.read_sector(sector, buf).map_err(|_| BlockError::IoError)
        } else {
            Err(BlockError::IoError)
        }
    }

    fn write_sector(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        let mut drv = crate::virtio_blk::VIRTIO_BLK.lock();
        if let Some(ref mut d) = *drv {
            d.write_sector(sector, buf).map_err(|_| BlockError::IoError)
        } else {
            Err(BlockError::IoError)
        }
    }
}

/// FAT32 ドライバ（BlockDevice 抽象化）
pub struct Fat32Fs<D: BlockDevice> {
    bpb: sabos_fat_core::Bpb,
    fat_start_sector: u32,
    data_start_sector: u32,
    root_cluster: u32,
    fsinfo_sector: u32,
    fsinfo: Option<FsInfo>,
    dev: D,
}

/// カーネル用の FAT32 型
pub type Fat32 = Fat32Fs<KernelBlockDevice>;

impl Fat32 {
    pub fn new() -> Result<Self, &'static str> {
        Fat32Fs::new_with_device(KernelBlockDevice)
    }

    /// VFS マネージャのファクトリ関数から呼ばれる。
    /// FileSystem trait の各メソッドは内部で Fat32::new() を呼ぶため、
    /// ここではダミーインスタンスを返すだけ（即座にフィールドを使わない）。
    pub fn new_fs() -> Self {
        // FileSystem trait の実装では各メソッド内で Fat32::new() を改めて呼ぶので、
        // ここではパニックしないダミーインスタンスを返す。
        // ただし BPB の解析が必要なため、実際に初期化する。
        Fat32::new().expect("Fat32::new_fs: virtio-blk not initialized")
    }
}

impl<D: BlockDevice> Fat32Fs<D> {
    pub fn new_with_device(mut dev: D) -> Result<Self, &'static str> {
        let mut buf = [0u8; SECTOR_SIZE];
        dev.read_sector(0, &mut buf).map_err(|_| "read_sector failed")?;
        let bpb = parse_bpb(&buf)?;
        if bpb.fat_type != FatType::Fat32 {
            return Err("Not FAT32");
        }

        let fat_start_sector = bpb.reserved_sectors as u32;
        let data_start_sector = fat_start_sector + bpb.num_fats as u32 * bpb.fat_size;
        let root_cluster = bpb.root_cluster;
        let fsinfo_sector = bpb.fsinfo_sector as u32;

        let mut fs = Self {
            bpb,
            fat_start_sector,
            data_start_sector,
            root_cluster,
            fsinfo_sector,
            fsinfo: None,
            dev,
        };
        fs.load_fsinfo();
        Ok(fs)
    }

    /// 1 クラスタあたりのバイト数
    pub fn cluster_bytes(&self) -> u32 {
        self.bpb.bytes_per_sector as u32 * self.bpb.sectors_per_cluster as u32
    }

    /// 総クラスタ数（データ領域）
    pub fn total_clusters(&self) -> u32 {
        let data_sectors = self.bpb.total_sectors.saturating_sub(self.data_start_sector);
        data_sectors / self.bpb.sectors_per_cluster as u32
    }

    /// 空きクラスタ数をスキャンして数える
    pub fn free_clusters(&mut self) -> Result<u32, &'static str> {
        if let Some(info) = self.fsinfo {
            if let Some(free) = info.free_cluster_count {
                return Ok(free);
            }
        }

        let total = self.total_clusters();
        let mut free = 0u32;
        for cluster in 2..(total + 2) {
            if self.read_fat_entry(cluster)? == 0 {
                free += 1;
            }
        }

        // 走査結果を FSInfo に反映
        if self.fsinfo.is_some() {
            self.fsinfo = Some(FsInfo {
                free_cluster_count: Some(free),
                next_free_cluster: None,
            });
            let _ = self.flush_fsinfo();
        }

        Ok(free)
    }

    fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        self.dev.read_sector(sector, buf).map_err(|_| "read_sector failed")
    }

    fn write_sector(&mut self, sector: u64, buf: &[u8]) -> Result<(), &'static str> {
        self.dev.write_sector(sector, buf).map_err(|_| "write_sector failed")
    }

    fn load_fsinfo(&mut self) {
        if self.fsinfo_sector == 0 || self.fsinfo_sector == 0xFFFF {
            return;
        }
        let mut buf = [0u8; SECTOR_SIZE];
        if self.read_sector(self.fsinfo_sector as u64, &mut buf).is_ok() {
            self.fsinfo = parse_fsinfo(&buf);
        }
    }

    fn flush_fsinfo(&mut self) -> Result<(), &'static str> {
        let Some(info) = self.fsinfo else { return Ok(()); };
        if self.fsinfo_sector == 0 || self.fsinfo_sector == 0xFFFF {
            return Ok(());
        }
        let mut buf = [0u8; SECTOR_SIZE];
        self.read_sector(self.fsinfo_sector as u64, &mut buf)?;
        write_fsinfo(&mut buf, info);
        self.write_sector(self.fsinfo_sector as u64, &buf)?;
        Ok(())
    }

    /// FAT エントリを読み取る（上位 4bit をマスク）
    fn read_fat_entry(&mut self, cluster: u32) -> Result<u32, &'static str> {
        let fat_offset = cluster * 4;
        let sector = self.fat_start_sector + (fat_offset / self.bpb.bytes_per_sector as u32);
        let offset = (fat_offset % self.bpb.bytes_per_sector as u32) as usize;
        let mut buf = [0u8; SECTOR_SIZE];
        self.read_sector(sector as u64, &mut buf)?;
        let val = u32::from_le_bytes([
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ]) & 0x0FFFFFFF;
        Ok(val)
    }

    /// FAT エントリを書き込む（全 FAT に反映）
    fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), &'static str> {
        let fat_offset = cluster * 4;
        let sector = self.fat_start_sector + (fat_offset / self.bpb.bytes_per_sector as u32);
        let offset = (fat_offset % self.bpb.bytes_per_sector as u32) as usize;
        let val = value & 0x0FFFFFFF;
        let bytes = val.to_le_bytes();
        for fat_idx in 0..self.bpb.num_fats {
            let fat_sector = sector + fat_idx as u32 * self.bpb.fat_size;
            let mut buf = [0u8; SECTOR_SIZE];
            self.read_sector(fat_sector as u64, &mut buf)?;
            buf[offset] = bytes[0];
            buf[offset + 1] = bytes[1];
            buf[offset + 2] = bytes[2];
            buf[offset + 3] = bytes[3];
            self.write_sector(fat_sector as u64, &buf)?;
        }
        Ok(())
    }

    /// クラスタ番号から先頭セクタへ
    fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.data_start_sector + (cluster - 2) * self.bpb.sectors_per_cluster as u32
    }

    /// 次のクラスタを取得
    fn next_cluster(&mut self, cluster: u32) -> Result<Option<u32>, &'static str> {
        let val = self.read_fat_entry(cluster)?;
        if val >= FAT32_EOC_MIN || val == 0 {
            Ok(None)
        } else {
            Ok(Some(val))
        }
    }

    /// クラスタチェーンの全セクタを走査してディレクトリエントリを読み取る
    fn list_dir_cluster(&mut self, start_cluster: u32) -> Result<Vec<DirEntry>, &'static str> {
        let mut entries = Vec::new();
        let mut cluster = start_cluster;
        loop {
            let first_sector = self.cluster_to_sector(cluster);
            for sect_offset in 0..self.bpb.sectors_per_cluster {
                let sector = first_sector + sect_offset as u32;
                let mut buf = [0u8; SECTOR_SIZE];
                self.read_sector(sector as u64, &mut buf)?;
                self.parse_dir_entries(&buf, &mut entries)?;
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => break,
            }
        }
        Ok(entries)
    }

    fn parse_dir_entries(
        &self,
        buf: &[u8],
        entries: &mut Vec<DirEntry>,
    ) -> Result<(), &'static str> {
        let mut lfn_parts: Vec<LfnPart> = Vec::new();
        for i in 0..(buf.len() / 32) {
            let offset = i * 32;
            let first = buf[offset];
            if first == 0x00 {
                break;
            }
            if first == 0xE5 {
                lfn_parts.clear();
                continue;
            }

            let attr = buf[offset + 11];
            if attr == ATTR_LFN {
                let part = parse_lfn_part(&buf[offset..offset + 32])?;
                lfn_parts.push(part);
                continue;
            }

            if attr & ATTR_VOLUME_ID != 0 {
                lfn_parts.clear();
                continue;
            }

            let short_name = {
                let mut s = [0u8; 11];
                s.copy_from_slice(&buf[offset..offset + 11]);
                s
            };

            let first_cluster_hi = u16::from_le_bytes([buf[offset + 20], buf[offset + 21]]) as u32;
            let first_cluster_lo = u16::from_le_bytes([buf[offset + 26], buf[offset + 27]]) as u32;
            let first_cluster = (first_cluster_hi << 16) | first_cluster_lo;
            let size = u32::from_le_bytes([
                buf[offset + 28],
                buf[offset + 29],
                buf[offset + 30],
                buf[offset + 31],
            ]);

            let name = if !lfn_parts.is_empty() {
                let checksum = lfn_checksum(&short_name);
                let mut parts: Vec<LfnPart> = lfn_parts
                    .drain(..)
                    .filter(|p| p.checksum == checksum)
                    .collect();
                parts.sort_by_key(|p| p.order & 0x1F);
                if let Ok(n) = decode_lfn_entries(&parts) {
                    n
                } else {
                    short_name_to_string(&short_name)
                }
            } else {
                short_name_to_string(&short_name)
            };

            entries.push(DirEntry {
                name,
                short_name,
                attr,
                first_cluster,
                size,
            });
        }
        Ok(())
    }

    /// パスからディレクトリの先頭クラスタを取得
    fn find_dir_cluster(&mut self, path: &str) -> Result<u32, &'static str> {
        let mut current = self.root_cluster;
        let mut parts = path.split('/').filter(|p| !p.is_empty());
        while let Some(part) = parts.next() {
            let entries = self.list_dir_cluster(current)?;
            let entry = entries
            .into_iter()
            .find(|e| e.name == part || e.name.eq_ignore_ascii_case(part) || short_name_to_string(&e.short_name).eq_ignore_ascii_case(part));
            let Some(entry) = entry else { return Err("Directory not found"); };
            if entry.attr & ATTR_DIRECTORY == 0 {
                return Err("Not a directory");
            }
            current = entry.first_cluster;
        }
        Ok(current)
    }

    /// パスからエントリを検索
    pub fn find_entry(&mut self, path: &str) -> Result<DirEntry, &'static str> {
        let path = path.trim();
        if path == "/" {
            return Err("Root has no entry");
        }
        let (dir_path, name) = split_parent(path)?;
        let dir_cluster = self.find_dir_cluster(dir_path)?;
        let entries = self.list_dir_cluster(dir_cluster)?;
        entries
            .into_iter()
            .find(|e| e.name == name || e.name.eq_ignore_ascii_case(name) || short_name_to_string(&e.short_name).eq_ignore_ascii_case(name))
            .ok_or("File not found")
    }

    /// ファイルを読み取る
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, &'static str> {
        let entry = self.find_entry(path)?;
        if entry.attr & ATTR_DIRECTORY != 0 {
            return Err("Cannot read directory");
        }
        self.read_file_data(&entry)
    }

    fn read_file_data(&mut self, entry: &DirEntry) -> Result<Vec<u8>, &'static str> {
        // ファイルサイズが分かっているので、事前に容量を確保して
        // Vec の倍々成長による一時メモリ消費を回避する
        let mut data = Vec::with_capacity(entry.size as usize);
        let mut remaining = entry.size as usize;
        let mut cluster = entry.first_cluster;
        if cluster == 0 {
            return Ok(data);
        }
        loop {
            let first_sector = self.cluster_to_sector(cluster);
            for sect_offset in 0..self.bpb.sectors_per_cluster {
                let sector = first_sector + sect_offset as u32;
                let mut buf = [0u8; SECTOR_SIZE];
                self.read_sector(sector as u64, &mut buf)?;
                let to_copy = core::cmp::min(remaining, SECTOR_SIZE);
                data.extend_from_slice(&buf[..to_copy]);
                remaining = remaining.saturating_sub(to_copy);
                if remaining == 0 {
                    return Ok(data);
                }
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => break,
            }
        }
        Ok(data)
    }

    /// ディレクトリ一覧
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, &'static str> {
        let cluster = if path == "/" || path.is_empty() {
            self.root_cluster
        } else {
            self.find_dir_cluster(path)?
        };
        self.list_dir_cluster(cluster)
    }

    /// ファイル作成
    pub fn create_file(&mut self, path: &str, data: &[u8]) -> Result<(), &'static str> {
        let (dir_path, name) = split_parent(path)?;
        let dir_cluster = self.find_dir_cluster(dir_path)?;
        self.create_entry(dir_cluster, name, data, false)
    }

    /// ディレクトリ作成
    pub fn create_dir(&mut self, path: &str) -> Result<(), &'static str> {
        let (dir_path, name) = split_parent(path)?;
        let dir_cluster = self.find_dir_cluster(dir_path)?;
        self.create_entry(dir_cluster, name, &[], true)
    }

    fn create_entry(
        &mut self,
        dir_cluster: u32,
        name: &str,
        data: &[u8],
        is_dir: bool,
    ) -> Result<(), &'static str> {
        if name.is_empty() {
            return Err("name is empty");
        }

        let entries = self.list_dir_cluster(dir_cluster)?;
        if entries.iter().any(|e| e.name == name || short_name_to_string(&e.short_name).eq_ignore_ascii_case(name)) {
            return Err("already exists");
        }

        let short_name = make_short_name(name, &|n| {
            entries.iter().any(|e| &e.short_name == n)
        });
        let checksum = lfn_checksum(&short_name);
        let lfn_entries = build_lfn_entries(name, checksum)?;

        let first_cluster = if is_dir {
            let cluster = self.alloc_cluster()?;
            self.init_dir_cluster(cluster, dir_cluster)?;
            cluster
        } else if !data.is_empty() {
            let (cluster, _size) = self.write_file_data(data)?;
            cluster
        } else {
            0
        };

        // ディレクトリエントリを書き込む
        self.add_dir_entries(dir_cluster, &lfn_entries, &short_name, is_dir, first_cluster, data.len() as u32)
    }

    fn add_dir_entries(
        &mut self,
        dir_cluster: u32,
        lfn_entries: &[LfnRaw],
        short_name: &[u8; 11],
        is_dir: bool,
        first_cluster: u32,
        size: u32,
    ) -> Result<(), &'static str> {
        let mut cluster = dir_cluster;
        loop {
            let first_sector = self.cluster_to_sector(cluster);
            for sect_offset in 0..self.bpb.sectors_per_cluster {
                let sector = first_sector + sect_offset as u32;
                let mut buf = [0u8; SECTOR_SIZE];
                self.read_sector(sector as u64, &mut buf)?;
                for i in 0..(SECTOR_SIZE / 32) {
                    let offset = i * 32;
                    let first = buf[offset];
                    if first == 0x00 || first == 0xE5 {
                        let needed = lfn_entries.len() + 1;
                        if has_contiguous_free(&buf, i, needed) {
                            let mut pos = i;
                            for lfn in lfn_entries {
                                write_lfn_entry(&mut buf, pos * 32, lfn);
                                pos += 1;
                            }
                            write_short_entry(
                                &mut buf,
                                pos * 32,
                                short_name,
                                is_dir,
                                first_cluster,
                                size,
                            );
                            self.write_sector(sector as u64, &buf)?;
                            return Ok(());
                        }
                    }
                }
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => {
                    let new_cluster = self.alloc_cluster()?;
                    self.write_fat_entry(cluster, new_cluster)?;
                    self.write_fat_entry(new_cluster, FAT32_EOC_MIN)?;
                    self.zero_cluster(new_cluster)?;
                    cluster = new_cluster;
                }
            }
        }
    }

    /// ファイル削除
    pub fn delete_file(&mut self, path: &str) -> Result<(), &'static str> {
        self.delete_entry(path, false)
    }

    /// ディレクトリ削除
    pub fn delete_dir(&mut self, path: &str) -> Result<(), &'static str> {
        self.delete_entry(path, true)
    }

    fn delete_entry(&mut self, path: &str, is_dir: bool) -> Result<(), &'static str> {
        let (dir_path, name) = split_parent(path)?;
        let dir_cluster = self.find_dir_cluster(dir_path)?;
        let mut cluster = dir_cluster;
        loop {
            let first_sector = self.cluster_to_sector(cluster);
            for sect_offset in 0..self.bpb.sectors_per_cluster {
                let sector = first_sector + sect_offset as u32;
                let mut buf = [0u8; SECTOR_SIZE];
                self.read_sector(sector as u64, &mut buf)?;
                let mut lfn_offsets: Vec<usize> = Vec::new();
                for i in 0..(SECTOR_SIZE / 32) {
                    let offset = i * 32;
                    let first = buf[offset];
                    if first == 0x00 {
                        break;
                    }
                    if first == 0xE5 {
                        lfn_offsets.clear();
                        continue;
                    }
                    let attr = buf[offset + 11];
                    if attr == ATTR_LFN {
                        lfn_offsets.push(offset);
                        continue;
                    }
                    if attr & ATTR_VOLUME_ID != 0 {
                        lfn_offsets.clear();
                        continue;
                    }
                    let short_name = {
                        let mut s = [0u8; 11];
                        s.copy_from_slice(&buf[offset..offset + 11]);
                        s
                    };
                    let entry_name = short_name_to_string(&short_name);
                    let first_cluster_hi = u16::from_le_bytes([buf[offset + 20], buf[offset + 21]]) as u32;
                    let first_cluster_lo = u16::from_le_bytes([buf[offset + 26], buf[offset + 27]]) as u32;
                    let first_cluster = (first_cluster_hi << 16) | first_cluster_lo;
                    let entry_is_dir = attr & ATTR_DIRECTORY != 0;

                    let lfn_name = if !lfn_offsets.is_empty() {
                        let mut parts: Vec<LfnPart> = Vec::new();
                        for &lfn_off in lfn_offsets.iter() {
                            let part = parse_lfn_part(&buf[lfn_off..lfn_off + 32])?;
                            parts.push(part);
                        }
                        let checksum = lfn_checksum(&short_name);
                        let mut parts: Vec<LfnPart> = parts.into_iter().filter(|p| p.checksum == checksum).collect();
                        parts.sort_by_key(|p| p.order & 0x1F);
                        decode_lfn_entries(&parts).ok()
                    } else {
                        None
                    };

                    let matched = entry_name.eq_ignore_ascii_case(name)
                        || lfn_name.as_deref() == Some(name);
                    if matched {
                        if entry_is_dir != is_dir {
                            return Err("type mismatch");
                        }
                        if entry_is_dir && first_cluster >= 2 && !self.is_dir_empty(first_cluster)? {
                            return Err("Directory not empty");
                        }
                        for &lfn_off in lfn_offsets.iter() {
                            buf[lfn_off] = 0xE5;
                        }
                        buf[offset] = 0xE5;
                        self.write_sector(sector as u64, &buf)?;
                        if first_cluster >= 2 {
                            self.free_cluster_chain(first_cluster)?;
                        }
                        return Ok(());
                    }
                    lfn_offsets.clear();
                }
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => break,
            }
        }
        Err("File not found")
    }

    fn is_dir_empty(&mut self, cluster: u32) -> Result<bool, &'static str> {
        let first_sector = self.cluster_to_sector(cluster);
        for sect_offset in 0..self.bpb.sectors_per_cluster {
            let sector = first_sector + sect_offset as u32;
            let mut buf = [0u8; SECTOR_SIZE];
            self.read_sector(sector as u64, &mut buf)?;
            for i in 0..(SECTOR_SIZE / 32) {
                let offset = i * 32;
                let first = buf[offset];
                if first == 0x00 {
                    return Ok(true);
                }
                if first == 0xE5 {
                    continue;
                }
                let attr = buf[offset + 11];
                if attr == ATTR_LFN || attr & ATTR_VOLUME_ID != 0 {
                    continue;
                }
                let short_name = &buf[offset..offset + 11];
                if short_name[0] == b'.' {
                    continue;
                }
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn write_file_data(&mut self, data: &[u8]) -> Result<(u32, usize), &'static str> {
        let mut first_cluster = 0u32;
        let mut prev_cluster = 0u32;
        let mut remaining = data;
        while !remaining.is_empty() {
            let cluster = self.alloc_cluster()?;
            if first_cluster == 0 {
                first_cluster = cluster;
            } else {
                self.write_fat_entry(prev_cluster, cluster)?;
            }
            let sector = self.cluster_to_sector(cluster);
            for i in 0..self.bpb.sectors_per_cluster {
                let mut buf = [0u8; SECTOR_SIZE];
                let to_copy = core::cmp::min(SECTOR_SIZE, remaining.len());
                buf[..to_copy].copy_from_slice(&remaining[..to_copy]);
                self.write_sector((sector + i as u32) as u64, &buf)?;
                remaining = &remaining[to_copy..];
                if remaining.is_empty() {
                    break;
                }
            }
            prev_cluster = cluster;
        }
        if prev_cluster != 0 {
            self.write_fat_entry(prev_cluster, FAT32_EOC_MIN)?;
        }
        Ok((first_cluster, data.len()))
    }

    fn zero_cluster(&mut self, cluster: u32) -> Result<(), &'static str> {
        let first_sector = self.cluster_to_sector(cluster);
        for i in 0..self.bpb.sectors_per_cluster {
            let buf = [0u8; SECTOR_SIZE];
            self.write_sector((first_sector + i as u32) as u64, &buf)?;
        }
        Ok(())
    }

    fn init_dir_cluster(&mut self, cluster: u32, parent_cluster: u32) -> Result<(), &'static str> {
        let mut buf = [0u8; SECTOR_SIZE];
        let name_dot = format_8_3_name(".")?;
        let name_dotdot = format_8_3_name("..")?;
        write_short_entry(&mut buf, 0, &name_dot, true, cluster, 0);
        write_short_entry(&mut buf, 32, &name_dotdot, true, parent_cluster, 0);
        let first_sector = self.cluster_to_sector(cluster);
        self.write_sector(first_sector as u64, &buf)?;
        for i in 1..self.bpb.sectors_per_cluster {
            let zero = [0u8; SECTOR_SIZE];
            self.write_sector((first_sector + i as u32) as u64, &zero)?;
        }
        Ok(())
    }

    fn alloc_cluster(&mut self) -> Result<u32, &'static str> {
        let total_entries = (self.bpb.fat_size * self.bpb.bytes_per_sector as u32) / 4;
        let start = self
            .fsinfo
            .and_then(|info| info.next_free_cluster)
            .unwrap_or(2);
        for cluster in start..total_entries {
            if self.read_fat_entry(cluster)? == 0 {
                self.write_fat_entry(cluster, FAT32_EOC_MIN)?;
                if let Some(info) = self.fsinfo {
                    let free = info.free_cluster_count.map(|v| v.saturating_sub(1));
                    let next = Some(cluster + 1);
                    self.fsinfo = Some(FsInfo {
                        free_cluster_count: free,
                        next_free_cluster: next,
                    });
                    let _ = self.flush_fsinfo();
                }
                return Ok(cluster);
            }
        }
        for cluster in 2..start {
            if self.read_fat_entry(cluster)? == 0 {
                self.write_fat_entry(cluster, FAT32_EOC_MIN)?;
                if let Some(info) = self.fsinfo {
                    let free = info.free_cluster_count.map(|v| v.saturating_sub(1));
                    let next = Some(cluster + 1);
                    self.fsinfo = Some(FsInfo {
                        free_cluster_count: free,
                        next_free_cluster: next,
                    });
                    let _ = self.flush_fsinfo();
                }
                return Ok(cluster);
            }
        }
        Err("no free cluster")
    }

    fn free_cluster_chain(&mut self, start: u32) -> Result<(), &'static str> {
        let mut cluster = start;
        let mut freed = 0u32;
        while cluster >= 2 && cluster < FAT32_EOC_MIN {
            let next = self.read_fat_entry(cluster)?;
            self.write_fat_entry(cluster, 0)?;
            freed = freed.saturating_add(1);
            if next >= FAT32_EOC_MIN || next == 0 {
                break;
            }
            cluster = next;
        }
        if let Some(info) = self.fsinfo {
            let free = info.free_cluster_count.map(|v| v.saturating_add(freed));
            self.fsinfo = Some(FsInfo {
                free_cluster_count: free,
                next_free_cluster: Some(start),
            });
            let _ = self.flush_fsinfo();
        }
        Ok(())
    }
}

// =================================================================
// VFS 実装
// =================================================================

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
        let mut fs = Fat32::new().map_err(|_| VfsError::IoError)?;
        if path == "/" || path.is_empty() {
            return Err(VfsError::NotAFile);
        }
        // 完全修飾パスで呼ぶ（FileSystem::read_file との曖昧さを回避）
        let data = Fat32Fs::<KernelBlockDevice>::read_file(&mut fs, path)
            .map_err(|_| VfsError::NotFound)?;
        Ok(Box::new(Fat32File { data }))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<VfsDirEntry>, VfsError> {
        let mut fs = Fat32::new().map_err(|_| VfsError::IoError)?;
        let entries = Fat32Fs::<KernelBlockDevice>::list_dir(&mut fs, path)
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
        let mut fs = Fat32::new().map_err(|_| VfsError::IoError)?;
        Fat32Fs::<KernelBlockDevice>::create_file(&mut fs, path, data)
            .map_err(|_| VfsError::IoError)
    }

    fn delete_file(&self, path: &str) -> Result<(), VfsError> {
        let mut fs = Fat32::new().map_err(|_| VfsError::IoError)?;
        Fat32Fs::<KernelBlockDevice>::delete_file(&mut fs, path)
            .map_err(|_| VfsError::NotFound)
    }

    fn create_dir(&self, path: &str) -> Result<(), VfsError> {
        let mut fs = Fat32::new().map_err(|_| VfsError::IoError)?;
        Fat32Fs::<KernelBlockDevice>::create_dir(&mut fs, path)
            .map_err(|_| VfsError::IoError)
    }

    fn delete_dir(&self, path: &str) -> Result<(), VfsError> {
        let mut fs = Fat32::new().map_err(|_| VfsError::IoError)?;
        Fat32Fs::<KernelBlockDevice>::delete_dir(&mut fs, path)
            .map_err(|_| VfsError::IoError)
    }

    /// ファイルの全内容を一括読み取り（Fat32 最適化版）
    ///
    /// open() → VfsNode::read() を使うと二重にメモリを確保してしまうため、
    /// Fat32 の read_file() を直接呼んでコピーを 1 回に抑える。
    fn read_file(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let mut fs = Fat32::new().map_err(|_| VfsError::IoError)?;
        Fat32Fs::<KernelBlockDevice>::read_file(&mut fs, path)
            .map_err(|_| VfsError::NotFound)
    }
}

fn short_name_to_string(name: &[u8; 11]) -> String {
    let base = core::str::from_utf8(&name[..8]).unwrap_or("").trim_end_matches(' ');
    let ext = core::str::from_utf8(&name[8..]).unwrap_or("").trim_end_matches(' ');
    if ext.is_empty() {
        String::from(base)
    } else {
        let mut s = String::new();
        s.push_str(base);
        s.push('.');
        s.push_str(ext);
        s
    }
}

fn split_parent(path: &str) -> Result<(&str, &str), &'static str> {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        return Err("invalid path");
    }
    if let Some(pos) = path.rfind('/') {
        let (dir, name) = path.split_at(pos);
        let name = name.trim_start_matches('/');
        let dir = if dir.is_empty() { "/" } else { dir };
        Ok((dir, name))
    } else {
        Ok(("/", path))
    }
}

fn has_contiguous_free(buf: &[u8], start_entry: usize, count: usize) -> bool {
    for i in 0..count {
        let offset = (start_entry + i) * 32;
        if offset >= buf.len() {
            return false;
        }
        let first = buf[offset];
        if first != 0x00 && first != 0xE5 {
            return false;
        }
    }
    true
}

fn format_8_3_name(name: &str) -> Result<[u8; 11], &'static str> {
    if name == "." {
        let mut out = [b' '; 11];
        out[0] = b'.';
        return Ok(out);
    }
    if name == ".." {
        let mut out = [b' '; 11];
        out[0] = b'.';
        out[1] = b'.';
        return Ok(out);
    }
    let mut out = [b' '; 11];
    let upper = name.to_ascii_uppercase();
    let mut parts = upper.splitn(2, '.');
    let base = parts.next().unwrap_or("");
    let ext = parts.next().unwrap_or("");
    if base.is_empty() {
        return Err("invalid name");
    }
    for (i, b) in base.bytes().take(8).enumerate() {
        out[i] = b;
    }
    for (i, b) in ext.bytes().take(3).enumerate() {
        out[8 + i] = b;
    }
    Ok(out)
}

// LFN 生成用の内部構造
struct LfnRaw {
    order: u8,
    checksum: u8,
    name: [u16; 13],
}

fn build_lfn_entries(name: &str, checksum: u8) -> Result<Vec<LfnRaw>, &'static str> {
    let utf16: Vec<u16> = name.encode_utf16().collect();
    let mut parts: Vec<[u16; 13]> = Vec::new();
    let mut idx = 0usize;
    let total = (utf16.len() + 12) / 13;
    for _ in 0..total {
        let mut part = [0xFFFFu16; 13];
        for i in 0..13 {
            if idx < utf16.len() {
                part[i] = utf16[idx];
                idx += 1;
            } else if idx == utf16.len() {
                part[i] = 0x0000;
                idx += 1;
            }
        }
        parts.push(part);
    }

    // LFN は逆順で並べる（最後の断片が先頭に来る）
    let mut out: Vec<LfnRaw> = Vec::new();
    let total = parts.len();
    for (i, part) in parts.into_iter().rev().enumerate() {
        let mut order = (total - i) as u8;
        if i == 0 {
            order |= 0x40;
        }
        out.push(LfnRaw {
            order,
            checksum,
            name: part,
        });
    }
    Ok(out)
}

fn write_lfn_entry(buf: &mut [u8], offset: usize, entry: &LfnRaw) {
    buf[offset] = entry.order;
    buf[offset + 11] = ATTR_LFN;
    buf[offset + 13] = entry.checksum;
    // type/first_cluster = 0
    buf[offset + 12] = 0;
    buf[offset + 26] = 0;
    buf[offset + 27] = 0;

    let mut idx = 0usize;
    for &off in &[1usize, 3, 5, 7, 9] {
        let ch = entry.name[idx].to_le_bytes();
        buf[offset + off] = ch[0];
        buf[offset + off + 1] = ch[1];
        idx += 1;
    }
    for &off in &[14usize, 16, 18, 20, 22, 24] {
        let ch = entry.name[idx].to_le_bytes();
        buf[offset + off] = ch[0];
        buf[offset + off + 1] = ch[1];
        idx += 1;
    }
    for &off in &[28usize, 30] {
        let ch = entry.name[idx].to_le_bytes();
        buf[offset + off] = ch[0];
        buf[offset + off + 1] = ch[1];
        idx += 1;
    }
}

fn write_short_entry(
    buf: &mut [u8],
    offset: usize,
    short_name: &[u8; 11],
    is_dir: bool,
    first_cluster: u32,
    size: u32,
) {
    buf[offset..offset + 11].copy_from_slice(short_name);
    buf[offset + 11] = if is_dir { ATTR_DIRECTORY } else { 0 };
    // クラスタ番号を high/low に分割
    let hi = (first_cluster >> 16) as u16;
    let lo = (first_cluster & 0xFFFF) as u16;
    let hi_bytes = hi.to_le_bytes();
    let lo_bytes = lo.to_le_bytes();
    buf[offset + 20] = hi_bytes[0];
    buf[offset + 21] = hi_bytes[1];
    buf[offset + 26] = lo_bytes[0];
    buf[offset + 27] = lo_bytes[1];
    let size_bytes = size.to_le_bytes();
    buf[offset + 28] = size_bytes[0];
    buf[offset + 29] = size_bytes[1];
    buf[offset + 30] = size_bytes[2];
    buf[offset + 31] = size_bytes[3];
}
