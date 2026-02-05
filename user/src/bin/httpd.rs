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

    let target = if path == "/" { "/HELLO.TXT" } else { path };

    let data = match read_file(target) {
        Ok(v) => v,
        Err(_) => {
            send_simple_response(netd_id, conn_id, 404, "Not Found", "not found\n");
            let _ = netd_tcp_close(netd_id, conn_id);
            return;
        }
    };

    let content_type = if target.ends_with(".HTML") || target.ends_with(".HTM") {
        "text/html"
    } else {
        "text/plain"
    };

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
