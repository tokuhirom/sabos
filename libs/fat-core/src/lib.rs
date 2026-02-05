#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// FAT の種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatType {
    Fat16,
    Fat32,
}

/// BPB (BIOS Parameter Block) 解析結果
#[derive(Debug, Clone, Copy)]
pub struct Bpb {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub num_fats: u8,
    pub root_entry_count: u16,
    pub total_sectors: u32,
    pub fat_size: u32,
    pub root_cluster: u32,     // FAT32 のみ有効
    pub fsinfo_sector: u16,    // FAT32 のみ有効
    pub fat_type: FatType,
}

/// FAT32 FSInfo 情報
#[derive(Debug, Clone, Copy)]
pub struct FsInfo {
    pub free_cluster_count: Option<u32>,
    pub next_free_cluster: Option<u32>,
}

/// BPB をパースして FAT 種別を判定する
pub fn parse_bpb(buf: &[u8]) -> Result<Bpb, &'static str> {
    if buf.len() < 512 {
        return Err("BPB buffer too small");
    }
    if buf[510] != 0x55 || buf[511] != 0xAA {
        return Err("Invalid boot sector signature");
    }

    let bytes_per_sector = u16::from_le_bytes([buf[11], buf[12]]);
    let sectors_per_cluster = buf[13];
    let reserved_sectors = u16::from_le_bytes([buf[14], buf[15]]);
    let num_fats = buf[16];
    let root_entry_count = u16::from_le_bytes([buf[17], buf[18]]);
    let total_sectors_16 = u16::from_le_bytes([buf[19], buf[20]]);
    let fat_size_16 = u16::from_le_bytes([buf[22], buf[23]]);
    let total_sectors_32 = u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);
    let fat_size_32 = u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]);
    let root_cluster = u32::from_le_bytes([buf[44], buf[45], buf[46], buf[47]]);
    let fsinfo_sector = u16::from_le_bytes([buf[48], buf[49]]);

    let total_sectors = if total_sectors_16 != 0 {
        total_sectors_16 as u32
    } else {
        total_sectors_32
    };

    let (fat_type, fat_size) = if fat_size_16 != 0 && root_entry_count != 0 {
        (FatType::Fat16, fat_size_16 as u32)
    } else {
        (FatType::Fat32, fat_size_32)
    };

    Ok(Bpb {
        bytes_per_sector,
        sectors_per_cluster,
        reserved_sectors,
        num_fats,
        root_entry_count,
        total_sectors,
        fat_size,
        root_cluster,
        fsinfo_sector,
        fat_type,
    })
}

/// FSInfo セクタをパースする
///
/// 仕様:
/// - 先頭シグネチャ: 0x41615252
/// - 構造体シグネチャ: 0x61417272 (offset 0x1E4)
/// - トレーラ: 0xAA550000 (offset 0x1FC)
/// - free_cluster_count / next_free_cluster が 0xFFFFFFFF の場合は未設定
pub fn parse_fsinfo(buf: &[u8]) -> Option<FsInfo> {
    if buf.len() < 512 {
        return None;
    }
    let lead = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let struct_sig = u32::from_le_bytes([buf[0x1E4], buf[0x1E5], buf[0x1E6], buf[0x1E7]]);
    let trail = u32::from_le_bytes([buf[0x1FC], buf[0x1FD], buf[0x1FE], buf[0x1FF]]);
    if lead != 0x41615252 || struct_sig != 0x61417272 || trail != 0xAA550000 {
        return None;
    }

    let free_raw = u32::from_le_bytes([buf[0x1E8], buf[0x1E9], buf[0x1EA], buf[0x1EB]]);
    let next_raw = u32::from_le_bytes([buf[0x1EC], buf[0x1ED], buf[0x1EE], buf[0x1EF]]);
    let free_cluster_count = if free_raw == 0xFFFFFFFF { None } else { Some(free_raw) };
    let next_free_cluster = if next_raw == 0xFFFFFFFF { None } else { Some(next_raw) };

    Some(FsInfo {
        free_cluster_count,
        next_free_cluster,
    })
}

/// FSInfo セクタに書き込む
pub fn write_fsinfo(buf: &mut [u8], info: FsInfo) {
    if buf.len() < 512 {
        return;
    }
    let lead = 0x41615252u32.to_le_bytes();
    let struct_sig = 0x61417272u32.to_le_bytes();
    let trail = 0xAA550000u32.to_le_bytes();
    buf[0..4].copy_from_slice(&lead);
    buf[0x1E4..0x1E8].copy_from_slice(&struct_sig);
    buf[0x1FC..0x200].copy_from_slice(&trail);

    let free_raw = info.free_cluster_count.unwrap_or(0xFFFFFFFF).to_le_bytes();
    let next_raw = info.next_free_cluster.unwrap_or(0xFFFFFFFF).to_le_bytes();
    buf[0x1E8..0x1EC].copy_from_slice(&free_raw);
    buf[0x1EC..0x1F0].copy_from_slice(&next_raw);
}

