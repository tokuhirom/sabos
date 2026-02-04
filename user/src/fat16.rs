// fat16.rs — FAT16 ファイルシステムドライバ（ユーザー空間）
//
// カーネルの virtio-blk に直接触れず、ブロック読み書きシステムコールを使う。
// 最低限の FAT16 実装で、ls/cat/write/rm をユーザー空間で動かす。

use alloc::string::String;
use alloc::vec::Vec;

use crate::syscall;

const SECTOR_SIZE: usize = 512;

/// FAT エントリの終端判定
const FAT16_EOC_MIN: u16 = 0xFFF8;

/// ディレクトリエントリの属性
const ATTR_LFN: u8 = 0x0F;
const ATTR_DIRECTORY: u8 = 0x10;

/// BPB (BIOS Parameter Block)
#[derive(Debug)]
pub struct Fat16Bpb {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub num_fats: u8,
    pub root_entry_count: u16,
    pub fat_size_16: u16,
}

/// ディレクトリエントリ
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
}

/// FAT16 ドライバ
pub struct Fat16 {
    pub bpb: Fat16Bpb,
    fat_start_sector: u32,
    root_dir_start_sector: u32,
    data_start_sector: u32,
}

impl Fat16 {
    pub fn new() -> Result<Self, &'static str> {
        let mut buf = [0u8; SECTOR_SIZE];
        block_read(0, &mut buf)?;

        if buf[510] != 0x55 || buf[511] != 0xAA {
            return Err("Invalid boot sector signature");
        }

        let bytes_per_sector = u16::from_le_bytes([buf[11], buf[12]]);
        let sectors_per_cluster = buf[13];
        let reserved_sectors = u16::from_le_bytes([buf[14], buf[15]]);
        let num_fats = buf[16];
        let root_entry_count = u16::from_le_bytes([buf[17], buf[18]]);
        let fat_size_16 = u16::from_le_bytes([buf[22], buf[23]]);
        let bpb = Fat16Bpb {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            root_entry_count,
            fat_size_16,
        };

        let root_dir_sectors =
            ((bpb.root_entry_count as u32 * 32) + (bpb.bytes_per_sector as u32 - 1))
                / bpb.bytes_per_sector as u32;

        let fat_start_sector = bpb.reserved_sectors as u32;
        let root_dir_start_sector = fat_start_sector + (bpb.num_fats as u32 * bpb.fat_size_16 as u32);
        let data_start_sector = root_dir_start_sector + root_dir_sectors;

