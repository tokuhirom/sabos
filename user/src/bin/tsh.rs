// tsh.rs — Telnet 用の簡易シェル（user space）
//
// telnetd からの IPC 入力を受け取り、結果を IPC で返す。
// 既存の SHELL.ELF とは別プロセスで動く。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use alloc::string::String;
use core::panic::PanicInfo;

const OPCODE_INIT: u32 = 1;
const OPCODE_INPUT: u32 = 2;
const OPCODE_OUTPUT: u32 = 3;

const IPC_BUF_SIZE: usize = 2048;
const FILE_BUFFER_SIZE: usize = 2048;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    tsh_main();
}

fn tsh_main() -> ! {
    let telnetd_id = wait_init();
    send_output(telnetd_id, "SABOS telnet shell\n");
    send_prompt(telnetd_id);

    let mut buf = [0u8; IPC_BUF_SIZE];
    let mut sender = 0u64;

    loop {
        let n = syscall::ipc_recv(&mut sender, &mut buf, 0);
        if n < 0 {
            syscall::sleep(10);
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
        if opcode != OPCODE_INPUT {
            continue;
        }
        let line = &buf[8..8 + len];
        handle_line(telnetd_id, line);
    }
}

fn wait_init() -> u64 {
    let mut buf = [0u8; IPC_BUF_SIZE];
    let mut sender = 0u64;
    loop {
        let n = syscall::ipc_recv(&mut sender, &mut buf, 0);
        if n < 0 {
            syscall::sleep(10);
            continue;
        }
        let n = n as usize;
        if n < 16 {
            continue;
        }
        let opcode = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if opcode != OPCODE_INIT || len != 8 {
            continue;
        }
        let id = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11],
            buf[12], buf[13], buf[14], buf[15],
        ]);
        return id;
    }
}

fn handle_line(telnetd_id: u64, line: &[u8]) {
    let Ok(s) = core::str::from_utf8(line) else {
        send_output(telnetd_id, "Error: invalid UTF-8\n");
        send_prompt(telnetd_id);
        return;
    };
    let s = s.trim();
    if s.is_empty() {
        send_prompt(telnetd_id);
        return;
    }

    let mut parts = s.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    match cmd {
        "help" => {
            send_output(telnetd_id, "Commands: help ls cat run spawn mem ps ip selftest exit\n");
        }
        "ls" => {
            let path = parts.next().unwrap_or("/");
            let mut buf = [0u8; FILE_BUFFER_SIZE];
            let n = syscall::dir_list(path, &mut buf);
            if n < 0 {
                send_output(telnetd_id, "Error: ls failed\n");
            } else {
                let n = n as usize;
                let Ok(text) = core::str::from_utf8(&buf[..n]) else {
                    send_output(telnetd_id, "Error: invalid dir list\n");
                    send_prompt(telnetd_id);
                    return;
                };
                send_output(telnetd_id, text);
                send_output(telnetd_id, "\n");
            }
        }
        "cat" => {
            let path = parts.next().unwrap_or("");
            if path.is_empty() {
                send_output(telnetd_id, "Usage: cat <path>\n");
            } else {
                cat_file(telnetd_id, path);
            }
        }
        "run" => {
            let path = parts.next().unwrap_or("");
            if path.is_empty() {
                send_output(telnetd_id, "Usage: run <path>\n");
            } else {
                let _ = syscall::exec(path);
            }
        }
        "spawn" => {
            let path = parts.next().unwrap_or("");
            if path.is_empty() {
                send_output(telnetd_id, "Usage: spawn <path>\n");
            } else {
                let id = syscall::spawn(path);
                if id < 0 {
                    send_output(telnetd_id, "Error: spawn failed\n");
                } else {
                    let mut msg = String::new();
                    msg.push_str("Spawned ");
                    msg.push_str(path);
                    msg.push_str(" (PID ");
                    msg.push_str(&itoa(id as u64));
                    msg.push_str(")\n");
                    send_output(telnetd_id, msg.as_str());
                }
            }
        }
        "mem" => {
            let mut buf = [0u8; FILE_BUFFER_SIZE];
            let n = syscall::get_mem_info(&mut buf);
            if n < 0 {
                send_output(telnetd_id, "Error: mem failed\n");
            } else {
                let n = n as usize;
                if let Ok(text) = core::str::from_utf8(&buf[..n]) {
                    send_output(telnetd_id, text);
                    send_output(telnetd_id, "\n");
                }
            }
        }
        "ps" => {
            let mut buf = [0u8; FILE_BUFFER_SIZE];
            let n = syscall::get_task_list(&mut buf);
            if n < 0 {
                send_output(telnetd_id, "Error: ps failed\n");
            } else {
                let n = n as usize;
                if let Ok(text) = core::str::from_utf8(&buf[..n]) {
                    send_output(telnetd_id, text);
                    send_output(telnetd_id, "\n");
                }
            }
        }
        "ip" => {
            let mut buf = [0u8; FILE_BUFFER_SIZE];
            let n = syscall::get_net_info(&mut buf);
            if n < 0 {
                send_output(telnetd_id, "Error: ip failed\n");
            } else {
                let n = n as usize;
                if let Ok(text) = core::str::from_utf8(&buf[..n]) {
                    send_output(telnetd_id, text);
                    send_output(telnetd_id, "\n");
                }
            }
        }
        "selftest" => {
            let _ = syscall::selftest();
            send_output(telnetd_id, "selftest done\n");
        }
        "exit" => {
            send_output(telnetd_id, "bye\n");
            syscall::exit();
        }
        _ => {
            send_output(telnetd_id, "Error: unknown command\n");
        }
    }

    send_prompt(telnetd_id);
}

fn cat_file(telnetd_id: u64, path: &str) {
    let handle = match syscall::open(path, syscall::HANDLE_RIGHT_READ) {
        Ok(h) => h,
        Err(_) => {
            send_output(telnetd_id, "Error: open failed\n");
            return;
        }
    };
    let mut buf = [0u8; 1024];
    loop {
        let n = syscall::handle_read(&handle, &mut buf);
        if n < 0 {
            send_output(telnetd_id, "Error: read failed\n");
            break;
        }
        if n == 0 {
            break;
        }
        let n = n as usize;
        if let Ok(text) = core::str::from_utf8(&buf[..n]) {
            send_output(telnetd_id, text);
        } else {
            send_output(telnetd_id, "[binary]\n");
        }
    }
    let _ = syscall::handle_close(&handle);
    send_output(telnetd_id, "\n");
}

fn send_prompt(telnetd_id: u64) {
    send_output(telnetd_id, "tsh> ");
}

fn send_output(telnetd_id: u64, s: &str) {
    let bytes = s.as_bytes();
    let mut buf = [0u8; IPC_BUF_SIZE];
    if 8 + bytes.len() > buf.len() {
        return;
    }
    buf[0..4].copy_from_slice(&OPCODE_OUTPUT.to_le_bytes());
    buf[4..8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf[8..8 + bytes.len()].copy_from_slice(bytes);
    let _ = syscall::ipc_send(telnetd_id, &buf[..8 + bytes.len()]);
}

fn itoa(mut v: u64) -> String {
    if v == 0 {
        return String::from("0");
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while v > 0 {
        let d = (v % 10) as u8;
        buf[i] = b'0' + d;
        i += 1;
        v /= 10;
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