/// LFN エントリの属性
pub const ATTR_LFN: u8 = 0x0F;

/// LFN チェックサム（短いファイル名 11 バイト）
pub fn lfn_checksum(short_name: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &b in short_name.iter() {
        sum = sum.rotate_right(1).wrapping_add(b);
    }
    sum
}

/// LFN エントリを UTF-16 から UTF-8 に変換する
pub fn decode_lfn_entries(entries: &[LfnPart]) -> Result<String, &'static str> {
    let mut utf16: Vec<u16> = Vec::new();
    for part in entries.iter() {
        utf16.extend_from_slice(&part.name);
    }
    // 0x0000 で終端、0xFFFF は埋め
    let mut out: Vec<u16> = Vec::new();
    for ch in utf16 {
        if ch == 0x0000 {
            break;
        }
        if ch == 0xFFFF {
            continue;
        }
        out.push(ch);
    }
    String::from_utf16(&out).map_err(|_| "invalid utf16")
}

/// LFN の 13 文字断片
#[derive(Debug, Clone)]
pub struct LfnPart {
    pub order: u8,
    pub checksum: u8,
    pub name: [u16; 13],
}

/// LFN エントリ（32 バイト）から LfnPart を作る
pub fn parse_lfn_part(entry: &[u8]) -> Result<LfnPart, &'static str> {
    if entry.len() < 32 {
        return Err("entry too small");
    }
    let order = entry[0];
    let checksum = entry[13];

    let mut name = [0u16; 13];
    let mut idx = 0usize;
    for &off in &[1usize, 3, 5, 7, 9] {
        name[idx] = u16::from_le_bytes([entry[off], entry[off + 1]]);
        idx += 1;
    }
    for &off in &[14usize, 16, 18, 20, 22, 24] {
        name[idx] = u16::from_le_bytes([entry[off], entry[off + 1]]);
        idx += 1;
    }
    for &off in &[28usize, 30] {
        name[idx] = u16::from_le_bytes([entry[off], entry[off + 1]]);
        idx += 1;
    }

    Ok(LfnPart { order, checksum, name })
}

/// 8.3 形式の短い名前を生成する（簡易）
///
/// - 非対応文字は '_' に置き換える
/// - 必要なら ~1 形式を付ける
pub fn make_short_name(long: &str, exists: &dyn Fn(&[u8; 11]) -> bool) -> [u8; 11] {
    let mut name = [b' '; 11];
    let mut base = String::new();
    let mut ext = String::new();

    if let Some(pos) = long.rfind('.') {
        base.push_str(&long[..pos]);
        ext.push_str(&long[pos + 1..]);
    } else {
        base.push_str(long);
    }

    let mut base_bytes: Vec<u8> = base
        .chars()
        .filter(|c| *c != ' ')
        .map(|c| c.to_ascii_uppercase() as u8)
        .map(|c| if is_valid_short_char(c) { c } else { b'_' })
        .collect();
    let mut ext_bytes: Vec<u8> = ext
        .chars()
        .map(|c| c.to_ascii_uppercase() as u8)
        .map(|c| if is_valid_short_char(c) { c } else { b'_' })
        .collect();

    if ext_bytes.len() > 3 {
        ext_bytes.truncate(3);
    }

    let mut suffix = 0u8;
    loop {
        let mut temp = [b' '; 11];
        if suffix == 0 {
            if base_bytes.len() > 8 {
                base_bytes.truncate(8);
            }
            for (i, b) in base_bytes.iter().take(8).enumerate() {
                temp[i] = *b;
            }
        } else {
            let mut b = base_bytes.clone();
            if b.len() > 6 {
                b.truncate(6);
            }
            for (i, ch) in b.iter().enumerate() {
                temp[i] = *ch;
            }
            temp[6] = b'~';
            temp[7] = b'0' + suffix;
        }
        for (i, b) in ext_bytes.iter().take(3).enumerate() {
            temp[8 + i] = *b;
        }

        if !exists(&temp) {
            name = temp;
            break;
        }
        if suffix == 0 {
            suffix = 1;
        } else if suffix < 9 {
            suffix += 1;
        } else {
            break;
        }
    }

    name
}

fn is_valid_short_char(c: u8) -> bool {
    matches!(c,
        b'A'..=b'Z' |
        b'0'..=b'9' |
        b'$' | b'%' | b'\'' | b'-' | b'_' | b'@' | b'~' | b'!' | b'(' | b')' | b'{' | b'}' | b'^' | b'#' | b'&'
    )
}
