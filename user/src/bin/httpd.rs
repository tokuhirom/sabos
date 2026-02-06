// httpd.rs — SABOS 簡易 HTTP サーバー（user space）
//
// HTTP/1.1 だが keep-alive は使わず、1リクエストで接続を閉じる。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../syscall.rs"]
mod syscall;
#[path = "../json.rs"]
mod json;

use alloc::string::String;
use alloc::vec::Vec;
use core::panic::PanicInfo;

const HTTP_PORT: u16 = 8080;
const IPC_BUF_SIZE: usize = 2048;
const FILE_BUFFER_SIZE: usize = 4096;
const MAX_REQUEST_SIZE: usize = 4096;

// netd IPC
const NETD_OPCODE_TCP_SEND: u32 = 3;
const NETD_OPCODE_TCP_RECV: u32 = 4;
const NETD_OPCODE_TCP_CLOSE: u32 = 5;
const NETD_OPCODE_TCP_LISTEN: u32 = 6;
const NETD_OPCODE_TCP_ACCEPT: u32 = 7;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    httpd_main();
}

fn httpd_main() -> ! {
    let mut netd_id = resolve_task_id_by_name("NETD.ELF").unwrap_or(0);

    loop {
        if netd_id == 0 {
            netd_id = resolve_task_id_by_name("NETD.ELF").unwrap_or(0);
            if netd_id == 0 {
                syscall::sleep(500);
                continue;
            }
        }

        if netd_tcp_listen(netd_id, HTTP_PORT).is_err() {
            syscall::sleep(500);
            continue;
        }

        loop {
            match netd_tcp_accept(netd_id, 0) {
                Ok(conn_id) => {
                    handle_connection(netd_id, conn_id);
                }
                Err(_) => {
                    syscall::sleep(10);
                }
            }
        }
    }
}

fn handle_connection(netd_id: u64, conn_id: u32) {
    let req = match read_http_request(netd_id, conn_id) {
        Ok(v) => v,
        Err(_) => {
            send_simple_response(netd_id, conn_id, 400, "Bad Request", "bad request\n");
            let _ = netd_tcp_close(netd_id, conn_id);
            return;
        }
    };

    let req_text = match core::str::from_utf8(&req) {
        Ok(v) => v,
        Err(_) => {
            send_simple_response(netd_id, conn_id, 400, "Bad Request", "bad request\n");
            let _ = netd_tcp_close(netd_id, conn_id);
            return;
        }
    };

    let mut lines = req_text.split("\r\n");
    let first = match lines.next() {
        Some(v) => v,
        None => {
            send_simple_response(netd_id, conn_id, 400, "Bad Request", "bad request\n");
            let _ = netd_tcp_close(netd_id, conn_id);
            return;
        }
    };

    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");

    if method != "GET" {
        send_simple_response(netd_id, conn_id, 405, "Method Not Allowed", "method not allowed\n");
        let _ = netd_tcp_close(netd_id, conn_id);
        return;
    }

    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        send_simple_response(netd_id, conn_id, 400, "Bad Request", "bad request\n");
        let _ = netd_tcp_close(netd_id, conn_id);
        return;
    }

    if !path.starts_with('/') || path.contains("..") {
        send_simple_response(netd_id, conn_id, 400, "Bad Request", "bad request\n");
        let _ = netd_tcp_close(netd_id, conn_id);
        return;
    }

    // パスが "/" で終わるか "/" そのものならディレクトリとして扱う
    if path == "/" || path.ends_with('/') {
        let dir_path = if path == "/" { "/" } else { &path[..path.len() - 1] };
        match list_directory(dir_path, path) {
            Ok(html) => {
                send_html_response(netd_id, conn_id, 200, "OK", &html);
            }
            Err(_) => {
                send_simple_response(netd_id, conn_id, 404, "Not Found", "not found\n");
            }
        }
        let _ = netd_tcp_close(netd_id, conn_id);
        return;
    }

    // まずファイルとして開いてみる
    let data = match read_file(path) {
        Ok(v) => v,
        Err(_) => {
            // ファイルが見つからなければディレクトリとして試す
            match list_directory(path, path) {
                Ok(html) => {
                    send_html_response(netd_id, conn_id, 200, "OK", &html);
                    let _ = netd_tcp_close(netd_id, conn_id);
                    return;
                }
                Err(_) => {
                    send_simple_response(netd_id, conn_id, 404, "Not Found", "not found\n");
                    let _ = netd_tcp_close(netd_id, conn_id);
                    return;
                }
            }
        }
    };

    let content_type = guess_content_type(path);

    let mut header = String::new();
    header.push_str("HTTP/1.1 200 OK\r\n");
    header.push_str("Content-Type: ");
    header.push_str(content_type);
    header.push_str("\r\n");
    header.push_str("Content-Length: ");
    header.push_str(&itoa(data.len() as u64));
    header.push_str("\r\n");
    header.push_str("Connection: close\r\n\r\n");

    let _ = netd_tcp_send_all(netd_id, conn_id, header.as_bytes());
    let _ = netd_tcp_send_all(netd_id, conn_id, &data);
    let _ = netd_tcp_close(netd_id, conn_id);
}

