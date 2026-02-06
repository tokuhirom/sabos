// telnetd.rs — Telnet サーバー（user space）
//
// 単一接続のみ対応。接続が来たら別のシェルプロセス (TSH.ELF) を起動し、
// TCP <-> IPC で入出力を中継する。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../json.rs"]
mod json;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use alloc::vec::Vec;
use core::panic::PanicInfo;

const TELNET_PORT: u16 = 2323;

// telnetd <-> tsh IPC
const OPCODE_INIT: u32 = 1;
const OPCODE_INPUT: u32 = 2;
const OPCODE_OUTPUT: u32 = 3;

// netd IPC
const NETD_OPCODE_TCP_SEND: u32 = 3;
const NETD_OPCODE_TCP_RECV: u32 = 4;
const NETD_OPCODE_TCP_CLOSE: u32 = 5;
const NETD_OPCODE_TCP_LISTEN: u32 = 6;
const NETD_OPCODE_TCP_ACCEPT: u32 = 7;

const IPC_BUF_SIZE: usize = 2048;
const FILE_BUFFER_SIZE: usize = 2048;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    telnetd_main();
}

fn telnetd_main() -> ! {
    let my_id = syscall::getpid();
    let mut netd_id = resolve_task_id_by_name("NETD.ELF").unwrap_or(0);

    loop {
        if netd_id == 0 {
            netd_id = resolve_task_id_by_name("NETD.ELF").unwrap_or(0);
            if netd_id == 0 {
                syscall::sleep(500);
                continue;
            }
        }

        if netd_tcp_listen(netd_id, TELNET_PORT).is_err() {
            syscall::sleep(500);
            continue;
        }

        let mut sessions: Vec<Session> = Vec::new();
        let mut tcp_buf = [0u8; 256];
        let mut ipc_buf = [0u8; IPC_BUF_SIZE];

        loop {
            // 新規接続の accept
            if let Ok(conn_id) = netd_tcp_accept(netd_id, 0) {
                if let Some(session) = start_session(netd_id, my_id, conn_id) {
                    sessions.push(session);
                } else {
                    let _ = netd_tcp_close(netd_id, conn_id);
                }
            }

            // tsh からの出力を処理
            let mut sender = 0u64;
            let n = syscall::ipc_recv(&mut sender, &mut ipc_buf, 0);
            if n > 0 {
                if let Some(pos) = sessions.iter().position(|s| s.tsh_id == sender) {
                    let conn_id = sessions[pos].conn_id;
                    if handle_tsh_output(&ipc_buf[..n as usize], netd_id, conn_id).is_err() {
                        close_session(netd_id, sessions.remove(pos));
                    }
                }
            }

            // TCP 受信を各セッションで処理
            let mut i = 0usize;
            while i < sessions.len() {
                let conn_id = sessions[i].conn_id;
                match netd_tcp_recv(netd_id, conn_id, &mut tcp_buf, 0) {
                    Ok(0) => {
                        i += 1;
                    }
                    Ok(n) => {
                        handle_tcp_input(&mut sessions[i], netd_id, &tcp_buf[..n]);
                        i += 1;
                    }
                    Err(_) => {
                        let session = sessions.remove(i);
                        close_session(netd_id, session);
                    }
                }
            }

            syscall::sleep(1);
        }
    }
}

struct Session {
    conn_id: u32,
    tsh_id: u64,
    line_buf: [u8; 512],
    line_len: usize,
    telnet_skip: u8,
}

