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
#[path = "../net.rs"]
mod net;
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

const IPC_BUF_SIZE: usize = 2048;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    telnetd_main();
}

fn telnetd_main() -> ! {
    let my_id = syscall::getpid();

    // netd の初期化（見つかるまでリトライ）
    loop {
        if net::init_netd().is_ok() {
            break;
        }
        syscall::sleep(500);
    }

    loop {
        // リッスン開始
        if net::raw_listen(TELNET_PORT).is_err() {
            syscall::sleep(500);
            continue;
        }

        let mut sessions: Vec<Session> = Vec::new();
        let mut tcp_buf = [0u8; 256];
        let mut ipc_buf = [0u8; IPC_BUF_SIZE];

        loop {
            // 新規接続の accept（raw API を使用: セッション管理のため conn_id を直接操作）
            if let Ok(conn_id) = net::raw_accept(0) {
                if let Some(session) = start_session(my_id, conn_id) {
                    sessions.push(session);
                } else {
                    let _ = net::raw_close(conn_id);
                }
            }

            // tsh からの出力を処理
            let mut sender = 0u64;
            let n = syscall::ipc_recv(&mut sender, &mut ipc_buf, 0);
            if n > 0 {
                if let Some(pos) = sessions.iter().position(|s| s.tsh_id == sender) {
                    let conn_id = sessions[pos].conn_id;
                    if handle_tsh_output(&ipc_buf[..n as usize], conn_id).is_err() {
                        close_session(sessions.remove(pos));
                    }
                }
            }

            // TCP 受信を各セッションで処理
            let mut i = 0usize;
            while i < sessions.len() {
                let conn_id = sessions[i].conn_id;
                match net::raw_recv(conn_id, &mut tcp_buf, 0) {
                    Ok(0) => {
                        i += 1;
                    }
                    Ok(n) => {
                        handle_tcp_input(&mut sessions[i], &tcp_buf[..n]);
                        i += 1;
                    }
                    Err(_) => {
                        let session = sessions.remove(i);
                        close_session(session);
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

fn start_session(my_id: u64, conn_id: u32) -> Option<Session> {
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

    let _ = net::raw_send(conn_id, b"Welcome to SABOS telnetd\r\n");

    Some(Session {
        conn_id,
        tsh_id,
        line_buf: [0u8; 512],
        line_len: 0,
        telnet_skip: 0,
    })
}

fn close_session(session: Session) {
    let _ = send_line_to_tsh(session.tsh_id, b"exit");
    let _ = syscall::wait(session.tsh_id, 1000);
    let _ = net::raw_close(session.conn_id);
}

fn handle_tcp_input(session: &mut Session, data: &[u8]) {
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
                let _ = net::raw_send(session.conn_id, b"\r\n");
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
                    let _ = net::raw_send(session.conn_id, b"\x08 \x08");
                }
            }
            b if b.is_ascii() && !b.is_ascii_control() => {
                if session.line_len < session.line_buf.len() {
                    session.line_buf[session.line_len] = b;
                    session.line_len += 1;
                    let _ = net::raw_send(session.conn_id, &[b]);
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

fn handle_tsh_output(msg: &[u8], conn_id: u32) -> Result<(), ()> {
    if msg.len() < 8 {
        return Ok(());
    }
    let opcode = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
    let len = u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
    if opcode != OPCODE_OUTPUT || 8 + len > msg.len() {
        return Ok(());
    }
    let data = &msg[8..8 + len];
    let _ = net::raw_send(conn_id, data);
    Ok(())
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