fn read_http_request(netd_id: u64, conn_id: u32) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 256];
    loop {
        let n = netd_tcp_recv(netd_id, conn_id, &mut tmp, 5000)?;
        if n == 0 {
            return Err(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_REQUEST_SIZE {
            return Err(());
        }
    }
    Ok(buf)
}

fn read_file(path: &str) -> Result<Vec<u8>, ()> {
    let handle = syscall::open(path, syscall::HANDLE_RIGHTS_FILE_READ).map_err(|_| ())?;
    let mut out = Vec::new();
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    loop {
        let n = syscall::handle_read(&handle, &mut buf);
        if n < 0 {
            let _ = syscall::handle_close(&handle);
            return Err(());
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    let _ = syscall::handle_close(&handle);
    Ok(out)
}

/// ディレクトリの内容を HTML で返す
///
/// handle_enum で取得したエントリ名（改行区切り）を HTML のリンク一覧に変換する。
/// 各エントリはクリックでアクセスできるリンクになる。
fn list_directory(dir_path: &str, display_path: &str) -> Result<String, ()> {
    let handle = syscall::open(dir_path, syscall::HANDLE_RIGHTS_DIRECTORY_READ).map_err(|_| ())?;
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let n = syscall::handle_enum(&handle, &mut buf);
    let _ = syscall::handle_close(&handle);
    if n < 0 {
        return Err(());
    }

    let entries_text = if n > 0 {
        core::str::from_utf8(&buf[..n as usize]).map_err(|_| ())?
    } else {
        ""
    };

    // HTML を構築
    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\n");
    html.push_str("<title>Index of ");
    html.push_str(display_path);
    html.push_str("</title>\n");
    // 簡単なスタイル
    html.push_str("<style>body{font-family:monospace;margin:2em}a{text-decoration:none}a:hover{text-decoration:underline}li{margin:0.3em 0}</style>\n");
    html.push_str("</head><body>\n");
    html.push_str("<h1>Index of ");
    html.push_str(display_path);
    html.push_str("</h1>\n<hr>\n<ul>\n");

    // 親ディレクトリへのリンク（ルート以外）
    if dir_path != "/" {
        html.push_str("<li><a href=\"");
        // 親パスを計算
        let parent = parent_path(display_path);
        html.push_str(&parent);
        html.push_str("\">..</a></li>\n");
    }

    // エントリをリンクとして追加
    for entry in entries_text.split('\n') {
        let name = entry.trim();
        if name.is_empty() {
            continue;
        }
        html.push_str("<li><a href=\"");
        // リンク先のパスを構築
        if display_path.ends_with('/') {
            html.push_str(display_path);
        } else {
            html.push_str(display_path);
            html.push('/');
        }
        html.push_str(name);
        html.push_str("\">");
        html.push_str(name);
        html.push_str("</a></li>\n");
    }

    html.push_str("</ul>\n<hr>\n<p><em>SABOS httpd</em></p>\n</body></html>\n");
    Ok(html)
}

/// 親パスを返す（"/" なら "/" のまま）
fn parent_path(path: &str) -> String {
    // 末尾の "/" を除去してから最後の "/" を探す
    let trimmed = if path.ends_with('/') && path.len() > 1 {
        &path[..path.len() - 1]
    } else {
        path
    };
    match trimmed.rfind('/') {
        Some(0) => String::from("/"),
        Some(pos) => String::from(&trimmed[..pos + 1]),
        None => String::from("/"),
    }
}

/// Content-Type を推測する
fn guess_content_type(path: &str) -> &'static str {
    if path.ends_with(".HTML") || path.ends_with(".HTM")
        || path.ends_with(".html") || path.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".JSON") || path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".ELF") || path.ends_with(".elf") {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// HTML レスポンスを送信する
fn send_html_response(netd_id: u64, conn_id: u32, code: u32, reason: &str, body: &str) {
    let mut header = String::new();
    header.push_str("HTTP/1.1 ");
    header.push_str(&itoa(code as u64));
    header.push(' ');
    header.push_str(reason);
    header.push_str("\r\n");
    header.push_str("Content-Type: text/html; charset=utf-8\r\n");
    header.push_str("Content-Length: ");
    header.push_str(&itoa(body.as_bytes().len() as u64));
    header.push_str("\r\n");
    header.push_str("Connection: close\r\n\r\n");

    let _ = netd_tcp_send_all(netd_id, conn_id, header.as_bytes());
    let _ = netd_tcp_send_all(netd_id, conn_id, body.as_bytes());
}

fn send_simple_response(netd_id: u64, conn_id: u32, code: u32, reason: &str, body: &str) {
    let mut header = String::new();
    header.push_str("HTTP/1.1 ");
    header.push_str(&itoa(code as u64));
    header.push(' ');
    header.push_str(reason);
    header.push_str("\r\n");
    header.push_str("Content-Type: text/plain\r\n");
    header.push_str("Content-Length: ");
    header.push_str(&itoa(body.as_bytes().len() as u64));
    header.push_str("\r\n");
    header.push_str("Connection: close\r\n\r\n");

    let _ = netd_tcp_send_all(netd_id, conn_id, header.as_bytes());
    let _ = netd_tcp_send_all(netd_id, conn_id, body.as_bytes());
}

fn netd_tcp_listen(netd_id: u64, port: u16) -> Result<(), ()> {
    let payload = port.to_le_bytes();
    let (status, _) = netd_request(netd_id, NETD_OPCODE_TCP_LISTEN, &payload, &mut [0u8; 32])?;
    if status < 0 { Err(()) } else { Ok(()) }
}

fn netd_tcp_accept(netd_id: u64, timeout_ms: u64) -> Result<u32, ()> {
    let payload = timeout_ms.to_le_bytes();
    let mut resp = [0u8; 32];
    let (status, len) = netd_request(netd_id, NETD_OPCODE_TCP_ACCEPT, &payload, &mut resp)?;
    if status < 0 || len != 4 { Err(()) } else { Ok(u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]])) }
}

