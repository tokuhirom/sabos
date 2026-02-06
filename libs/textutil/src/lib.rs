#![no_std]

extern crate alloc;

use alloc::string::String;

/// リテラル文字列の置換を行う（正規表現は使わない）
///
/// - `global=false`: 最初の一致のみ置換
/// - `global=true`: すべての一致を置換
/// - 戻り値は (置換後文字列, 置換が発生したか)
/// リテラル文字列が行内に含まれるかを判定する
///
/// - `case_insensitive=true`: 大文字小文字を区別しない
/// - 戻り値は一致したかどうかの bool
pub fn contains_literal(line: &str, pattern: &str, case_insensitive: bool) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if case_insensitive {
        // ASCII 範囲の大文字小文字を無視して検索する
        // no_std 環境なので to_lowercase() が使えないため、バイト単位で比較
        let line_bytes = line.as_bytes();
        let pat_bytes = pattern.as_bytes();
        if pat_bytes.len() > line_bytes.len() {
            return false;
        }
        for i in 0..=(line_bytes.len() - pat_bytes.len()) {
            let mut matched = true;
            for j in 0..pat_bytes.len() {
                let a = ascii_lower(line_bytes[i + j]);
                let b = ascii_lower(pat_bytes[j]);
                if a != b {
                    matched = false;
                    break;
                }
            }
            if matched {
                return true;
            }
        }
        false
    } else {
        line.find(pattern).is_some()
    }
}

/// ASCII 範囲の大文字を小文字に変換する
fn ascii_lower(b: u8) -> u8 {
    if b >= b'A' && b <= b'Z' {
        b + 32
    } else {
        b
    }
}

pub fn replace_literal(line: &str, from: &str, to: &str, global: bool) -> (String, bool) {
    if from.is_empty() {
        return (String::from(line), false);
    }

    let mut rest = line;
    let mut out = String::new();
    let mut changed = false;

    while let Some(pos) = rest.find(from) {
        changed = true;
        out.push_str(&rest[..pos]);
        out.push_str(to);
        rest = &rest[pos + from.len()..];
        if !global {
            out.push_str(rest);
            return (out, true);
        }
    }

    if changed {
        out.push_str(rest);
        (out, true)
    } else {
        (String::from(line), false)
    }
}