fn start_session(netd_id: u64, my_id: u64, conn_id: u32) -> Option<Session> {
    let tsh_id = syscall::spawn("/TSH.ELF");
    if tsh_id < 0 {
        return None;
    }
    let tsh_id = tsh_id as u64;

    let mut init_msg = [0u8; 16];
    init_msg[0..4].copy_from_slice(&OPCODE_INIT.to_le_bytes());
    init_msg[4..8].copy_from_slice(&8u32.to_le_bytes());
    init_msg[8..16].copy_from_slice(&my_id.to_le_bytes());
    let _ = syscall::ipc_send(tsh_id, &init_msg);

    let _ = netd_tcp_send(netd_id, conn_id, b"Welcome to SABOS telnetd\r\n");

    Some(Session {
        conn_id,
        tsh_id,
        line_buf: [0u8; 512],
        line_len: 0,
        telnet_skip: 0,
    })
}

fn close_session(netd_id: u64, session: Session) {
    let _ = send_line_to_tsh(session.tsh_id, b"exit");
    let _ = syscall::wait(session.tsh_id, 1000);
    let _ = netd_tcp_close(netd_id, session.conn_id);
}

fn handle_tcp_input(session: &mut Session, netd_id: u64, data: &[u8]) {
    for &b in data {
        if session.telnet_skip > 0 {
            session.telnet_skip -= 1;
            continue;
        }
        if b == 0xFF {
            session.telnet_skip = 2;
            continue;
        }
        match b {
            b'\r' | b'\n' => {
                let _ = netd_tcp_send(netd_id, session.conn_id, b"\r\n");
                if session.line_len > 0 {
                    let line = &session.line_buf[..session.line_len];
                    let _ = send_line_to_tsh(session.tsh_id, line);
                    session.line_len = 0;
                } else {
                    let _ = send_line_to_tsh(session.tsh_id, b"");
                }
            }
            0x08 | 0x7F => {
                if session.line_len > 0 {
                    session.line_len -= 1;
                    let _ = netd_tcp_send(netd_id, session.conn_id, b"\x08 \x08");
                }
            }
            b if b.is_ascii() && !b.is_ascii_control() => {
                if session.line_len < session.line_buf.len() {
                    session.line_buf[session.line_len] = b;
                    session.line_len += 1;
                    let _ = netd_tcp_send(netd_id, session.conn_id, &[b]);
                }
            }
            _ => {}
        }
    }
}

fn send_line_to_tsh(tsh_id: u64, line: &[u8]) -> Result<(), ()> {
    let mut buf = [0u8; IPC_BUF_SIZE];
    if 8 + line.len() > buf.len() {
        return Err(());
    }
    buf[0..4].copy_from_slice(&OPCODE_INPUT.to_le_bytes());
    buf[4..8].copy_from_slice(&(line.len() as u32).to_le_bytes());
    buf[8..8 + line.len()].copy_from_slice(line);
    if syscall::ipc_send(tsh_id, &buf[..8 + line.len()]) < 0 {
        Err(())
    } else {
        Ok(())
    }
}

fn handle_tsh_output(msg: &[u8], netd_id: u64, conn_id: u32) -> Result<(), ()> {
    if msg.len() < 8 {
        return Ok(());
    }
    let opcode = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
    let len = u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
    if opcode != OPCODE_OUTPUT || 8 + len > msg.len() {
        return Ok(());
    }
    let data = &msg[8..8 + len];
    let _ = netd_tcp_send(netd_id, conn_id, data);
    Ok(())
}

// ================================
// netd クライアント
// ================================

fn netd_tcp_listen(netd_id: u64, port: u16) -> Result<(), ()> {
    let payload = port.to_le_bytes();
    let (status, _) = netd_request(netd_id, NETD_OPCODE_TCP_LISTEN, &payload, &mut [0u8; 32])?;
    if status < 0 { Err(()) } else { Ok(()) }
}

fn netd_tcp_accept(netd_id: u64, timeout_ms: u64) -> Result<u32, ()> {
    let payload = timeout_ms.to_le_bytes();
    let mut resp = [0u8; 32];
    let (status, len) = netd_request(netd_id, NETD_OPCODE_TCP_ACCEPT, &payload, &mut resp)?;
    if status < 0 || len != 4 {
        Err(())
    } else {
        Ok(u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]))
    }
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

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