fn netd_tcp_send(netd_id: u64, conn_id: u32, data: &[u8]) -> Result<(), ()> {
    let mut payload = [0u8; IPC_BUF_SIZE];
    if 4 + data.len() > payload.len() {
        return Err(());
    }
    payload[0..4].copy_from_slice(&conn_id.to_le_bytes());
    payload[4..4 + data.len()].copy_from_slice(data);
    let (status, _) = netd_request(netd_id, NETD_OPCODE_TCP_SEND, &payload[..4 + data.len()], &mut [0u8; 32])?;
    if status < 0 { Err(()) } else { Ok(()) }
}

fn netd_tcp_send_all(netd_id: u64, conn_id: u32, data: &[u8]) -> Result<(), ()> {
    let mut offset = 0usize;
    while offset < data.len() {
        let end = core::cmp::min(offset + 1024, data.len());
        netd_tcp_send(netd_id, conn_id, &data[offset..end])?;
        offset = end;
    }
    Ok(())
}

fn netd_tcp_recv(netd_id: u64, conn_id: u32, buf: &mut [u8], timeout_ms: u64) -> Result<usize, ()> {
    let mut payload = [0u8; 16];
    payload[0..4].copy_from_slice(&conn_id.to_le_bytes());
    payload[4..8].copy_from_slice(&(buf.len() as u32).to_le_bytes());
    payload[8..16].copy_from_slice(&timeout_ms.to_le_bytes());

    let (status, len) = netd_request(netd_id, NETD_OPCODE_TCP_RECV, &payload, buf)?;
    if status == -42 {
        return Ok(0);
    }
    if status < 0 {
        return Err(());
    }
    Ok(len)
}

