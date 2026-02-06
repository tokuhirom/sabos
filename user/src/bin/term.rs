// term.rs — GUI ターミナルエミュレータ
//
// GUI ウィンドウ内で動作するシェル。
// 既存のコンソールシェル (shell.rs) と同じコマンドが使える。
//
// ## 仕組み
//
// 1. GuiClient を使って GUI サービスにウィンドウを作成
// 2. console_grab でキーボードフォーカスを取得
// 3. key_read でノンブロッキングにキー入力をポーリング
// 4. コマンド出力を TermBuffer に蓄積し、ウィンドウにテキスト描画
//
// ## 対応コマンド
//
// echo, help, clear, exit, ls, cat, write, rm, cd, pwd,
// mem, ps, ip, run, spawn, kill, sleep, cal, selftest, halt

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../gui_client.rs"]
mod gui_client;
#[path = "../json.rs"]
mod json;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::panic::PanicInfo;

// =================================================================
// 定数
// =================================================================

/// ターミナルの列数（文字数）
const TERM_COLS: usize = 80;
/// ターミナルの行数
const TERM_ROWS: usize = 25;
/// フォント幅（8px + 1px 間隔）
const CHAR_W: u32 = 9;
/// 行の高さ（8px フォント + 4px 間隔）
const LINE_H: u32 = 12;
/// コンテンツ領域の左右パディング
const PAD_X: u32 = 4;
/// コンテンツ領域の上下パディング
const PAD_Y: u32 = 4;

/// 背景色（ダーク）
const BG: (u8, u8, u8) = (16, 16, 24);
/// 前景色（明るい緑、ターミナルっぽく）
const FG: (u8, u8, u8) = (0, 220, 100);
/// プロンプト色（明るい黄色）
const PROMPT_FG: (u8, u8, u8) = (255, 220, 100);

/// プロンプト文字列
const PROMPT: &str = "user> ";

/// 行バッファの最大サイズ
const LINE_BUFFER_SIZE: usize = 256;
/// ファイル読み取り等のバッファサイズ
const FILE_BUFFER_SIZE: usize = 4096;

// =================================================================
// TermBuffer — ターミナル出力バッファ
// =================================================================

/// ターミナルの出力テキストを管理するバッファ
///
/// 完了した行（改行済み）を lines に保持し、
/// まだ改行されていない部分を current_line に保持する。
/// GUI ウィンドウへの描画時は、末尾 TERM_ROWS 行分を表示する。
struct TermBuffer {
    /// 完了した行（改行で区切られたもの）
    lines: Vec<String>,
    /// 現在の行（まだ改行されていない）
    current_line: String,
    /// 表示可能な列数
    cols: usize,
}

impl TermBuffer {
    fn new(cols: usize) -> Self {
        Self {
            lines: Vec::new(),
            current_line: String::new(),
            cols,
        }
    }

    /// テキストをバッファに追加する
    ///
    /// 改行文字で行を分割し、列数を超えた場合は折り返す。
    fn write_text(&mut self, s: &str) {
        for ch in s.chars() {
            match ch {
                '\n' => {
                    // 改行: 現在の行を確定して新しい行を開始
                    self.lines.push(core::mem::take(&mut self.current_line));
                }
                '\r' => {
                    // キャリッジリターンは無視
                }
                _ => {
                    self.current_line.push(ch);
                    // 列数を超えたら自動折り返し
                    if self.current_line.len() >= self.cols {
                        self.lines.push(core::mem::take(&mut self.current_line));
                    }
                }
            }
        }
    }

    /// バイト列をバッファに追加する（UTF-8 として解釈）
    fn write_bytes(&mut self, b: &[u8]) {
        if let Ok(s) = core::str::from_utf8(b) {
            self.write_text(s);
        }
    }

    /// バッファをクリアする（clear コマンド用）
    fn clear(&mut self) {
        self.lines.clear();
        self.current_line.clear();
    }
}

/// core::fmt::Write を実装して format!/write! マクロを使えるようにする
impl core::fmt::Write for TermBuffer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write_text(s);
        Ok(())
    }
}

// =================================================================
// ShellState — シェルの状態
// =================================================================

/// シェルの状態（カレントディレクトリ等）
struct ShellState {
    /// カレントディレクトリのハンドル
    cwd_handle: syscall::Handle,
    /// カレントディレクトリの表示用文字列
    cwd_text: String,
    /// ディレクトリスタック（ハンドル）
    cwd_stack: Vec<syscall::Handle>,
    /// ディレクトリスタック（表示用）
    cwd_text_stack: Vec<String>,
}

