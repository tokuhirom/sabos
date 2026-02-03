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
#[path = "../syscall_netd.rs"]
mod syscall;

use core::panic::PanicInfo;

const OPCODE_DNS_LOOKUP: u32 = 1;
const OPCODE_TCP_CONNECT: u32 = 2;
const OPCODE_TCP_SEND: u32 = 3;
const OPCODE_TCP_RECV: u32 = 4;
const OPCODE_TCP_CLOSE: u32 = 5;

const IPC_BUF_SIZE: usize = 2048;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    netd_loop();
}

fn netd_loop() -> ! {
    let mut buf = [0u8; IPC_BUF_SIZE];
    let mut sender: u64 = 0;

    loop {
        let n = syscall::ipc_recv(&mut sender, &mut buf, 0);
        if n < 0 {
            continue;
        }
        let n = n as usize;
        if n < 8 {
            continue;
        }

        let opcode = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if 8 + len > n {
            continue;
        }
        let payload = &buf[8..8 + len];

        let mut resp = [0u8; IPC_BUF_SIZE];
        let mut resp_len = 0usize;
        let mut status: i32 = 0;

        match opcode {
            OPCODE_DNS_LOOKUP => {
                let domain = core::str::from_utf8(payload).unwrap_or("");
                let mut ip = [0u8; 4];
                let r = syscall::dns_lookup(domain, &mut ip);
                if r < 0 {
                    status = r as i32;
                } else {
                    resp_len = 4;
                    resp[12..16].copy_from_slice(&ip);
                }
            }
            OPCODE_TCP_CONNECT => {
                if payload.len() == 6 {
                    let ip = [payload[0], payload[1], payload[2], payload[3]];
                    let port = u16::from_le_bytes([payload[4], payload[5]]);
                    let r = syscall::tcp_connect(&ip, port);
                    if r < 0 {
                        status = r as i32;
                    }
                } else {
                    status = -1;
                }
            }
            OPCODE_TCP_SEND => {
                let r = syscall::tcp_send(payload);
                if r < 0 {
                    status = r as i32;
                }
            }
            OPCODE_TCP_RECV => {
                if payload.len() == 12 {
                    let max_len = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                    let timeout = u64::from_le_bytes([
                        payload[4], payload[5], payload[6], payload[7],
                        payload[8], payload[9], payload[10], payload[11],
                    ]);
                    let mut tmp = [0u8; 1024];
                    let read_len = core::cmp::min(max_len, tmp.len());
                    let r = syscall::tcp_recv(&mut tmp[..read_len], timeout);
                    if r < 0 {
                        status = r as i32;
                    } else {
                        let rlen = r as usize;
                        resp_len = rlen;
                        resp[12..12 + rlen].copy_from_slice(&tmp[..rlen]);
                    }
                } else {
                    status = -1;
                }
            }
            OPCODE_TCP_CLOSE => {
                let r = syscall::tcp_close();
                if r < 0 {
                    status = r as i32;
                }
            }
            _ => {
                status = -1;
            }
        }

        // レスポンス: [opcode][status][len][payload]
        resp[0..4].copy_from_slice(&opcode.to_le_bytes());
        resp[4..8].copy_from_slice(&(status as i32).to_le_bytes());
        resp[8..12].copy_from_slice(&(resp_len as u32).to_le_bytes());

        let total = 12 + resp_len;
        let _ = syscall::ipc_send(sender, &resp[..total]);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