fn netd_tcp_close(netd_id: u64, conn_id: u32) -> Result<(), ()> {
    let payload = conn_id.to_le_bytes();
    let (status, _) = netd_request(netd_id, NETD_OPCODE_TCP_CLOSE, &payload, &mut [0u8; 32])?;
    if status < 0 { Err(()) } else { Ok(()) }
}

fn netd_request(
    netd_id: u64,
    opcode: u32,
    payload: &[u8],
    resp_buf: &mut [u8],
) -> Result<(i32, usize), ()> {
    if 8 + payload.len() > IPC_BUF_SIZE {
        return Err(());
    }
    let mut req = [0u8; IPC_BUF_SIZE];
    req[0..4].copy_from_slice(&opcode.to_le_bytes());
    req[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    req[8..8 + payload.len()].copy_from_slice(payload);

    if syscall::ipc_send(netd_id, &req[..8 + payload.len()]) < 0 {
        return Err(());
    }

    let mut sender = 0u64;
    let n = syscall::ipc_recv(&mut sender, resp_buf, 5000);
    if n < 0 {
        return Err(());
    }
    let n = n as usize;
    if n < 12 {
        return Err(());
    }

    let resp_opcode = u32::from_le_bytes([resp_buf[0], resp_buf[1], resp_buf[2], resp_buf[3]]);
    if resp_opcode != opcode {
        return Err(());
    }
    let status = i32::from_le_bytes([resp_buf[4], resp_buf[5], resp_buf[6], resp_buf[7]]);
    let len = u32::from_le_bytes([resp_buf[8], resp_buf[9], resp_buf[10], resp_buf[11]]) as usize;
    if 12 + len > n {
        return Err(());
    }
    resp_buf.copy_within(12..12 + len, 0);
    Ok((status, len))
}

/// タスク一覧から指定名のタスク ID を探す
fn resolve_task_id_by_name(name: &str) -> Option<u64> {
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::get_task_list(&mut buf);
    if result < 0 {
        return None;
    }
    let len = result as usize;
    let Ok(s) = core::str::from_utf8(&buf[..len]) else {
        return None;
    };

    let (tasks_start, tasks_end) = json::json_find_array_bounds(s, "tasks")?;
    let mut i = tasks_start;
    let bytes = s.as_bytes();
    while i < tasks_end {
        while i < tasks_end && bytes[i] != b'{' && bytes[i] != b']' {
            i += 1;
        }
        if i >= tasks_end || bytes[i] == b']' {
            break;
        }

        let obj_end = json::find_matching_brace(s, i)?;
        if obj_end > tasks_end {
            break;
        }

        let obj = &s[i + 1..obj_end];
        let id = json::json_find_u64(obj, "id");
        let task_name = json::json_find_str(obj, "name");
        if let (Some(id), Some(task_name)) = (id, task_name) {
            if task_name == name {
                return Some(id);
            }
        }
        i = obj_end + 1;
    }
    None
}

fn itoa(mut n: u64) -> String {
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
    let mut s = String::new();
    for j in (0..i).rev() {
        s.push(buf[j] as char);
    }
    s
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
