#![no_std]

extern crate alloc;

use alloc::string::String;

/// リテラル文字列の置換を行う（正規表現は使わない）
///
/// - `global=false`: 最初の一致のみ置換
/// - `global=true`: すべての一致を置換
/// - 戻り値は (置換後文字列, 置換が発生したか)
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