        Ok(Self {
            bpb,
            fat_start_sector,
            root_dir_start_sector,
            data_start_sector,
        })
    }

    /// ディレクトリ一覧を取得
    pub fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, &'static str> {
        if path != "/" && !path.is_empty() {
            return Err("only root dir supported");
        }

        let mut entries = Vec::new();
        let total_entries = self.bpb.root_entry_count as usize;
        let bytes_per_sector = self.bpb.bytes_per_sector as usize;
        let mut sector = self.root_dir_start_sector;

        let mut read_entries = 0usize;
        while read_entries < total_entries {
            let mut buf = [0u8; SECTOR_SIZE];
            block_read(sector as u64, &mut buf)?;

            let entries_in_sector = bytes_per_sector / 32;
            for i in 0..entries_in_sector {
                if read_entries >= total_entries {
                    break;
                }

                let offset = i * 32;
                let first = buf[offset];
                if first == 0x00 {
                    return Ok(entries);
                }
                if first == 0xE5 {
                    read_entries += 1;
                    continue;
                }

                let attr = buf[offset + 11];
                if attr == ATTR_LFN {
                    read_entries += 1;
                    continue;
                }

                let name = parse_8_3_name(&buf[offset..offset + 11]);
                entries.push(DirEntry {
                    name,
                });

                read_entries += 1;
            }

            sector += 1;
        }

        Ok(entries)
    }

    /// ディレクトリを作成する（ルートのみ対応）
    pub fn create_dir(&self, name: &str) -> Result<(), &'static str> {
        let upper = name.trim().trim_start_matches('/').to_ascii_uppercase();
        if upper.is_empty() {
            return Err("directory name is empty");
        }
        if upper.contains('/') {
            return Err("only root dir supported");
        }
        if upper.contains('.') {
            return Err("invalid directory name");
        }

        // 既存エントリがあればエラー
        let entries = self.list_dir("/")?;
        if entries.iter().any(|e| e.name.to_ascii_uppercase() == upper) {
            return Err("already exists");
        }

        let total_entries = self.bpb.root_entry_count as usize;
        let bytes_per_sector = self.bpb.bytes_per_sector as usize;
        let mut sector = self.root_dir_start_sector;

        let mut read_entries = 0usize;
        while read_entries < total_entries {
            let mut buf = [0u8; SECTOR_SIZE];
            block_read(sector as u64, &mut buf)?;

            let entries_in_sector = bytes_per_sector / 32;
            for i in 0..entries_in_sector {
                if read_entries >= total_entries {
                    break;
                }

                let offset = i * 32;
                let first = buf[offset];
                if first == 0x00 || first == 0xE5 {
                    let dir_cluster = self.alloc_cluster()?;
                    if let Err(e) = self.init_dir_cluster(dir_cluster, 0) {
                        let _ = self.free_cluster_chain(dir_cluster);
                        return Err(e);
                    }

                    let name_bytes = format_8_3_name(&upper)?;
                    self.write_dir_entry(&mut buf, offset, &name_bytes, ATTR_DIRECTORY, dir_cluster, 0);
                    block_write(sector as u64, &buf)?;
                    return Ok(());
                }

                read_entries += 1;
            }

            sector += 1;
        }

        Err("root directory full")
    }

    /// ディレクトリを削除する（ルートのみ対応）
    pub fn remove_dir(&self, name: &str) -> Result<(), &'static str> {
        let upper = name.trim().trim_start_matches('/').to_ascii_uppercase();
        if upper.is_empty() {
            return Err("directory name is empty");
        }
        if upper.contains('/') {
            return Err("only root dir supported");
        }

        let total_entries = self.bpb.root_entry_count as usize;
        let bytes_per_sector = self.bpb.bytes_per_sector as usize;
        let mut sector = self.root_dir_start_sector;

        let mut read_entries = 0usize;
        while read_entries < total_entries {
            let mut buf = [0u8; SECTOR_SIZE];
            block_read(sector as u64, &mut buf)?;

            let entries_in_sector = bytes_per_sector / 32;
            for i in 0..entries_in_sector {
                if read_entries >= total_entries {
                    break;
                }

                let offset = i * 32;
                let first = buf[offset];
                if first == 0x00 {
                    return Err("directory not found");
                }
                if first == 0xE5 {
                    read_entries += 1;
                    continue;
                }

                let attr = buf[offset + 11];
                if attr == ATTR_LFN {
                    read_entries += 1;
                    continue;
                }

                let entry_name = parse_8_3_name(&buf[offset..offset + 11]);
                if entry_name.to_ascii_uppercase() == upper {
                    if attr & ATTR_DIRECTORY == 0 {
                        return Err("not a directory");
                    }

                    let first_cluster = u16::from_le_bytes([buf[offset + 26], buf[offset + 27]]);
                    if first_cluster >= 2 {
                        if !self.is_dir_empty(first_cluster)? {
                            return Err("directory not empty");
                        }
                        self.free_cluster_chain(first_cluster)?;
                    }

                    buf[offset] = 0xE5;
                    block_write(sector as u64, &buf)?;
                    return Ok(());
                }

                read_entries += 1;
            }

            sector += 1;
        }

        Err("directory not found")
    }

    /// ファイルを削除する
    pub fn delete_file(&self, filename: &str) -> Result<(), &'static str> {
        let upper = filename.to_ascii_uppercase();

        let total_entries = self.bpb.root_entry_count as usize;
        let bytes_per_sector = self.bpb.bytes_per_sector as usize;
        let mut sector = self.root_dir_start_sector;

        let mut read_entries = 0usize;
        while read_entries < total_entries {
            let mut buf = [0u8; SECTOR_SIZE];
            block_read(sector as u64, &mut buf)?;

            let entries_in_sector = bytes_per_sector / 32;
            for i in 0..entries_in_sector {
                if read_entries >= total_entries {
                    break;
                }

                let offset = i * 32;
                let first = buf[offset];
                if first == 0x00 {
                    return Err("file not found");
                }
                if first == 0xE5 {
                    read_entries += 1;
                    continue;
                }

                let attr = buf[offset + 11];
                if attr == ATTR_LFN {
                    read_entries += 1;
                    continue;
                }

                let name = parse_8_3_name(&buf[offset..offset + 11]);
                if name.to_ascii_uppercase() == upper {
                    // 先頭クラスタを FAT から解放
                    let first_cluster = u16::from_le_bytes([buf[offset + 26], buf[offset + 27]]);
                    if first_cluster >= 2 {
                        self.free_cluster_chain(first_cluster)?;
                    }

                    // ディレクトリエントリを削除マーク
                    buf[offset] = 0xE5;
                    block_write(sector as u64, &buf)?;
                    return Ok(());
                }

                read_entries += 1;
            }

            sector += 1;
        }

        Err("file not found")
    }

    /// ファイルを作成/上書きする
    pub fn create_file(&self, filename: &str, data: &[u8]) -> Result<(), &'static str> {
        let upper = filename.to_ascii_uppercase();

        // 既存ファイルがあれば削除
        let _ = self.delete_file(&upper);

        let total_entries = self.bpb.root_entry_count as usize;
        let bytes_per_sector = self.bpb.bytes_per_sector as usize;
        let mut sector = self.root_dir_start_sector;

        let mut read_entries = 0usize;
        while read_entries < total_entries {
            let mut buf = [0u8; SECTOR_SIZE];
            block_read(sector as u64, &mut buf)?;

            let entries_in_sector = bytes_per_sector / 32;
            for i in 0..entries_in_sector {
                if read_entries >= total_entries {
                    break;
                }

                let offset = i * 32;
                let first = buf[offset];
                if first == 0x00 || first == 0xE5 {
                    // 空きエントリに作成
                    let name = format_8_3_name(&upper)?;
                    buf[offset..offset + 11].copy_from_slice(&name);
                    buf[offset + 11] = 0x20; // archive

                    let (first_cluster, size) = self.write_file_data(data)?;
                    buf[offset + 26..offset + 28].copy_from_slice(&first_cluster.to_le_bytes());
                    buf[offset + 28..offset + 32].copy_from_slice(&(size as u32).to_le_bytes());

                    block_write(sector as u64, &buf)?;
                    return Ok(());
                }

                read_entries += 1;
            }

            sector += 1;
        }

        Err("directory full")
    }

    fn write_file_data(&self, data: &[u8]) -> Result<(u16, usize), &'static str> {
        let mut remaining = data.len();
        let mut offset = 0usize;
        let mut first_cluster = 0u16;
        let mut prev_cluster = 0u16;

        while remaining > 0 {
            let cluster = self.alloc_cluster()?;
            if first_cluster == 0 {
                first_cluster = cluster;
            } else {
                self.write_fat_entry(prev_cluster, cluster)?;
            }

            let sector = self.cluster_to_sector(cluster);
            for i in 0..self.bpb.sectors_per_cluster {
                let mut buf = [0u8; SECTOR_SIZE];
                let copy_len = core::cmp::min(remaining, SECTOR_SIZE);
                buf[..copy_len].copy_from_slice(&data[offset..offset + copy_len]);
                block_write((sector + i as u32) as u64, &buf)?;
                offset += copy_len;
                remaining -= copy_len;
                if remaining == 0 {
                    break;
                }
            }

            prev_cluster = cluster;
        }

        if prev_cluster != 0 {
            self.write_fat_entry(prev_cluster, 0xFFFF)?;
        }

        Ok((first_cluster, data.len()))
    }

    fn cluster_to_sector(&self, cluster: u16) -> u32 {
        self.data_start_sector + (cluster as u32 - 2) * self.bpb.sectors_per_cluster as u32
    }

    fn init_dir_cluster(&self, cluster: u16, parent_cluster: u16) -> Result<(), &'static str> {
        let mut buf = [0u8; SECTOR_SIZE];
        buf.fill(0);

        let name_dot = format_dir_name(".")?;
        self.write_dir_entry(&mut buf, 0, &name_dot, ATTR_DIRECTORY, cluster, 0);
        let name_dotdot = format_dir_name("..")?;
        self.write_dir_entry(&mut buf, 32, &name_dotdot, ATTR_DIRECTORY, parent_cluster, 0);

        let first_sector = self.cluster_to_sector(cluster);
        block_write(first_sector as u64, &buf)?;

        for i in 1..self.bpb.sectors_per_cluster {
            let mut zero = [0u8; SECTOR_SIZE];
            zero.fill(0);
            block_write((first_sector + i as u32) as u64, &zero)?;
        }

        Ok(())
    }

    fn is_dir_empty(&self, cluster: u16) -> Result<bool, &'static str> {
        let first_sector = self.cluster_to_sector(cluster);
        for i in 0..self.bpb.sectors_per_cluster {
            let mut buf = [0u8; SECTOR_SIZE];
            block_read((first_sector + i as u32) as u64, &mut buf)?;
            let entries_in_sector = SECTOR_SIZE / 32;
            for e in 0..entries_in_sector {
                let offset = e * 32;
                let first = buf[offset];
                if first == 0x00 {
                    return Ok(true);
                }
                if first == 0xE5 {
                    continue;
                }
                let attr = buf[offset + 11];
                if attr == ATTR_LFN {
                    continue;
                }
                let name = parse_8_3_name(&buf[offset..offset + 11]);
                if name != "." && name != ".." {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn write_dir_entry(
        &self,
        buf: &mut [u8; SECTOR_SIZE],
        offset: usize,
        name_bytes: &[u8; 11],
        attr: u8,
        first_cluster: u16,
        size: u32,
    ) {
        buf[offset..offset + 11].copy_from_slice(name_bytes);
        buf[offset + 11] = attr;
        for j in 12..26 {
            buf[offset + j] = 0;
        }
        let cluster_bytes = first_cluster.to_le_bytes();
        buf[offset + 26] = cluster_bytes[0];
        buf[offset + 27] = cluster_bytes[1];
        let size_bytes = size.to_le_bytes();
        buf[offset + 28] = size_bytes[0];
        buf[offset + 29] = size_bytes[1];
        buf[offset + 30] = size_bytes[2];
        buf[offset + 31] = size_bytes[3];
    }

    fn read_fat_entry(&self, cluster: u16) -> Result<u16, &'static str> {
        let fat_offset = cluster as u32 * 2;
        let sector = self.fat_start_sector + (fat_offset / SECTOR_SIZE as u32);
        let offset = (fat_offset % SECTOR_SIZE as u32) as usize;

        let mut buf = [0u8; SECTOR_SIZE];
        block_read(sector as u64, &mut buf)?;

        Ok(u16::from_le_bytes([buf[offset], buf[offset + 1]]))
    }

    fn write_fat_entry(&self, cluster: u16, value: u16) -> Result<(), &'static str> {
        let fat_offset = cluster as u32 * 2;
        let sector = self.fat_start_sector + (fat_offset / SECTOR_SIZE as u32);
        let offset = (fat_offset % SECTOR_SIZE as u32) as usize;

        let mut buf = [0u8; SECTOR_SIZE];
        block_read(sector as u64, &mut buf)?;
        buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        block_write(sector as u64, &buf)?;

        // 2つ目の FAT も更新
        if self.bpb.num_fats >= 2 {
            let sector2 = sector + self.bpb.fat_size_16 as u32;
            block_read(sector2 as u64, &mut buf)?;
            buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
            block_write(sector2 as u64, &buf)?;
        }

        Ok(())
    }

    fn alloc_cluster(&self) -> Result<u16, &'static str> {
        let total_entries = (self.bpb.fat_size_16 as u32 * SECTOR_SIZE as u32) / 2;
        for cluster in 2..total_entries {
            if self.read_fat_entry(cluster as u16)? == 0 {
                self.write_fat_entry(cluster as u16, 0xFFFF)?;
                return Ok(cluster as u16);
            }
        }
        Err("no free cluster")
    }

    fn free_cluster_chain(&self, start: u16) -> Result<(), &'static str> {
        let mut cluster = start;
        while cluster >= 2 && cluster < FAT16_EOC_MIN {
            let next = self.read_fat_entry(cluster)?;
            self.write_fat_entry(cluster, 0)?;
            cluster = next;
        }
        Ok(())
    }
}