// =================================================================
// エントリポイント
// =================================================================

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    app_main();
}

fn app_main() -> ! {
    // GUI クライアントを初期化
    let mut gui = gui_client::GuiClient::new();

    // ウィンドウサイズを計算
    // コンテンツ領域: 80文字 * 9px + 左右パディング 8px = 728px
    // 高さ: 25行 * 12px + 上下パディング 8px = 308px
    let content_w = TERM_COLS as u32 * CHAR_W + PAD_X * 2;
    let content_h = TERM_ROWS as u32 * LINE_H + PAD_Y * 2;
    let win_w = content_w + 4;       // +4 はウィンドウ枠
    let win_h = content_h + 4 + 24;  // +24 はタイトルバー

    let win_id = match gui.window_create(win_w, win_h, "TERMINAL") {
        Ok(id) => id,
        Err(_) => syscall::exit(),
    };

    // キーボードフォーカスを取得
    // GUI 環境でキーボード入力を受け取るために必要
    syscall::console_grab(true);

    // ターミナルバッファを初期化
    let mut term = TermBuffer::new(TERM_COLS);

    // ウェルカムメッセージ
    term.write_text("SABOS GUI Terminal\n");
    term.write_text("Type 'help' for available commands.\n\n");

    // カレントディレクトリを初期化
    let cwd_handle = match open_root_dir() {
        Ok(h) => h,
        Err(_) => {
            term.write_text("Error: Failed to open root directory\n");
            render(&term, "", &mut gui, win_id);
            loop { syscall::sleep(1000); }
        }
    };
    let mut state = ShellState {
        cwd_handle,
        cwd_text: String::from("/"),
        cwd_stack: Vec::new(),
        cwd_text_stack: Vec::new(),
    };

    // 入力バッファ
    let mut input = String::new();
    let mut key_buf = [0u8; 32];

    // 初回描画
    render(&term, &input, &mut gui, win_id);

    // メインループ: キー入力をポーリングして処理
    loop {
        let key_n = syscall::key_read(&mut key_buf);
        if key_n > 0 {
            let mut needs_redraw = false;

            for i in 0..(key_n as usize) {
                let c = key_buf[i];
                match c {
                    // Enter: コマンドを実行
                    b'\n' | b'\r' => {
                        // 入力行をバッファに表示（プロンプト付き）
                        let _ = write!(term, "{}{}\n", PROMPT, input);

                        // コマンドを実行
                        if !input.is_empty() {
                            let line = input.clone();
                            input.clear();
                            execute_command(&line, &mut term, &mut state);
                        } else {
                            input.clear();
                        }
                        needs_redraw = true;
                    }
                    // Backspace (0x08) または DEL (0x7F)
                    0x08 | 0x7F => {
                        if !input.is_empty() {
                            input.pop();
                            needs_redraw = true;
                        }
                    }
                    // ESC: 特殊キーのエスケープシーケンスを無視
                    0x1b => {
                        // ESC シーケンス（矢印キー等）はスキップ
                    }
                    // 通常の印字可能文字
                    c if c >= 0x20 && c < 0x7F => {
                        if input.len() < LINE_BUFFER_SIZE {
                            input.push(c as char);
                            needs_redraw = true;
                        }
                    }
                    // その他の制御文字は無視
                    _ => {}
                }
            }

            if needs_redraw {
                render(&term, &input, &mut gui, win_id);
            }
        }

        // 16ms スリープ（約60fps のポーリングレート）
        syscall::sleep(16);
    }
}

// =================================================================
// 描画
// =================================================================

