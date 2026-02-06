// netd.rs — ネットワークサービス (user space)
//
// IPC 経由で DNS/TCP のリクエストを受け取り、
// カーネルのネットワーク syscalls を代理で呼び出す。
//
// 第1段階は「代理サービス」だが、将来的に TCP/IP スタック本体を
// ユーザー空間へ移すための足場になる。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../netstack.rs"]
mod netstack;
#[path = "../syscall_netd.rs"]
mod syscall_netd;

use core::panic::PanicInfo;
use crate::syscall_netd as syscall;

const OPCODE_DNS_LOOKUP: u32 = 1;
const OPCODE_TCP_CONNECT: u32 = 2;
const OPCODE_TCP_SEND: u32 = 3;
const OPCODE_TCP_RECV: u32 = 4;
const OPCODE_TCP_CLOSE: u32 = 5;
const OPCODE_TCP_LISTEN: u32 = 6;
const OPCODE_TCP_ACCEPT: u32 = 7;

const IPC_BUF_SIZE: usize = 2048;
const IPC_RECV_TIMEOUT_MS: u64 = 10;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    netd_loop();
}

fn netd_loop() -> ! {
    let mut buf = [0u8; IPC_BUF_SIZE];
    let mut sender: u64 = 0;
    let mut init_ok = netstack::init().is_ok();

    loop {
        if !init_ok {
            init_ok = netstack::init().is_ok();
        }

        let n = syscall::ipc_recv(&mut sender, &mut buf, IPC_RECV_TIMEOUT_MS);
        if n < 0 {
            netstack::poll_and_handle();
            continue;
        }
        let n = n as usize;
        if n < 8 {
            netstack::poll_and_handle();
            continue;
        }

        let opcode = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if 8 + len > n {
            netstack::poll_and_handle();
            continue;
        }
        let payload = &buf[8..8 + len];

        let mut resp = [0u8; IPC_BUF_SIZE];
        let mut resp_len = 0usize;
        let mut status: i32 = 0;

        if !init_ok {
            status = -99;
        } else {
            match opcode {
            OPCODE_DNS_LOOKUP => {
                let domain = core::str::from_utf8(payload).unwrap_or("");
                match netstack::dns_lookup(domain) {
                    Ok(ip) => {
                    resp_len = 4;
                    resp[12..16].copy_from_slice(&ip);
                    }
                    Err(err) => {
                        status = map_netstack_error(err);
                    }
                }
            }
            OPCODE_TCP_CONNECT => {
                if payload.len() == 6 {
                    let ip = [payload[0], payload[1], payload[2], payload[3]];
                    let port = u16::from_le_bytes([payload[4], payload[5]]);
                    match netstack::tcp_connect(ip, port) {
                        Ok(conn_id) => {
                            resp_len = 4;
                            resp[12..16].copy_from_slice(&conn_id.to_le_bytes());
                        }
                        Err(err) => {
                            status = map_netstack_error(err);
                        }
                    }
                } else {
                    status = -1;
                }
            }
            OPCODE_TCP_SEND => {
                if payload.len() < 4 {
                    status = -1;
                } else {
                    let conn_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    let data = &payload[4..];
                    if let Err(err) = netstack::tcp_send(conn_id, data) {
                        status = map_netstack_error(err);
                    }
                }
            }
            OPCODE_TCP_RECV => {
                if payload.len() == 16 {
                    let conn_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    let max_len = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
                    let timeout = u64::from_le_bytes([
                        payload[8], payload[9], payload[10], payload[11],
                        payload[12], payload[13], payload[14], payload[15],
                    ]);
                    match netstack::tcp_recv(conn_id, timeout) {
                        Ok(data) => {
                            let cap = core::cmp::min(max_len, data.len());
                            let cap = core::cmp::min(cap, resp.len() - 12);
                            resp_len = cap;
                            resp[12..12 + cap].copy_from_slice(&data[..cap]);
                        }
                        Err(err) => {
                            status = map_netstack_error(err);
                        }
                    }
                } else {
                    status = -1;
                }
            }
            OPCODE_TCP_CLOSE => {
                if payload.len() == 4 {
                    let conn_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    if let Err(err) = netstack::tcp_close(conn_id) {
                        status = map_netstack_error(err);
                    }
                } else {
                    status = -1;
                }
            }
            OPCODE_TCP_LISTEN => {
                if payload.len() == 2 {
                    let port = u16::from_le_bytes([payload[0], payload[1]]);
                    if let Err(err) = netstack::tcp_listen(port) {
                        status = map_netstack_error(err);
                    }
                } else {
                    status = -1;
                }
            }
            OPCODE_TCP_ACCEPT => {
                if payload.len() == 8 {
                    let timeout = u64::from_le_bytes([
                        payload[0], payload[1], payload[2], payload[3],
                        payload[4], payload[5], payload[6], payload[7],
                    ]);
                    match netstack::tcp_accept(timeout) {
                        Ok(conn_id) => {
                            resp_len = 4;
                            resp[12..16].copy_from_slice(&conn_id.to_le_bytes());
                        }
                        Err(err) => {
                            status = map_netstack_error(err);
                        }
                    }
                } else {
                    status = -1;
                }
            }
            _ => {
                status = -1;
            }
            }
        }

        // レスポンス: [opcode][status][len][payload]
        resp[0..4].copy_from_slice(&opcode.to_le_bytes());
        resp[4..8].copy_from_slice(&(status as i32).to_le_bytes());
        resp[8..12].copy_from_slice(&(resp_len as u32).to_le_bytes());

        let total = 12 + resp_len;
        let _ = syscall::ipc_send(sender, &resp[..total]);

        netstack::poll_and_handle();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}

fn map_netstack_error(err: &str) -> i32 {
    if err.contains("timeout") {
        -42
    } else {
        -99
    }
}
