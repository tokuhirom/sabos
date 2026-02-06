// ed.rs — SABOS コンソール簡易エディタ（ed 風）
//
// 行指向の最小エディタ。フルスクリーンやカーソル移動は行わない。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[allow(dead_code)]
#[path = "../fat32.rs"]
mod fat32;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use alloc::string::String;
use alloc::vec::Vec;
use core::panic::PanicInfo;
use fat32::Fat32;

/// 行バッファの最大サイズ（シェルと同等）
const LINE_BUFFER_SIZE: usize = 256;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    ed_main();
}

fn ed_main() -> ! {
    syscall::write_str("SABOS ed (line editor)\n");
    syscall::write_str("File (blank for new): ");

    let mut line_buf = [0u8; LINE_BUFFER_SIZE];
    let name_len = read_line(&mut line_buf);
    let mut file_name = if name_len == 0 {
        String::new()
    } else {
        let s = core::str::from_utf8(&line_buf[..name_len]).unwrap_or("").trim();
        String::from(s)
    };

    let mut lines: Vec<String> = Vec::new();
    if !file_name.is_empty() {
        match load_file(&file_name) {
            Ok(v) => lines = v,
            Err(msg) => {
                syscall::write_str(msg);
                syscall::write_str("\n");
            }
        }
    }

    loop {
        syscall::write_str("ed> ");
        let len = read_line(&mut line_buf);
        if len == 0 {
            continue;
        }

        let line = match core::str::from_utf8(&line_buf[..len]) {
            Ok(s) => s.trim(),
            Err(_) => {
                syscall::write_str("Error: invalid UTF-8\n");
                continue;
            }
        };

        if line.is_empty() {
            continue;
        }

        let (cmd, args) = split_command(line);
        match cmd {
            "p" => cmd_print(&lines, false),
            "n" => cmd_print(&lines, true),
            "a" => cmd_append(&mut lines),
            "i" => cmd_insert(&mut lines, args),
            "d" => cmd_delete(&mut lines, args),
            "w" => {
                let _ = cmd_write(&lines, &mut file_name, &mut line_buf);
            }
            "q" => break,
            "wq" => {
                if cmd_write(&lines, &mut file_name, &mut line_buf) {
                    break;
                }
            }
            _ => {
                syscall::write_str("Error: unknown command\n");
                syscall::write_str("Commands: p n a i d w q wq\n");
            }
        }
    }

    syscall::exit();
}

/// 改行まで1行を読み取る（エコーバックあり）
fn read_line(buf: &mut [u8]) -> usize {
    let mut len = 0;
    loop {
        let c = syscall::read_char();
        match c {
            '\n' | '\r' => {
                syscall::write_str("\n");
                return len;
            }
            '\x08' | '\x7f' => {
                if len > 0 {
                    len -= 1;
                    syscall::write_str("\x08 \x08");
                }
            }
            c if c.is_ascii() && !c.is_ascii_control() => {
                if len < buf.len() {
                    buf[len] = c as u8;
                    len += 1;
                    syscall::write(&[c as u8]);
                }
            }
            _ => {}
        }
    }
}

/// コマンド文字列をコマンド名と引数に分割
fn split_command(line: &str) -> (&str, &str) {
    match line.find(' ') {
        Some(pos) => (&line[..pos], line[pos + 1..].trim_start()),
        None => (line, ""),
    }
}

/// ファイルを読み込んで行配列に変換
fn load_file(path: &str) -> Result<Vec<String>, &'static str> {
    let handle = syscall::open(path, syscall::HANDLE_RIGHTS_FILE_READ)
        .map_err(|_| "Error: failed to open file")?;
    let data = match read_all_handle(&handle) {
        Ok(v) => v,
        Err(_) => {
            let _ = syscall::handle_close(&handle);
            return Err("Error: failed to read file");
        }
    };
    let _ = syscall::handle_close(&handle);
    let text = core::str::from_utf8(&data).map_err(|_| "Error: invalid UTF-8")?;
    Ok(split_lines(text))
}