/// ターミナルの内容を GUI ウィンドウに描画する
///
/// TermBuffer の末尾 TERM_ROWS 行分（入力行含む）をウィンドウに表示する。
/// 入力行はプロンプト付きで最下行に表示する。
fn render(
    term: &TermBuffer,
    input: &str,
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
) {
    // 背景をクリア
    let _ = gui.window_clear(win_id, BG.0, BG.1, BG.2);

    // 表示する行を収集
    // term.lines（確定済み行）+ term.current_line（未確定行）をまとめる
    let total_completed = term.lines.len();
    let has_partial = !term.current_line.is_empty();

    // 全行数: 確定行 + 未確定行(あれば) + 入力行
    let extra = if has_partial { 1 } else { 0 };
    let total_lines = total_completed + extra + 1; // +1 は入力行

    // 表示開始位置（末尾 TERM_ROWS 行を表示）
    let start = if total_lines > TERM_ROWS {
        total_lines - TERM_ROWS
    } else {
        0
    };

    let mut row: u32 = 0;
    let mut line_idx = start;

    // 確定済み行を描画
    while line_idx < total_completed && (row as usize) < TERM_ROWS - 1 {
        let line = &term.lines[line_idx];
        if !line.is_empty() {
            let _ = gui.window_text(
                win_id,
                PAD_X, PAD_Y + row * LINE_H,
                FG, BG,
                line,
            );
        }
        row += 1;
        line_idx += 1;
    }

    // 未確定行を描画
    if has_partial && line_idx == total_completed && (row as usize) < TERM_ROWS - 1 {
        let _ = gui.window_text(
            win_id,
            PAD_X, PAD_Y + row * LINE_H,
            FG, BG,
            &term.current_line,
        );
        row += 1;
    }

    // 入力行を描画（プロンプトは別色）
    if (row as usize) < TERM_ROWS {
        // プロンプト部分
        let _ = gui.window_text(
            win_id,
            PAD_X, PAD_Y + row * LINE_H,
            PROMPT_FG, BG,
            PROMPT,
        );
        // 入力テキスト + カーソル
        let input_with_cursor = format!("{}_", input);
        let prompt_px = PROMPT.len() as u32 * CHAR_W;
        let _ = gui.window_text(
            win_id,
            PAD_X + prompt_px, PAD_Y + row * LINE_H,
            FG, BG,
            &input_with_cursor,
        );
    }

    let _ = gui.window_present(win_id);
}

// =================================================================
// コマンド実行
// =================================================================

