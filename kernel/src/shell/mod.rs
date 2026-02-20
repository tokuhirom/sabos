// shell/mod.rs — SABOS シェル（エントリポイント・コア機能）
//
// キーボードから受け取った文字を行バッファに溜めて、
// Enter で「コマンド」として解釈・実行する。
// 簡易的なコマンドラインインターフェースを提供する。

mod commands;
mod selftest;

use alloc::string::String;
use alloc::vec::Vec;

use crate::framebuffer;
use crate::{kprint, kprintln};

/// シェルの状態を管理する構造体。
pub struct Shell {
    /// 現在入力中の行バッファ
    line_buffer: String,
    /// メモリ情報（起動時に取得した値を保持）
    usable_mib: u64,
    usable_pages: u64,
}

impl Shell {
    /// 新しいシェルを作成する。
    /// メモリ情報は起動時にしか取得できないので、ここで受け取って保持する。
    pub fn new(usable_mib: u64, usable_pages: u64) -> Self {
        Self {
            line_buffer: String::new(),
            usable_mib,
            usable_pages,
        }
    }

    /// プロンプトを表示する。
    pub fn print_prompt(&self) {
        framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
        kprint!("sabos> ");
        framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    }

    /// キーボードから1文字受け取って処理する。
    /// Enter でコマンド実行、Backspace で1文字削除、
    /// それ以外は行バッファに追加してエコーバックする。
    pub fn handle_char(&mut self, c: char) {
        match c {
            // Enter: コマンドを実行
            '\n' => {
                kprintln!();
                self.execute_command();
                self.line_buffer.clear();
                self.print_prompt();
            }
            // Backspace (0x08): 1文字削除
            '\x08' => {
                if !self.line_buffer.is_empty() {
                    self.line_buffer.pop();
                    // 画面上のカーソルを1文字戻して、その位置を背景色で塗りつぶす。
                    // '\x08' は framebuffer.rs でバックスペース処理される。
                    kprint!("\x08");
                }
            }
            // Tab: 無視（将来的にはタブ補完）
            '\t' => {}
            // 表示可能な文字: バッファに追加してエコー
            c if !c.is_control() => {
                self.line_buffer.push(c);
                kprint!("{}", c);
            }
            // その他の制御文字: 無視
            _ => {}
        }
    }

    /// 行バッファの内容をコマンドとして解釈・実行する。
    fn execute_command(&self) {
        let cmd = self.line_buffer.trim();
        if cmd.is_empty() {
            return;
        }

        // コマンド名と引数を分離（最初の空白で2分割）
        let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
        let command = parts[0];
        let args = if parts.len() > 1 { parts[1] } else { "" };

        match command {
            "help" => self.cmd_help(),
            "clear" => self.cmd_clear(),
            "mem" => self.cmd_mem(),
            "page" => self.cmd_page(args),
            "ps" => self.cmd_ps(),
            "echo" => self.cmd_echo(args),
            "usermode" => self.cmd_usermode(),
            "usertest" => self.cmd_usertest(),
            "isolate" => self.cmd_isolate(),
            "elf" => self.cmd_elf(),
            "lspci" => self.cmd_lspci(),
            "blkread" => self.cmd_blkread(args),
            "blkwrite" => self.cmd_blkwrite(args),
            "ls" => self.cmd_ls(args),
            "cat" => self.cmd_cat(args),
            "write" => self.cmd_write(args),
            "rm" => self.cmd_rm(args),
            "run" => self.cmd_run(args),
            "spawn" => self.cmd_spawn(args),
            "ip" => self.cmd_ip(),
            "linkstatus" => self.cmd_linkstatus(),
            "selftest" => self.cmd_selftest(args),
            "ipc_bench" => self.cmd_ipc_bench(args),
            "beep" => self.cmd_beep(args),
            "panic" => self.cmd_panic(),
            "shutdown" => self.cmd_shutdown(),
            "reboot" => self.cmd_reboot(),
            "halt" => self.cmd_halt(),
            "exit_qemu" => self.cmd_exit_qemu(args),
            _ => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Unknown command: {}", command);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                kprintln!("Type 'help' for available commands.");
            }
        }
    }
}

/// rdtsc 命令で TSC (Time Stamp Counter) を読み取る。
///
/// IPC ベンチマーク等のサイクル計測で使用する。
#[inline]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// カーネル側から selftest を実行するための公開関数。
///
/// syscall から呼べるように、最小限の Shell を生成して selftest を実行する。
/// syscall 経由で selftest を実行する。
/// auto_exit が true の場合、selftest のコマンド引数に "--exit" を付けて
/// ISA debug exit で QEMU を自動終了する。
pub fn run_selftest(auto_exit: bool) {
    let shell = Shell::new(0, 0);
    if auto_exit {
        shell.cmd_selftest("--exit");
    } else {
        shell.cmd_selftest("");
    }
}