/// 文字列を行に分割（末尾の空行も保持する）
fn split_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i <= bytes.len() {
        let at_end = i == bytes.len();
        let is_nl = !at_end && bytes[i] == b'\n';
        if at_end || is_nl {
            let mut line = &text[start..i];
            if let Some(b'\r') = line.as_bytes().last().copied() {
                line = &line[..line.len().saturating_sub(1)];
            }
            out.push(String::from(line));
            start = i + 1;
        }
        i += 1;
    }
    if bytes.is_empty() {
        out.clear();
    }
    if text.ends_with('\n') && !out.is_empty() {
        if let Some(last) = out.last() {
            if last.is_empty() {
                out.pop();
            }
        }
    }
    out
}

fn cmd_print(lines: &[String], with_number: bool) {
    for (i, line) in lines.iter().enumerate() {
        if with_number {
            write_number((i + 1) as u64);
            syscall::write_str("  ");
        }
        syscall::write_str(line.as_str());
        syscall::write_str("\n");
    }
}

fn cmd_append(lines: &mut Vec<String>) {
    let mut buf = [0u8; LINE_BUFFER_SIZE];
    loop {
        let len = read_line(&mut buf);
        let s = core::str::from_utf8(&buf[..len]).unwrap_or("");
        if s == "." {
            break;
        }
        lines.push(String::from(s));
    }
}

fn cmd_insert(lines: &mut Vec<String>, args: &str) {
    let num = match parse_u64(args.trim()) {
        Some(v) => v as usize,
        None => {
            syscall::write_str("Usage: i <line_number>\n");
            return;
        }
    };
    if num == 0 || num > lines.len() + 1 {
        syscall::write_str("Error: line number out of range\n");
        return;
    }

    let mut buf = [0u8; LINE_BUFFER_SIZE];
    let mut insert_at = num - 1;
    loop {
        let len = read_line(&mut buf);
        let s = core::str::from_utf8(&buf[..len]).unwrap_or("");
        if s == "." {
            break;
        }
        lines.insert(insert_at, String::from(s));
        insert_at += 1;
    }
}

fn cmd_delete(lines: &mut Vec<String>, args: &str) {
    let num = match parse_u64(args.trim()) {
        Some(v) => v as usize,
        None => {
            syscall::write_str("Usage: d <line_number>\n");
            return;
        }
    };
    if num == 0 || num > lines.len() {
        syscall::write_str("Error: line number out of range\n");
        return;
    }
    lines.remove(num - 1);
}

fn cmd_write(lines: &[String], file_name: &mut String, buf: &mut [u8]) -> bool {
    if file_name.is_empty() {
        syscall::write_str("File: ");
        let len = read_line(buf);
        let s = core::str::from_utf8(&buf[..len]).unwrap_or("").trim();
        if s.is_empty() {
            syscall::write_str("Error: no file name\n");
            return false;
        }
        *file_name = String::from(s);
    }
    let target = file_name.as_str();

    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.as_str());
    }
    if !out.is_empty() {
        out.push('\n');
    }

    let mut fs = match Fat32::new() {
        Ok(v) => v,
        Err(_) => {
            syscall::write_str("Error: FAT32 not available\n");
            return false;
        }
    };
    if fs.create_file(target, out.as_bytes()).is_err() {
        syscall::write_str("Error: failed to write file\n");
        return false;
    }
    syscall::write_str("Wrote file\n");
    true
}

fn read_all_handle(handle: &syscall::Handle) -> Result<Vec<u8>, syscall::SyscallResult> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = syscall::handle_read(handle, &mut buf);
        if n < 0 {
            return Err(n);
        }
        if n == 0 {
            break;
        }
        let len = n as usize;
        out.extend_from_slice(&buf[..len]);
    }
    Ok(out)
}

fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut n = 0u64;
    for c in s.chars() {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n * 10 + (c as u8 - b'0') as u64;
    }
    Some(n)
}

fn write_number(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = 0;
    if n == 0 {
        buf[0] = b'0';
        i = 1;
    } else {
        while n > 0 {
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i += 1;
        }
    }
    for j in (0..i).rev() {
        syscall::write(&[buf[j]]);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