/// コマンドを実行する
fn execute_command(line: &str, term: &mut TermBuffer, state: &mut ShellState) {
    let line = line.trim();
    let (cmd, args) = split_command(line);

    match cmd {
        "echo" => cmd_echo(term, args),
        "help" => cmd_help(term),
        "clear" => term.clear(),
        "exit" => cmd_exit(),
        "ls" => cmd_ls(term, args, state),
        "cat" => cmd_cat(term, args, state),
        "write" => cmd_write(term, args, state),
        "rm" => cmd_rm(term, args, state),
        "cd" => cmd_cd(term, args, state),
        "pwd" => cmd_pwd(term, state),
        "mem" => cmd_mem(term),
        "ps" => cmd_ps(term),
        "ip" => cmd_ip(term),
        "run" => cmd_run(term, args, state),
        "spawn" => cmd_spawn(term, args, state),
        "kill" => cmd_kill(term, args),
        "sleep" => cmd_sleep(term, args),
        "cal" => cmd_cal(term, args),
        "beep" => cmd_beep(term, args),
        "selftest" => cmd_selftest(term),
        "halt" => cmd_halt(term),
        "" => {}
        _ => {
            let _ = writeln!(term, "Unknown command: {}", cmd);
            term.write_text("Type 'help' for available commands.\n");
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

// =================================================================
// コマンド実装
// =================================================================

/// echo コマンド: 引数をそのまま出力
fn cmd_echo(term: &mut TermBuffer, args: &str) {
    term.write_text(args);
    term.write_text("\n");
}

/// help コマンド: ヘルプを表示
fn cmd_help(term: &mut TermBuffer) {
    term.write_text("\n");
    term.write_text("SABOS GUI Terminal - Commands\n");
    term.write_text("============================\n\n");
    term.write_text("  echo <text>       - Print text\n");
    term.write_text("  help              - Show this help\n");
    term.write_text("  clear             - Clear terminal\n");
    term.write_text("  exit              - Close terminal\n");
    term.write_text("  ls [path]         - List directory\n");
    term.write_text("  cat <file>        - Display file\n");
    term.write_text("  write <f> <text>  - Create file\n");
    term.write_text("  rm <file>         - Delete file\n");
    term.write_text("  cd <dir>          - Change directory\n");
    term.write_text("  pwd               - Print directory\n");
    term.write_text("  mem               - Memory info\n");
    term.write_text("  ps                - Task list\n");
    term.write_text("  ip                - Network info\n");
    term.write_text("  run <file>        - Run program (fg)\n");
    term.write_text("  spawn <file>      - Run program (bg)\n");
    term.write_text("  kill <task_id>    - Kill a task\n");
    term.write_text("  sleep <ms>        - Sleep\n");
    term.write_text("  cal <m> <y>       - Calendar\n");
    term.write_text("  selftest          - Kernel selftest\n");
    term.write_text("  halt              - Halt system\n");
    term.write_text("\n");
}

/// exit コマンド: ターミナルを閉じる
fn cmd_exit() {
    // キーボードフォーカスを解放してから終了
    syscall::console_grab(false);
    syscall::exit();
}

/// selftest コマンド: カーネル selftest を実行
fn cmd_selftest(term: &mut TermBuffer) {
    term.write_text("Running kernel selftest...\n");
    let _ = syscall::selftest();
    term.write_text("(selftest output goes to serial console)\n");
}

/// beep コマンド: AC97 ドライバでビープ音を再生する
///
/// # 使い方
/// - `beep` — デフォルト (440Hz, 200ms)
/// - `beep 880` — 880Hz, 200ms
/// - `beep 880 500` — 880Hz, 500ms
fn cmd_beep(term: &mut TermBuffer, args: &str) {
    let parts: Vec<&str> = args.split_whitespace().collect();

    let freq = if parts.is_empty() {
        440
    } else {
        match parts[0].parse::<u32>() {
            Ok(n) if n >= 1 && n <= 20000 => n,
            _ => {
                term.write_text("Error: freq must be 1-20000\n");
                return;
            }
        }
    };

    let duration = if parts.len() < 2 {
        200
    } else {
        match parts[1].parse::<u32>() {
            Ok(n) if n >= 1 && n <= 10000 => n,
            _ => {
                term.write_text("Error: duration must be 1-10000 ms\n");
                return;
            }
        }
    };

    let result = syscall::sound_play(freq, duration);
    if result < 0 {
        term.write_text("Error: sound_play failed (AC97 not available?)\n");
    }
}

/// halt コマンド: システム停止
fn cmd_halt(term: &mut TermBuffer) {
    term.write_text("System halted.\n");
    syscall::halt();
}

// =================================================================
// ファイルシステムコマンド
// =================================================================

/// ls コマンド: ディレクトリ一覧
fn cmd_ls(term: &mut TermBuffer, args: &str, state: &ShellState) {
    let target = args.trim();
    let (handle, need_close) = if target.is_empty() {
        (state.cwd_handle, false)
    } else {
        match open_dir_from_args(state, target) {
            Ok(h) => (h, true),
            Err(msg) => {
                term.write_text(msg);
                term.write_text("\n");
                return;
            }
        }
    };

    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let n = syscall::handle_enum(&handle, &mut buf);
    if n < 0 {
        term.write_text("Error: Failed to list directory\n");
        if need_close {
            let _ = syscall::handle_close(&handle);
        }
        return;
    }

    if n > 0 {
        let len = n as usize;
        term.write_bytes(&buf[..len]);
        if buf[len - 1] != b'\n' {
            term.write_text("\n");
        }
    }

    if need_close {
        let _ = syscall::handle_close(&handle);
    }
}

/// cat コマンド: ファイル内容を表示
fn cmd_cat(term: &mut TermBuffer, args: &str, state: &ShellState) {
    if args.is_empty() {
        term.write_text("Usage: cat <filename>\n");
        return;
    }

    let handle = match open_file_from_args(state, args.trim()) {
        Ok(h) => h,
        Err(msg) => {
            term.write_text(msg);
            term.write_text("\n");
            return;
        }
    };

    let mut buf = [0u8; 512];
    loop {
        let n = syscall::handle_read(&handle, &mut buf);
        if n < 0 {
            term.write_text("Error: File read failed\n");
            break;
        }
        if n == 0 {
            break;
        }
        term.write_bytes(&buf[..n as usize]);
    }

    let _ = syscall::handle_close(&handle);
}

/// write コマンド: ファイルを作成/上書き
///
/// syscall 経由でファイルを書き込む。
fn cmd_write(term: &mut TermBuffer, args: &str, state: &ShellState) {
    let (filename, content) = split_command(args);
    if filename.is_empty() {
        term.write_text("Usage: write <filename> <text>\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, filename);

    if syscall::file_write(&abs_path, content.as_bytes()) < 0 {
        term.write_text("Error: Failed to write file\n");
        return;
    }

    term.write_text("File written successfully\n");
}

/// rm コマンド: ファイルを削除
///
/// syscall 経由でファイルを削除する。
fn cmd_rm(term: &mut TermBuffer, args: &str, state: &ShellState) {
    let filename = args.trim();
    if filename.is_empty() {
        term.write_text("Usage: rm <filename>\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, filename);

    if syscall::file_delete(&abs_path) < 0 {
        let _ = writeln!(term, "Error: Failed to delete '{}'", filename);
    } else {
        let _ = writeln!(term, "Deleted '{}'", filename);
    }
}

/// cd コマンド: カレントディレクトリを変更
fn cmd_cd(term: &mut TermBuffer, args: &str, state: &mut ShellState) {
    let target = args.trim();
    if target.is_empty() || target == "/" {
        // ルートに戻る
        close_handle_stack(state);
        if let Ok(new_root) = open_root_dir() {
            let _ = syscall::handle_close(&state.cwd_handle);
            state.cwd_handle = new_root;
            state.cwd_text = String::from("/");
        } else {
            term.write_text("Error: Failed to open root directory\n");
        }
        return;
    }

    if target == ".." || target == "-" {
        if let Some(prev_handle) = state.cwd_stack.pop() {
            if let Some(prev_text) = state.cwd_text_stack.pop() {
                let _ = syscall::handle_close(&state.cwd_handle);
                state.cwd_handle = prev_handle;
                state.cwd_text = prev_text;
            }
        } else {
            term.write_text("Error: No previous directory\n");
        }
        return;
    }

    let new_handle = match open_dir_from_args(state, target) {
        Ok(h) => h,
        Err(msg) => {
            term.write_text(msg);
            term.write_text("\n");
            return;
        }
    };

    // ディレクトリかどうか確認
    let mut buf = [0u8; 8];
    if syscall::handle_enum(&new_handle, &mut buf) < 0 {
        let _ = syscall::handle_close(&new_handle);
        term.write_text("Error: Not a directory\n");
        return;
    }

    state.cwd_stack.push(state.cwd_handle);
    state.cwd_text_stack.push(state.cwd_text.clone());
    state.cwd_handle = new_handle;
    state.cwd_text = resolve_path(&state.cwd_text, target);
}

/// pwd コマンド: カレントディレクトリを表示
fn cmd_pwd(term: &mut TermBuffer, state: &ShellState) {
    term.write_text(&state.cwd_text);
    term.write_text("\n");
}

// =================================================================
// システム情報コマンド
// =================================================================

/// mem コマンド: メモリ情報を表示
fn cmd_mem(term: &mut TermBuffer) {
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::get_mem_info(&mut buf);
    if result < 0 {
        term.write_text("Error: Failed to get memory info\n");
        return;
    }
    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        let total = json::json_find_u64(s, "total_frames").unwrap_or(0);
        let allocated = json::json_find_u64(s, "allocated_frames").unwrap_or(0);
        let free = json::json_find_u64(s, "free_frames").unwrap_or(0);
        let free_kib = json::json_find_u64(s, "free_kib").unwrap_or(0);
        let _ = writeln!(term, "Memory: {} total / {} alloc / {} free ({} KiB)",
            total, allocated, free, free_kib);
    }
}

/// ps コマンド: タスク一覧を表示
fn cmd_ps(term: &mut TermBuffer) {
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::get_task_list(&mut buf);
    if result < 0 {
        term.write_text("Error: Failed to get task list\n");
        return;
    }

    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        term.write_text("  ID  STATE       NAME\n");
        term.write_text("  --  ----------  ----------\n");

        let Some((tasks_start, tasks_end)) = json::json_find_array_bounds(s, "tasks") else {
            return;
        };

        let mut i = tasks_start;
        while i < tasks_end {
            let bytes = s.as_bytes();
            while i < tasks_end && bytes[i] != b'{' && bytes[i] != b']' {
                i += 1;
            }
            if i >= tasks_end || bytes[i] == b']' {
                break;
            }

            let Some(obj_end) = json::find_matching_brace(s, i) else {
                break;
            };
            if obj_end > tasks_end {
                break;
            }

            let obj = &s[i + 1..obj_end];
            let id = json::json_find_u64(obj, "id");
            let st = json::json_find_str(obj, "state");
            let name = json::json_find_str(obj, "name");

            if let (Some(id), Some(st), Some(name)) = (id, st, name) {
                let _ = writeln!(term, "  {:>2}  {:<10}  {}", id, st, name);
            }

            i = obj_end + 1;
        }
    }
}

/// ip コマンド: ネットワーク情報を表示
fn cmd_ip(term: &mut TermBuffer) {
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::get_net_info(&mut buf);
    if result < 0 {
        term.write_text("Error: Failed to get network info\n");
        return;
    }
    let len = result as usize;
    term.write_bytes(&buf[..len]);
    if len > 0 && buf[len - 1] != b'\n' {
        term.write_text("\n");
    }
}

// =================================================================
// プロセス管理コマンド
// =================================================================

/// run コマンド: ELF プログラムをフォアグラウンドで実行
fn cmd_run(term: &mut TermBuffer, args: &str, state: &ShellState) {
    let filename = args.trim();
    if filename.is_empty() {
        term.write_text("Usage: run <FILENAME>\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, filename);
    let _ = writeln!(term, "Running {}...", abs_path);

    // exec はブロッキング（子プロセスの終了を待つ）
    // 子プロセスの出力はシリアルコンソールに行く（ターミナルウィンドウには表示されない）
    let result = syscall::exec(&abs_path);
    if result < 0 {
        term.write_text("Error: Failed to run program\n");
    } else {
        term.write_text("Program exited.\n");
    }
}

/// spawn コマンド: ELF プログラムをバックグラウンドで実行
fn cmd_spawn(term: &mut TermBuffer, args: &str, state: &ShellState) {
    let filename = args.trim();
    if filename.is_empty() {
        term.write_text("Usage: spawn <FILENAME>\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, filename);
    let _ = writeln!(term, "Spawning {}...", abs_path);

    let result = syscall::spawn(&abs_path);
    if result < 0 {
        term.write_text("Error: Failed to spawn process\n");
    } else {
        let _ = writeln!(term, "Process spawned as task {} (background)", result);
    }
}

/// kill コマンド: タスクを強制終了
fn cmd_kill(term: &mut TermBuffer, args: &str) {
    let id_str = args.trim();
    if id_str.is_empty() {
        term.write_text("Usage: kill <task_id>\n");
        return;
    }

    let task_id = match parse_u64(id_str) {
        Some(id) => id,
        None => {
            term.write_text("Error: invalid task ID\n");
            return;
        }
    };

    let result = syscall::kill(task_id);
    if result == 0 {
        let _ = writeln!(term, "Task {} killed.", task_id);
    } else {
        let _ = writeln!(term, "Error: failed to kill task {} (error {})", task_id, -result);
    }
}

/// sleep コマンド: 指定ミリ秒スリープ
fn cmd_sleep(term: &mut TermBuffer, args: &str) {
    let ms_str = args.trim();
    if ms_str.is_empty() {
        term.write_text("Usage: sleep <milliseconds>\n");
        return;
    }

    let ms = match parse_u64(ms_str) {
        Some(n) => n,
        None => {
            term.write_text("Error: Invalid number\n");
            return;
        }
    };

    let _ = writeln!(term, "Sleeping for {} ms...", ms);
    syscall::sleep(ms);
    term.write_text("Done.\n");
}

// =================================================================
// cal コマンド
// =================================================================

/// cal コマンド: カレンダーを表示
fn cmd_cal(term: &mut TermBuffer, args: &str) {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.len() != 2 {
        term.write_text("Usage: cal <month> <year>\n");
        term.write_text("  Example: cal 2 2026\n");
        return;
    }

    let month: u32 = match parse_u64(parts[0]) {
        Some(m) if (1..=12).contains(&m) => m as u32,
        _ => {
            term.write_text("Invalid month (1-12)\n");
            return;
        }
    };

    let year: u32 = match parse_u64(parts[1]) {
        Some(y) if y >= 1 && y <= u32::MAX as u64 => y as u32,
        _ => {
            term.write_text("Invalid year\n");
            return;
        }
    };

    let month_name = match month {
        1 => "January", 2 => "February", 3 => "March",
        4 => "April", 5 => "May", 6 => "June",
        7 => "July", 8 => "August", 9 => "September",
        10 => "October", 11 => "November", 12 => "December",
        _ => unreachable!(),
    };

    // ヘッダー（月名と年をセンタリング）
    let header = format!("{} {}", month_name, year);
    let pad = if header.len() < 20 { (20 - header.len()) / 2 } else { 0 };
    for _ in 0..pad {
        term.write_text(" ");
    }
    let _ = writeln!(term, "{}", header);
    term.write_text("Su Mo Tu We Th Fr Sa\n");

    let days_in_month = cal_days_in_month(year, month);
    let first_dow = cal_day_of_week(year, month, 1);

    // 月初日までの空白
    for _ in 0..first_dow {
        term.write_text("   ");
    }

    let mut dow = first_dow;
    for day in 1..=days_in_month {
        if day < 10 {
            let _ = write!(term, " {}", day);
        } else {
            let _ = write!(term, "{}", day);
        }

        dow += 1;
        if dow == 7 {
            term.write_text("\n");
            dow = 0;
        } else {
            term.write_text(" ");
        }
    }

    if dow != 0 {
        term.write_text("\n");
    }
}

/// ツェラーの公式で曜日を計算する
/// 戻り値: 0=日曜, 1=月曜, ..., 6=土曜
fn cal_day_of_week(year: u32, month: u32, day: u32) -> u32 {
    let (y, m) = if month <= 2 {
        (year as i32 - 1, month as i32 + 12)
    } else {
        (year as i32, month as i32)
    };

    let q = day as i32;
    let k = y % 100;
    let j = y / 100;

    let h = (q + (13 * (m + 1)) / 5 + k + k / 4 + j / 4 - 2 * j) % 7;
    let h = ((h + 7) % 7) as u32;
    match h {
        0 => 6, 1 => 0, 2 => 1, 3 => 2, 4 => 3, 5 => 4, 6 => 5,
        _ => unreachable!(),
    }
}

/// 指定した年月の日数を返す（うるう年対応）
fn cal_days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

// =================================================================
// ユーティリティ関数
// =================================================================

/// ルートディレクトリを開く
fn open_root_dir() -> Result<syscall::Handle, syscall::SyscallResult> {
    syscall::open("/", syscall::HANDLE_RIGHTS_DIRECTORY_READ)
}

/// 引数からディレクトリハンドルを開く
fn open_dir_from_args(state: &ShellState, args: &str) -> Result<syscall::Handle, &'static str> {
    if args.starts_with('/') {
        syscall::open(args, syscall::HANDLE_RIGHTS_DIRECTORY_READ)
            .map_err(|_| "Error: Failed to open directory")
    } else {
        syscall::openat(&state.cwd_handle, args, syscall::HANDLE_RIGHTS_DIRECTORY_READ)
            .map_err(|_| "Error: Failed to open directory")
    }
}

/// 引数からファイルハンドルを開く
fn open_file_from_args(state: &ShellState, args: &str) -> Result<syscall::Handle, &'static str> {
    if args.starts_with('/') {
        syscall::open(args, syscall::HANDLE_RIGHTS_FILE_READ)
            .map_err(|_| "Error: File not found")
    } else {
        syscall::openat(&state.cwd_handle, args, syscall::HANDLE_RIGHTS_FILE_READ)
            .map_err(|_| "Error: File not found")
    }
}

/// cwd スタックのハンドルを閉じる
fn close_handle_stack(state: &mut ShellState) {
    for handle in state.cwd_stack.drain(..) {
        let _ = syscall::handle_close(&handle);
    }
    state.cwd_text_stack.clear();
}

/// カレントディレクトリと入力パスから絶対パスを作る
fn resolve_path(cwd: &str, input: &str) -> String {
    if input.starts_with('/') {
        return normalize_path(input);
    }
    if cwd == "/" {
        return normalize_path(&format!("/{}", input));
    }
    normalize_path(&format!("{}/{}", cwd, input))
}

/// 絶対パスを正規化する（"." と ".." を処理）
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            let _ = parts.pop();
            continue;
        }
        parts.push(part);
    }

    if parts.is_empty() {
        return String::from("/");
    }

    let mut result = String::from("/");
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            result.push('/');
        }
        result.push_str(part);
    }
    result
}

/// 文字列を u64 にパース
fn parse_u64(s: &str) -> Option<u64> {
    let mut result: u64 = 0;
    for c in s.chars() {
        if !c.is_ascii_digit() {
            return None;
        }
        result = result.checked_mul(10)?;
        result = result.checked_add((c as u64) - ('0' as u64))?;
    }
    Some(result)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::console_grab(false);
    syscall::exit();
}