fn parse_8_3_name(raw: &[u8]) -> String {
    let name = core::str::from_utf8(&raw[0..8]).unwrap_or("");
    let ext = core::str::from_utf8(&raw[8..11]).unwrap_or("");
    let name = name.trim_end();
    let ext = ext.trim_end();
    if ext.is_empty() {
        String::from(name)
    } else {
        let mut s = String::new();
        s.push_str(name);
        s.push('.');
        s.push_str(ext);
        s
    }
}

fn format_8_3_name(name: &str) -> Result<[u8; 11], &'static str> {
    let mut out = [b' '; 11];
    let parts: Vec<&str> = name.splitn(2, '.').collect();
    let base = parts.get(0).copied().unwrap_or("");
    let ext = parts.get(1).copied().unwrap_or("");

    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return Err("invalid 8.3 name");
    }

    for (i, b) in base.as_bytes().iter().enumerate() {
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.as_bytes().iter().enumerate() {
        out[8 + i] = b.to_ascii_uppercase();
    }
    Ok(out)
}

fn format_dir_name(name: &str) -> Result<[u8; 11], &'static str> {
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
    format_8_3_name(name)
}

fn block_read(sector: u64, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    let ret = syscall::block_read(sector, buf);
    if ret < 0 {
        Err("block_read failed")
    } else {
        Ok(())
    }
}

fn block_write(sector: u64, buf: &[u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    let ret = syscall::block_write(sector, buf);
    if ret < 0 {
        Err("block_write failed")
    } else {
        Ok(())
    }
}
