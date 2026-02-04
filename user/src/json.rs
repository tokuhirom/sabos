// json.rs — JSON パーサ（最小実装・共通部品）
//
// procfs の JSON を読むための最小パーサ。
// 完全な JSON 仕様には対応しないが、SABOS の出力形式には十分。

#![allow(dead_code)]

/// JSON のキーに対応する値の開始位置を返す
pub fn json_find_key_value_start(s: &str, key: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let key_bytes = key.as_bytes();
    let mut i = 0;
    while i + key_bytes.len() + 2 <= bytes.len() {
        if bytes[i] == b'"'
            && bytes[i + 1..i + 1 + key_bytes.len()] == *key_bytes
            && bytes[i + 1 + key_bytes.len()] == b'"'
        {
            let mut j = i + 1 + key_bytes.len() + 1;
            // 空白をスキップ
            while j < bytes.len() && is_json_space(bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b':' {
                j += 1;
                while j < bytes.len() && is_json_space(bytes[j]) {
                    j += 1;
                }
                return Some(j);
            }
        }
        i += 1;
    }
    None
}

/// JSON から数値を取り出す
pub fn json_find_u64(s: &str, key: &str) -> Option<u64> {
    let start = json_find_key_value_start(s, key)?;
    let tail = &s[start..];
    parse_u64_prefix(tail)
}

/// JSON から文字列を取り出す（エスケープは展開しない）
pub fn json_find_str<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let start = json_find_key_value_start(s, key)?;
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut i = start + 1;
    let mut escape = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if b == b'\\' {
            escape = true;
            i += 1;
            continue;
        }
        if b == b'"' {
            return Some(&s[start + 1..i]);
        }
        i += 1;
    }
    None
}

/// JSON 配列の範囲を取得する
pub fn json_find_array_bounds(s: &str, key: &str) -> Option<(usize, usize)> {
    let start = json_find_key_value_start(s, key)?;
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let end = find_matching_delim(s, start, b'[', b']')?;
    Some((start + 1, end))
}

/// { ... } の対応する } を探す
pub fn find_matching_brace(s: &str, start: usize) -> Option<usize> {
    find_matching_delim(s, start, b'{', b'}')
}

/// 対応する閉じ括弧を探す（最小実装）
fn find_matching_delim(s: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&open) {
        return None;
    }
    let mut depth = 1usize;
    let mut i = start + 1;
    let mut in_string = false;
    let mut escape = false;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if b == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// JSON の空白判定
fn is_json_space(b: u8) -> bool {
    b == b' ' || b == b'\n' || b == b'\r' || b == b'\t'
}

/// 文字列先頭の数値を u64 にパース
fn parse_u64_prefix(s: &str) -> Option<u64> {
    let mut result: u64 = 0;
    let mut found = false;
    for b in s.bytes() {
        if b < b'0' || b > b'9' {
            break;
        }
        found = true;
        result = result.checked_mul(10)?;
        result = result.checked_add((b - b'0') as u64)?;
    }
    if found { Some(result) } else { None }
}
