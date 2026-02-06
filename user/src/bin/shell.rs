// shell.rs — SABOS ユーザー空間シェル（独立バイナリ版）
//
// init プロセスから起動される独立した ELF バイナリ。
// システムコールを使ってカーネルとやり取りする。
//
// ## コマンド
//
// - echo <text>: テキストを出力
// - help: ヘルプを表示
// - clear: 画面をクリア
// - exit: シェルを終了
// - ls [path]: ディレクトリ一覧
// - cat <file>: ファイル内容を表示
// - write <file> <text>: ファイルを作成/上書き
// - rm <file>: ファイルを削除
// - cd <dir>: カレントディレクトリを変更
// - pwd: カレントディレクトリを表示
// - pushd <dir>: ディレクトリスタックに積んで移動
// - popd: ディレクトリスタックから戻る
// - mem: メモリ情報を表示
// - ps: タスク一覧を表示
// - ip: ネットワーク情報を表示
// - lspci: PCI デバイス一覧を表示
// - run <file>: ELF プログラムをフォアグラウンドで実行
// - spawn <file>: ELF プログラムをバックグラウンドで実行
// - kill <task_id>: タスクを強制終了
// - sleep <ms>: 指定ミリ秒スリープ
// - dns <domain>: DNS 解決
// - http <host[:port]> [path]: HTTP GET リクエスト（localhost 対応）
// - sed [-n] s/OLD/NEW/[gp] <file>: 簡易 sed（リテラル置換）
// - grep [-i] [-v] [-c] PATTERN [FILE]: パターンに一致する行を出力
// - top: リアルタイムシステムモニター（ps + mem を定期更新）
// - nc <host> <port>: TCP接続（netcat クライアントモード）
// - nc -l <port>: TCP待ち受け（netcat サーバーモード）
// - パイプ（|）: echo/cat/sed/grep の簡易パイプライン
// - selftest: カーネル selftest を実行
// - halt: システム停止

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../fat32.rs"]
mod fat32;
#[path = "../gui_client.rs"]
mod gui_client;
#[path = "../json.rs"]
mod json;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use sabos_textutil::{contains_literal, replace_literal};

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::panic::PanicInfo;
use fat32::Fat32;

/// netd のタスクID（起動できた場合のみ設定）
static mut NETD_TASK_ID: u64 = 0;
/// 行バッファの最大サイズ
const LINE_BUFFER_SIZE: usize = 256;

/// シェルの状態
///
/// カーネル側にカレントディレクトリ情報を持たせない方針なので、
/// ユーザーシェル内で cwd を保持する。
/// 真実は cwd_handle。cwd_text は表示専用。
struct ShellState {
    /// カレントディレクトリのハンドル（真実）
    cwd_handle: syscall::Handle,
    /// カレントディレクトリの表示用文字列
    cwd_text: String,
    /// 過去の cwd を戻るためのスタック（ハンドル）
    cwd_stack: Vec<syscall::Handle>,
    /// 過去の cwd を戻るためのスタック（表示用）
    cwd_text_stack: Vec<String>,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    run();
}

/// シェルのメインループを実行
fn run() -> ! {
    // log クレートのロガーを初期化
    // これにより log::info!() 等のマクロが使えるようになる
    print::init_logger();

    print_welcome();
    log::info!("Shell started");
    // init が netd を起動するので、ここでは netd の PID を取得するだけ
    // 将来的には init から netd の PID を受け取る仕組みにする
    find_netd();

    // カレントディレクトリはユーザー空間で管理する
    let cwd_handle = match open_root_dir() {
        Ok(h) => h,
        Err(_) => {
            syscall::write_str("Error: Failed to open root directory handle\n");
            syscall::exit();
        }
    };
    let mut state = ShellState {
        cwd_handle,
        cwd_text: String::from("/"),
        cwd_stack: Vec::new(),
        cwd_text_stack: Vec::new(),
    };

    let mut line_buf = [0u8; LINE_BUFFER_SIZE];

    loop {
        // プロンプトを表示
        syscall::write_str("user> ");

        // 行を読み取る（改行まで）
        let line_len = read_line(&mut line_buf);

        // 空行は無視
        if line_len == 0 {
            continue;
        }

        // コマンドを実行
        let line = &line_buf[..line_len];
        execute_command(line, &mut state);
    }
}

/// ウェルカムメッセージを表示
fn print_welcome() {
    syscall::write_str("\n");
    syscall::write_str("=================================\n");
    syscall::write_str("  SABOS User Shell\n");
    syscall::write_str("=================================\n");
    syscall::write_str("Type 'help' for available commands.\n");
    syscall::write_str("\n");
}

/// netd の PID を探す（ps コマンド相当の処理）
/// init が先に netd を起動しているはずなので、タスク一覧から探す
fn find_netd() {
    let netd_id = resolve_task_id_by_name("NETD.ELF").unwrap_or(0);
    unsafe {
        NETD_TASK_ID = netd_id;
    }
}


/// 改行まで1行を読み取る
///
/// エコーバックを行い、バックスペースに対応する。
/// 戻り値は読み取った文字数（改行を含まない）。
fn read_line(buf: &mut [u8]) -> usize {
    let mut len = 0;

    loop {
        let c = syscall::read_char();

        match c {
            // Enter（改行）
            '\n' | '\r' => {
                syscall::write_str("\n");
                return len;
            }
            // Backspace (0x08) または DEL (0x7F)
            '\x08' | '\x7f' => {
                if len > 0 {
                    len -= 1;
                    // カーソルを1つ戻して空白で上書き、さらに戻る
                    syscall::write_str("\x08 \x08");
                }
            }
            // 通常の文字
            c if c.is_ascii() && !c.is_ascii_control() => {
                if len < buf.len() {
                    buf[len] = c as u8;
                    len += 1;
                    // エコーバック
                    syscall::write(&[c as u8]);
                }
            }
            // その他の制御文字は無視
            _ => {}
        }
    }
}

/// コマンドを実行
fn execute_command(line: &[u8], state: &mut ShellState) {
    // UTF-8 として解釈
    let line_str = match core::str::from_utf8(line) {
        Ok(s) => s.trim(),
        Err(_) => {
            syscall::write_str("Error: Invalid UTF-8 input\n");
            return;
        }
    };

    // コマンドと引数に分割
    let (cmd, args) = split_command(line_str);

    if line_str.contains('|') {
        if let Err(msg) = execute_pipeline(line_str, state) {
            syscall::write_str(msg);
            syscall::write_str("\n");
        }
        return;
    }

    match cmd {
        "echo" => cmd_echo(args),
        "help" => cmd_help(),
        "clear" => cmd_clear(),
        "exit" => cmd_exit(),
        "ls" => cmd_ls(args, state),
        "cat" => cmd_cat(args, state),
        "write" => cmd_write(args, state),
        "rm" => cmd_rm(args, state),
        "mkdir" => cmd_mkdir(args, state),
        "rmdir" => cmd_rmdir(args, state),
        "cd" => cmd_cd(args, state),
        "pwd" => cmd_pwd(state),
        "pushd" => cmd_pushd(args, state),
        "popd" => cmd_popd(state),
        "df" => cmd_df(),
        "mem" => cmd_mem(),
        "ps" => cmd_ps(),
        "top" => cmd_top(),
        "ip" => cmd_ip(),
        "lspci" => cmd_lspci(),
        "run" => cmd_run(args, state),
        "spawn" => cmd_spawn(args, state),
        "kill" => cmd_kill(args),
        "sleep" => cmd_sleep(args),
        "dns" => cmd_dns(args),
        "http" => cmd_http(args),
        "nc" => cmd_nc(args),
        "sed" => cmd_sed(args, state),
        "grep" => cmd_grep(args, state),
        "gui" => cmd_gui(args),
        "rect" => cmd_rect(args),
        "cal" => cmd_cal(args),
        "selftest" => cmd_selftest(),
        "halt" => cmd_halt(),
        "" => {}  // 空のコマンドは無視
        _ => {
            syscall::write_str("Unknown command: ");
            syscall::write_str(cmd);
            syscall::write_str("\nType 'help' for available commands.\n");
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

/// パイプラインを実行（簡易版）
///
/// 対応コマンド: echo / cat / sed / grep
/// 入出力は UTF-8 テキスト前提で扱う。
fn execute_pipeline(line: &str, state: &ShellState) -> Result<(), &'static str> {
    let mut parts: Vec<&str> = Vec::new();
    for part in line.split('|') {
        let p = part.trim();
        if p.is_empty() {
            return Err("Error: empty pipeline segment");
        }
        parts.push(p);
    }
    if parts.len() < 2 {
        return Err("Error: invalid pipeline");
    }

    let mut input: Option<String> = None;
    for (i, part) in parts.iter().enumerate() {
        let (cmd, args) = split_command(part);
        let output = match cmd {
            "echo" => pipeline_echo(args),
            "cat" => pipeline_cat(args, state, input.as_deref())?,
            "sed" => pipeline_sed(args, state, input.as_deref())?,
            "grep" => pipeline_grep(args, state, input.as_deref())?,
            _ => return Err("Error: pipeline supports only echo/cat/sed/grep"),
        };

        if i + 1 == parts.len() {
            syscall::write_str(&output);
        } else {
            input = Some(output);
        }
    }

    Ok(())
}

fn pipeline_echo(args: &str) -> String {
    let mut out = String::new();
    out.push_str(args);
    out.push('\n');
    out
}

fn pipeline_cat(args: &str, state: &ShellState, input: Option<&str>) -> Result<String, &'static str> {
    let target = args.trim();
    if target.is_empty() {
        if let Some(text) = input {
            return Ok(String::from(text));
        }
        return Err("Usage: cat <filename>");
    }

    let handle = open_file_from_args(state, target)?;
    let data = read_all_handle(&handle).map_err(|_| "Error: Failed to read file")?;
    let _ = syscall::handle_close(&handle);

    let Ok(text) = core::str::from_utf8(&data) else {
        return Err("Error: invalid UTF-8 in input");
    };
    Ok(String::from(text))
}

fn pipeline_sed(args: &str, state: &ShellState, input: Option<&str>) -> Result<String, &'static str> {
    let (suppress, expr, file) = parse_sed_args(args)?;
    let (from, to, global, print_on_change) = parse_sed_expr(expr)?;
    if from.is_empty() {
        return Err("Error: empty search pattern is not supported");
    }

    let owned = if !file.is_empty() {
        let handle = open_file_from_args(state, file)?;
        let data = read_all_handle(&handle).map_err(|_| "Error: Failed to read file")?;
        let _ = syscall::handle_close(&handle);
        let Ok(text) = core::str::from_utf8(&data) else {
            return Err("Error: invalid UTF-8 in input");
        };
        Some(String::from(text))
    } else {
        None
    };

    let text = if let Some(ref owned_text) = owned {
        owned_text.as_str()
    } else if let Some(text) = input {
        text
    } else {
        return Err("Usage: sed [-n] s/OLD/NEW/[gp] <file>");
    };

    sed_apply(text, from, to, global, print_on_change, suppress)
}

/// ルートディレクトリのハンドルを開く
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
            .map_err(|_| "Error: File not found or cannot be read")
    } else {
        syscall::openat(&state.cwd_handle, args, syscall::HANDLE_RIGHTS_FILE_READ)
            .map_err(|_| "Error: File not found or cannot be read")
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
///
/// 例:
/// - cwd="/", input="FOO" -> "/FOO"
/// - cwd="/A", input="../B" -> "/B"
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

/// echo コマンド: 引数をそのまま出力
fn cmd_echo(args: &str) {
    syscall::write_str(args);
    syscall::write_str("\n");
}

/// help コマンド: ヘルプを表示
fn cmd_help() {
    syscall::write_str("\n");
    syscall::write_str("SABOS User Shell - Available Commands\n");
    syscall::write_str("=====================================\n");
    syscall::write_str("\n");
    syscall::write_str("  echo <text>       - Print text to console\n");
    syscall::write_str("  help              - Show this help message\n");
    syscall::write_str("  clear             - Clear the screen\n");
    syscall::write_str("  exit              - Exit the shell\n");
    syscall::write_str("  ls [path]         - List directory contents\n");
    syscall::write_str("  cat <file>        - Display file contents\n");
    syscall::write_str("  write <file> <text> - Create/overwrite file\n");
    syscall::write_str("  rm <file>         - Delete file\n");
    syscall::write_str("  mkdir <dir>       - Create directory\n");
    syscall::write_str("  rmdir <dir>       - Remove empty directory\n");
    syscall::write_str("  cd <dir>          - Change current directory\n");
    syscall::write_str("  pwd               - Print current directory\n");
    syscall::write_str("  pushd <dir>       - Push directory and change to it\n");
    syscall::write_str("  popd              - Pop directory and change to it\n");
    syscall::write_str("  df                - Show filesystem usage (JSON)\n");
    syscall::write_str("  mem               - Show memory information\n");
    syscall::write_str("  ps                - Show task list\n");
    syscall::write_str("  top               - System monitor (real-time ps + mem)\n");
    syscall::write_str("  ip                - Show network information\n");
    syscall::write_str("  lspci             - List PCI devices\n");
    syscall::write_str("  run <file>        - Run ELF program (foreground)\n");
    syscall::write_str("  spawn <file>      - Run ELF program (background)\n");
    syscall::write_str("  kill <task_id>    - Kill a task by ID\n");
    syscall::write_str("  sleep <ms>        - Sleep for milliseconds\n");
    syscall::write_str("  dns <domain>      - DNS lookup\n");
    syscall::write_str("  http <host[:port]> [path] - HTTP GET request\n");
    syscall::write_str("  nc <host> <port>  - TCP connect (netcat client)\n");
    syscall::write_str("  nc -l <port>      - TCP listen (netcat server)\n");
    syscall::write_str("  sed [-n] s/OLD/NEW/[gp] <file> - Simple sed (literal)\n");
    syscall::write_str("  grep [-i] [-v] [-c] PATTERN [FILE] - Filter lines by pattern\n");
    syscall::write_str("  pipe (|)          - echo/cat/sed/grep pipeline\n");
    syscall::write_str("  gui <subcmd>      - Send GUI IPC commands\n");
    syscall::write_str("  rect x y w h r g b - Draw filled rectangle (GUI demo)\n");
    syscall::write_str("  cal <month> <year> - Show calendar for given month\n");
    syscall::write_str("  selftest          - Run kernel selftest\n");
    syscall::write_str("  halt              - Halt the system\n");
    syscall::write_str("\n");
}

/// clear コマンド: 画面をクリア
fn cmd_clear() {
    syscall::clear_screen();
}

/// exit コマンド: シェルを終了
fn cmd_exit() {
    syscall::write_str("Goodbye!\n");
    syscall::exit();
}

/// selftest コマンド: カーネル selftest を実行
fn cmd_selftest() {
    syscall::write_str("Running kernel selftest...\n");
    let _ = syscall::selftest();
}

// =================================================================
// ファイルシステムコマンド
// =================================================================

/// df コマンド: ファイルシステム使用量を表示
fn cmd_df() {
    let mut fs = match Fat32::new() {
        Ok(v) => v,
        Err(err) => {
            syscall::write_str("Error: Failed to init FAT32: ");
            syscall::write_str(err);
            syscall::write_str("\n");
            return;
        }
    };

    let total_clusters = fs.total_clusters();
    let free_clusters = match fs.free_clusters() {
        Ok(v) => v,
        Err(err) => {
            syscall::write_str("Error: Failed to scan FAT: ");
            syscall::write_str(err);
            syscall::write_str("\n");
            return;
        }
    };
    let cluster_bytes = fs.cluster_bytes() as u64;
    let total_bytes = total_clusters as u64 * cluster_bytes;
    let free_bytes = free_clusters as u64 * cluster_bytes;
    let used_bytes = total_bytes.saturating_sub(free_bytes);

    syscall::write_str("{\"fs\":\"fat32\",\"total_bytes\":");
    write_number(total_bytes);
    syscall::write_str(",\"used_bytes\":");
    write_number(used_bytes);
    syscall::write_str(",\"free_bytes\":");
    write_number(free_bytes);
    syscall::write_str(",\"cluster_bytes\":");
    write_number(cluster_bytes);
    syscall::write_str(",\"total_clusters\":");
    write_number(total_clusters as u64);
    syscall::write_str(",\"free_clusters\":");
    write_number(free_clusters as u64);
    syscall::write_str("}\n");
}

/// ls コマンド: ディレクトリ一覧を表示
fn cmd_ls(args: &str, state: &ShellState) {
    let target = args.trim();
    let (handle, need_close) = if target.is_empty() {
        (state.cwd_handle, false)
    } else {
        match open_dir_from_args(state, target) {
            Ok(h) => (h, true),
            Err(msg) => {
                syscall::write_str(msg);
                syscall::write_str("\n");
                return;
            }
        }
    };

    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let n = syscall::handle_enum(&handle, &mut buf);
    if n < 0 {
        syscall::write_str("Error: Failed to list directory\n");
        if need_close {
            let _ = syscall::handle_close(&handle);
        }
        return;
    }

    if n > 0 {
        let len = n as usize;
        syscall::write(&buf[..len]);
        if buf[len - 1] != b'\n' {
            syscall::write_str("\n");
        }
    }

    if need_close {
        let _ = syscall::handle_close(&handle);
    }
}

/// cat コマンド: ファイル内容を表示
fn cmd_cat(args: &str, state: &ShellState) {
    if args.is_empty() {
        syscall::write_str("Usage: cat <filename>\n");
        return;
    }

    let handle = match open_file_from_args(state, args.trim()) {
        Ok(h) => h,
        Err(msg) => {
            syscall::write_str(msg);
            syscall::write_str("\n");
            return;
        }
    };

    let mut buf = [0u8; 512];
    loop {
        let n = syscall::handle_read(&handle, &mut buf);
        if n < 0 {
            syscall::write_str("Error: File not found or cannot be read\n");
            break;
        }
        if n == 0 {
            break;
        }
        let len = n as usize;
        syscall::write(&buf[..len]);
    }

    let _ = syscall::handle_close(&handle);
}

/// sed の式をパースする
///
/// 形式: s/OLD/NEW/[gp]
fn parse_sed_expr(expr: &str) -> Result<(&str, &str, bool, bool), &'static str> {
    if !expr.starts_with("s/") {
        return Err("Error: only s/OLD/NEW/ is supported");
    }
    let rest = &expr[2..];
    let Some(pos1) = rest.find('/') else {
        return Err("Error: invalid sed expression");
    };
    let from = &rest[..pos1];
    let rest = &rest[pos1 + 1..];
    let Some(pos2) = rest.find('/') else {
        return Err("Error: invalid sed expression");
    };
    let to = &rest[..pos2];
    let flags = &rest[pos2 + 1..];

    let mut global = false;
    let mut print_on_change = false;
    for ch in flags.chars() {
        match ch {
            'g' => global = true,
            'p' => print_on_change = true,
            _ => return Err("Error: unsupported sed flag"),
        }
    }

    Ok((from, to, global, print_on_change))
}

/// sed の引数をパースする
///
/// 形式: [-n] s/OLD/NEW/[gp] <file>
fn parse_sed_args(args: &str) -> Result<(bool, &str, &str), &'static str> {
    let mut parts = args.split_whitespace();
    let mut suppress = false;
    let first = parts.next().unwrap_or("");
    let (expr, file) = if first == "-n" {
        suppress = true;
        (parts.next().unwrap_or(""), parts.next().unwrap_or(""))
    } else {
        (first, parts.next().unwrap_or(""))
    };
    if parts.next().is_some() {
        return Err("Usage: sed [-n] s/OLD/NEW/[gp] <file>");
    }
    if expr.is_empty() {
        return Err("Usage: sed [-n] s/OLD/NEW/[gp] <file>");
    }
    Ok((suppress, expr, file))
}

/// sed の変換を適用する
fn sed_apply(
    text: &str,
    from: &str,
    to: &str,
    global: bool,
    print_on_change: bool,
    suppress: bool,
) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i <= text.len() {
        let at_end = i == text.len();
        let is_nl = !at_end && text.as_bytes()[i] == b'\n';
        if at_end || is_nl {
            let mut line = &text[start..i];
            if let Some(b'\r') = line.as_bytes().last().copied() {
                line = &line[..line.len().saturating_sub(1)];
            }
            let (line_out, changed) = replace_literal(line, from, to, global);
            let should_print = if suppress { print_on_change && changed } else { true };
            if should_print {
                out.push_str(line_out.as_str());
                out.push('\n');
            }
            start = i + 1;
        }
        i += 1;
    }
    Ok(out)
}

/// ハンドルから全データを読み取る
fn read_all_handle(handle: &syscall::Handle) -> Result<Vec<u8>, syscall::SyscallResult> {
    let mut out = Vec::new();
    let mut buf = [0u8; FILE_BUFFER_SIZE];
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

/// sed コマンド: 簡易的なリテラル置換
///
/// 使い方:
///   sed [-n] s/OLD/NEW/[gp] <file>
///
/// - 置換は正規表現ではなくリテラル一致
/// - g フラグで全置換、p フラグで置換成功時に出力
/// - -n を指定すると自動出力を抑制する
fn cmd_sed(args: &str, state: &ShellState) {
    let (suppress, expr, file) = match parse_sed_args(args) {
        Ok(v) => v,
        Err(msg) => {
            syscall::write_str(msg);
            syscall::write_str("\n");
            return;
        }
    };
    if file.is_empty() {
        syscall::write_str("Usage: sed [-n] s/OLD/NEW/[gp] <file>\n");
        return;
    }

    let (from, to, global, print_on_change) = match parse_sed_expr(expr) {
        Ok(v) => v,
        Err(msg) => {
            syscall::write_str(msg);
            syscall::write_str("\n");
            return;
        }
    };

    if from.is_empty() {
        syscall::write_str("Error: empty search pattern is not supported\n");
        return;
    }

    let handle = match open_file_from_args(state, file) {
        Ok(h) => h,
        Err(msg) => {
            syscall::write_str(msg);
            syscall::write_str("\n");
            return;
        }
    };

    let data = match read_all_handle(&handle) {
        Ok(v) => v,
        Err(_) => {
            let _ = syscall::handle_close(&handle);
            syscall::write_str("Error: Failed to read file\n");
            return;
        }
    };
    let _ = syscall::handle_close(&handle);
    let Ok(text) = core::str::from_utf8(&data) else {
        syscall::write_str("Error: invalid UTF-8 in input\n");
        return;
    };
    let out = match sed_apply(text, from, to, global, print_on_change, suppress) {
        Ok(v) => v,
        Err(msg) => {
            syscall::write_str(msg);
            syscall::write_str("\n");
            return;
        }
    };
    syscall::write_str(out.as_str());
}

/// grep の引数をパースする
///
/// 書式: grep [-i] [-v] [-c] PATTERN [FILE]
/// - -i: 大文字小文字を無視
/// - -v: マッチしない行を出力
/// - -c: マッチした行数を出力
struct GrepOpts<'a> {
    case_insensitive: bool,
    invert: bool,
    count_only: bool,
    pattern: &'a str,
    file: &'a str,
}

fn parse_grep_args(args: &str) -> Result<GrepOpts<'_>, &'static str> {
    let mut case_insensitive = false;
    let mut invert = false;
    let mut count_only = false;
    let mut rest = args;

    // オプションを先に消化する
    loop {
        rest = rest.trim_start();
        if rest.starts_with("-") {
            let (opt, after) = split_command(rest);
            match opt {
                "-i" => case_insensitive = true,
                "-v" => invert = true,
                "-c" => count_only = true,
                "-iv" | "-vi" => { case_insensitive = true; invert = true; }
                "-ic" | "-ci" => { case_insensitive = true; count_only = true; }
                "-vc" | "-cv" => { invert = true; count_only = true; }
                "-ivc" | "-icv" | "-vic" | "-vci" | "-civ" | "-cvi" => {
                    case_insensitive = true; invert = true; count_only = true;
                }
                _ => return Err("Error: unknown option"),
            }
            rest = after;
        } else {
            break;
        }
    }

    // PATTERN と FILE を取得
    let (pattern, file) = split_command(rest);
    if pattern.is_empty() {
        return Err("Usage: grep [-i] [-v] [-c] PATTERN [FILE]");
    }

    Ok(GrepOpts { case_insensitive, invert, count_only, pattern, file })
}

/// テキストに grep フィルタを適用する
///
/// 行ごとにパターンの含有を判定し、条件に合う行を出力する。
/// -c 指定時はマッチ行数のみ返す。
fn grep_apply(
    text: &str,
    pattern: &str,
    case_insensitive: bool,
    invert: bool,
    count_only: bool,
) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    while i <= text.len() {
        let at_end = i == text.len();
        let is_nl = !at_end && text.as_bytes()[i] == b'\n';
        if at_end || is_nl {
            let mut line = &text[start..i];
            // CRLF の \r を除去
            if let Some(b'\r') = line.as_bytes().last().copied() {
                line = &line[..line.len().saturating_sub(1)];
            }
            // 空文字列の最後（EOF で行が空）はスキップ
            if !(at_end && line.is_empty()) {
                let matched = contains_literal(line, pattern, case_insensitive);
                let should_print = if invert { !matched } else { matched };
                if should_print {
                    if count_only {
                        count += 1;
                    } else {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
            start = i + 1;
        }
        i += 1;
    }
    if count_only {
        out.push_str(&format!("{}\n", count));
    }
    out
}

/// grep コマンド: パターンに一致する行を出力
///
/// 使い方:
///   grep [-i] [-v] [-c] PATTERN FILE
///
/// - リテラル一致（正規表現ではない）
/// - -i: 大文字小文字を無視
/// - -v: マッチしない行を出力
/// - -c: マッチした行数を出力
fn cmd_grep(args: &str, state: &ShellState) {
    let opts = match parse_grep_args(args) {
        Ok(v) => v,
        Err(msg) => {
            syscall::write_str(msg);
            syscall::write_str("\n");
            return;
        }
    };
    if opts.file.is_empty() {
        syscall::write_str("Usage: grep [-i] [-v] [-c] PATTERN FILE\n");
        return;
    }

    let handle = match open_file_from_args(state, opts.file) {
        Ok(h) => h,
        Err(msg) => {
            syscall::write_str(msg);
            syscall::write_str("\n");
            return;
        }
    };

    let data = match read_all_handle(&handle) {
        Ok(v) => v,
        Err(_) => {
            let _ = syscall::handle_close(&handle);
            syscall::write_str("Error: Failed to read file\n");
            return;
        }
    };
    let _ = syscall::handle_close(&handle);
    let Ok(text) = core::str::from_utf8(&data) else {
        syscall::write_str("Error: invalid UTF-8 in input\n");
        return;
    };

    let out = grep_apply(text, opts.pattern, opts.case_insensitive, opts.invert, opts.count_only);
    syscall::write_str(&out);
}

/// パイプライン用 grep
fn pipeline_grep(args: &str, state: &ShellState, input: Option<&str>) -> Result<String, &'static str> {
    let opts = parse_grep_args(args)?;

    let owned = if !opts.file.is_empty() {
        let handle = open_file_from_args(state, opts.file)?;
        let data = read_all_handle(&handle).map_err(|_| "Error: Failed to read file")?;
        let _ = syscall::handle_close(&handle);
        let Ok(text) = core::str::from_utf8(&data) else {
            return Err("Error: invalid UTF-8 in input");
        };
        Some(String::from(text))
    } else {
        None
    };

    let text = if let Some(ref owned_text) = owned {
        owned_text.as_str()
    } else if let Some(text) = input {
        text
    } else {
        return Err("Usage: grep [-i] [-v] [-c] PATTERN [FILE]");
    };

    Ok(grep_apply(text, opts.pattern, opts.case_insensitive, opts.invert, opts.count_only))
}

/// write コマンド: ファイルを作成/上書き
fn cmd_write(args: &str, state: &ShellState) {
    // ファイル名とデータを分割
    let (filename, data) = split_command(args);

    if filename.is_empty() {
        syscall::write_str("Usage: write <filename> <text>\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, filename);

    let mut fs = match Fat32::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT32 not available\n");
            return;
        }
    };

    if fs.create_file(&abs_path, data.as_bytes()).is_err() {
        syscall::write_str("Error: Failed to write file\n");
        return;
    }

    syscall::write_str("File written successfully\n");
}

/// rm コマンド: ファイルを削除
fn cmd_rm(args: &str, state: &ShellState) {
    if args.is_empty() {
        syscall::write_str("Usage: rm <filename>\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, args);

    let mut fs = match Fat32::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT32 not available\n");
            return;
        }
    };

    if fs.delete_file(&abs_path).is_err() {
        syscall::write_str("Error: Failed to delete file\n");
        return;
    }

    syscall::write_str("File deleted successfully\n");
}

/// mkdir コマンド: ディレクトリを作成
fn cmd_mkdir(args: &str, state: &ShellState) {
    let name = args.trim();
    if name.is_empty() {
        syscall::write_str("Usage: mkdir <dirname>\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, name);

    let mut fs = match Fat32::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT32 not available\n");
            return;
        }
    };

    if fs.create_dir(&abs_path).is_err() {
        syscall::write_str("Error: Failed to create directory\n");
        return;
    }

    syscall::write_str("Directory created successfully\n");
}

/// rmdir コマンド: 空のディレクトリを削除
fn cmd_rmdir(args: &str, state: &ShellState) {
    let name = args.trim();
    if name.is_empty() {
        syscall::write_str("Usage: rmdir <dirname>\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, name);

    let mut fs = match Fat32::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT32 not available\n");
            return;
        }
    };

    if fs.remove_dir(&abs_path).is_err() {
        syscall::write_str("Error: Failed to remove directory\n");
        return;
    }

    syscall::write_str("Directory removed successfully\n");
}

/// cd コマンド: カレントディレクトリを変更
fn cmd_cd(args: &str, state: &mut ShellState) {
    let target = args.trim();
    if target.is_empty() || target == "/" {
        // ルートに戻る（スタックは破棄）
        close_handle_stack(state);
        if let Ok(new_root) = open_root_dir() {
            let _ = syscall::handle_close(&state.cwd_handle);
            state.cwd_handle = new_root;
            state.cwd_text = String::from("/");
        } else {
            syscall::write_str("Error: Failed to open root directory\n");
        }
        return;
    }

    if target == ".." || target == "-" {
        // スタックから戻る
        if let Some(prev_handle) = state.cwd_stack.pop() {
            if let Some(prev_text) = state.cwd_text_stack.pop() {
                let _ = syscall::handle_close(&state.cwd_handle);
                state.cwd_handle = prev_handle;
                state.cwd_text = prev_text;
            }
        } else {
            syscall::write_str("Error: No previous directory\n");
        }
        return;
    }

    // 新しいディレクトリを開く
    let new_handle = match open_dir_from_args(state, target) {
        Ok(h) => h,
        Err(msg) => {
            syscall::write_str(msg);
            syscall::write_str("\n");
            return;
        }
    };

    // ディレクトリかどうか確認（ENUM できるか）
    let mut buf = [0u8; 8];
    if syscall::handle_enum(&new_handle, &mut buf) < 0 {
        let _ = syscall::handle_close(&new_handle);
        syscall::write_str("Error: Not a directory\n");
        return;
    }

    // 現在の cwd をスタックに保存して切り替え
    state.cwd_stack.push(state.cwd_handle);
    state.cwd_text_stack.push(state.cwd_text.clone());
    state.cwd_handle = new_handle;
    state.cwd_text = resolve_path(&state.cwd_text, target);
}

/// pwd コマンド: カレントディレクトリを表示
fn cmd_pwd(state: &ShellState) {
    syscall::write_str(&state.cwd_text);
    syscall::write_str("\n");
}

/// pushd コマンド: ディレクトリスタックに積んで移動
fn cmd_pushd(args: &str, state: &mut ShellState) {
    let target = args.trim();
    if target.is_empty() {
        syscall::write_str("Usage: pushd <dir>\n");
        return;
    }

    let new_handle = match open_dir_from_args(state, target) {
        Ok(h) => h,
        Err(msg) => {
            syscall::write_str(msg);
            syscall::write_str("\n");
            return;
        }
    };

    // ディレクトリかどうか確認（ENUM できるか）
    let mut buf = [0u8; 8];
    if syscall::handle_enum(&new_handle, &mut buf) < 0 {
        let _ = syscall::handle_close(&new_handle);
        syscall::write_str("Error: Not a directory\n");
        return;
    }

    state.cwd_stack.push(state.cwd_handle);
    state.cwd_text_stack.push(state.cwd_text.clone());
    state.cwd_handle = new_handle;
    state.cwd_text = resolve_path(&state.cwd_text, target);
}

/// popd コマンド: ディレクトリスタックから戻る
fn cmd_popd(state: &mut ShellState) {
    if let Some(prev_handle) = state.cwd_stack.pop() {
        if let Some(prev_text) = state.cwd_text_stack.pop() {
            let _ = syscall::handle_close(&state.cwd_handle);
            state.cwd_handle = prev_handle;
            state.cwd_text = prev_text;
        }
    } else {
        syscall::write_str("Error: No previous directory\n");
    }
}

// =================================================================
// システム情報コマンド
// =================================================================

/// ファイル読み取り/ディレクトリ一覧用のバッファサイズ
const FILE_BUFFER_SIZE: usize = 4096;

/// mem コマンド: メモリ情報を表示
///
/// カーネルからメモリ情報を取得して表示する。
fn cmd_mem() {
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::get_mem_info(&mut buf);

    if result < 0 {
        syscall::write_str("Error: Failed to get memory info\n");
        return;
    }

    syscall::write_str("Memory Information:\n");

    // 結果をパースして表示（JSON）
    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        let total = json::json_find_u64(s, "total_frames");
        let allocated = json::json_find_u64(s, "allocated_frames");
        let free = json::json_find_u64(s, "free_frames");
        let free_kib = json::json_find_u64(s, "free_kib");

        if let Some(v) = total {
            syscall::write_str("  Total frames:     ");
            write_number(v);
            syscall::write_str("\n");
        }
        if let Some(v) = allocated {
            syscall::write_str("  Allocated frames: ");
            write_number(v);
            syscall::write_str("\n");
        }
        if let Some(v) = free {
            syscall::write_str("  Free frames:      ");
            write_number(v);
            syscall::write_str("\n");
        }
        if let Some(v) = free_kib {
            syscall::write_str("  Free memory:      ");
            write_number(v);
            syscall::write_str(" KiB\n");
        }
    }
}

/// ps コマンド: タスク一覧を表示
///
/// カーネルからタスク情報を取得して表示する。
fn cmd_ps() {
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::get_task_list(&mut buf);

    if result < 0 {
        syscall::write_str("Error: Failed to get task list\n");
        return;
    }

    // 結果を表示（JSON 形式をテーブル形式に変換）
    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        // ヘッダを表示
        syscall::write_str("  ID  STATE       TYPE    NAME\n");
        syscall::write_str("  --  ----------  ------  ----------\n");

        let Some((tasks_start, tasks_end)) = json::json_find_array_bounds(s, "tasks") else {
            return;
        };

        let mut i = tasks_start;
        while i < tasks_end {
            // 次のオブジェクト開始を探す
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
            let state = json::json_find_str(obj, "state");
            let ty = json::json_find_str(obj, "type");
            let name = json::json_find_str(obj, "name");

            if let (Some(id), Some(state), Some(ty), Some(name)) = (id, state, ty, name) {
                syscall::write_str("  ");
                write_number(id);
                syscall::write_str("  ");
                write_padded(state, 10);
                syscall::write_str("  ");
                write_padded(ty, 6);
                syscall::write_str("  ");
                syscall::write_str(name);
                syscall::write_str("\n");
            }

            i = obj_end + 1;
        }
    }
}

/// top コマンド: リアルタイムシステムモニター
///
/// タスク一覧とメモリ情報を1秒間隔でリフレッシュ表示する。
/// 'q' / 'Q' / ESC キーで終了。
/// nc コマンドと同じく console_grab + key_read のポーリングパターンを使う。
fn cmd_top() {
    // キーボードフォーカスを取得（GUI 環境で top がキーを読めるように）
    syscall::console_grab(true);

    loop {
        // 画面クリアしてヘッダを表示
        syscall::clear_screen();
        syscall::write_str("SABOS top - press 'q' to quit\n");
        syscall::write_str("================================================\n");

        // メモリ情報を1行サマリーで表示
        top_display_mem();

        syscall::write_str("\n");

        // タスク一覧をテーブル形式で表示
        top_display_tasks();

        // 1秒のポーリング: 100ms × 10回、毎回 key_read で終了キーをチェック
        let mut quit = false;
        for _ in 0..10 {
            let mut key_buf = [0u8; 16];
            let key_n = syscall::key_read(&mut key_buf);
            if key_n > 0 {
                for j in 0..(key_n as usize) {
                    if key_buf[j] == b'q' || key_buf[j] == b'Q' || key_buf[j] == 0x1b {
                        quit = true;
                        break;
                    }
                }
            }
            if quit {
                break;
            }
            syscall::sleep(100);
        }
        if quit {
            break;
        }
    }

    syscall::console_grab(false);
}

/// top 用: メモリ情報を1行サマリーで表示
///
/// println! マクロを活用して、フォーマット文字列で一発出力する。
/// "Memory: {total} total / {alloc} allocated / {free} free ({kib} KiB free)"
fn top_display_mem() {
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::get_mem_info(&mut buf);
    if result < 0 {
        println!("Memory: (error)");
        return;
    }

    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        let total = json::json_find_u64(s, "total_frames").unwrap_or(0);
        let allocated = json::json_find_u64(s, "allocated_frames").unwrap_or(0);
        let free = json::json_find_u64(s, "free_frames").unwrap_or(0);
        let free_kib = json::json_find_u64(s, "free_kib").unwrap_or(0);

        println!("Memory: {} total / {} allocated / {} free ({} KiB free)",
            total, allocated, free, free_kib);
    }
}

/// top 用: タスク一覧をテーブル形式で表示
///
/// cmd_ps() と同じパースロジックでテーブルを出力する。
fn top_display_tasks() {
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::get_task_list(&mut buf);
    if result < 0 {
        syscall::write_str("Tasks: (error)\n");
        return;
    }

    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        // ヘッダを表示
        syscall::write_str("  ID  STATE       TYPE    NAME\n");
        syscall::write_str("  --  ----------  ------  ----------\n");

        let Some((tasks_start, tasks_end)) = json::json_find_array_bounds(s, "tasks") else {
            return;
        };

        let mut i = tasks_start;
        while i < tasks_end {
            // 次のオブジェクト開始を探す
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
            let state = json::json_find_str(obj, "state");
            let ty = json::json_find_str(obj, "type");
            let name = json::json_find_str(obj, "name");

            if let (Some(id), Some(state), Some(ty), Some(name)) = (id, state, ty, name) {
                syscall::write_str("  ");
                write_number(id);
                syscall::write_str("  ");
                write_padded(state, 10);
                syscall::write_str("  ");
                write_padded(ty, 6);
                syscall::write_str("  ");
                syscall::write_str(name);
                syscall::write_str("\n");
            }

            i = obj_end + 1;
        }
    }
}

/// ip コマンド: ネットワーク情報を表示
///
/// カーネルからネットワーク情報を取得して表示する。
fn cmd_ip() {
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::get_net_info(&mut buf);

    if result < 0 {
        syscall::write_str("Error: Failed to get network info\n");
        return;
    }

    syscall::write_str("IP Configuration:\n");

    // 結果をパースして表示
    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        for line in s.lines() {
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "ip" => {
                        syscall::write_str("  IP Address: ");
                        syscall::write_str(value);
                        syscall::write_str("\n");
                    }
                    "gateway" => {
                        syscall::write_str("  Gateway:    ");
                        syscall::write_str(value);
                        syscall::write_str("\n");
                    }
                    "dns" => {
                        syscall::write_str("  DNS:        ");
                        syscall::write_str(value);
                        syscall::write_str("\n");
                    }
                    "mac" => {
                        syscall::write_str("  MAC:        ");
                        syscall::write_str(value);
                        syscall::write_str("\n");
                    }
                    _ => {}
                }
            }
        }
    }
}

/// lspci コマンド: PCI デバイス一覧を表示
///
/// PCI Configuration Space を読み取って、バス 0 のデバイスを列挙する。
/// 直接 I/O ポートを叩かず、システムコール経由で読み取る。
fn cmd_lspci() {
    syscall::write_str("PCI devices on bus 0:\n");
    syscall::write_str("  BDF       Vendor Device Class\n");
    syscall::write_str("  --------- ------ ------ --------\n");

    let mut count: u64 = 0;

    for device in 0..32u8 {
        // ファンクション 0 のベンダー ID で存在確認
        let vendor0 = match pci_config_read_u16(0, device, 0, 0x00) {
            Some(v) => v,
            None => {
                syscall::write_str("Error: PCI config read failed\n");
                return;
            }
        };
        if vendor0 == 0xFFFF {
            continue;
        }

        // ヘッダータイプでマルチファンクション判定
        let header_type = match pci_config_read_u8(0, device, 0, 0x0E) {
            Some(v) => v,
            None => {
                syscall::write_str("Error: PCI config read failed\n");
                return;
            }
        };
        let is_multi = (header_type & 0x80) != 0;
        let max_func = if is_multi { 8 } else { 1 };

        for function in 0..max_func {
            let vendor_id = match pci_config_read_u16(0, device, function, 0x00) {
                Some(v) => v,
                None => {
                    syscall::write_str("Error: PCI config read failed\n");
                    return;
                }
            };
            if vendor_id == 0xFFFF {
                continue;
            }

            let device_id = match pci_config_read_u16(0, device, function, 0x02) {
                Some(v) => v,
                None => {
                    syscall::write_str("Error: PCI config read failed\n");
                    return;
                }
            };

            let class_reg = match pci_config_read_u32(0, device, function, 0x08) {
                Some(v) => v,
                None => {
                    syscall::write_str("Error: PCI config read failed\n");
                    return;
                }
            };
            let class_code = ((class_reg >> 24) & 0xFF) as u8;
            let subclass = ((class_reg >> 16) & 0xFF) as u8;
            let prog_if = ((class_reg >> 8) & 0xFF) as u8;

            syscall::write_str("  ");
            write_hex_u8(0);
            syscall::write_str(":");
            write_hex_u8(device);
            syscall::write_str(".");
            write_number(function as u64);
            syscall::write_str("   ");
            write_hex_u16(vendor_id);
            syscall::write_str("   ");
            write_hex_u16(device_id);
            syscall::write_str("   ");
            write_hex_u8(class_code);
            syscall::write_str(":");
            write_hex_u8(subclass);
            syscall::write_str(".");
            write_hex_u8(prog_if);
            syscall::write_str("\n");

            count += 1;
        }
    }

    syscall::write_str("  Total: ");
    write_number(count);
    syscall::write_str(" devices\n");
}

// =================================================================
// プロセス実行コマンド
// =================================================================

/// run コマンド: ELF プログラムをフォアグラウンドで実行
///
/// 指定した ELF ファイルを読み込んで同期実行する。
/// プログラムが終了するまでシェルはブロックする。
fn cmd_run(args: &str, state: &ShellState) {
    let filename = args.trim();
    if filename.is_empty() {
        syscall::write_str("Usage: run <FILENAME>\n");
        syscall::write_str("  Example: run HELLO.ELF\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, filename);

    syscall::write_str("Running ");
    syscall::write_str(&abs_path);
    syscall::write_str("...\n");

    let result = syscall::exec(&abs_path);

    if result < 0 {
        syscall::write_str("Error: Failed to run program\n");
        return;
    }

    syscall::write_str("Program exited.\n");
}

/// spawn コマンド: ELF プログラムをバックグラウンドで実行
///
/// 指定した ELF ファイルを読み込んでバックグラウンドで実行する。
/// 即座にシェルに戻り、プログラムはスケジューラで管理される。
fn cmd_spawn(args: &str, state: &ShellState) {
    let filename = args.trim();
    if filename.is_empty() {
        syscall::write_str("Usage: spawn <FILENAME>\n");
        syscall::write_str("  Example: spawn HELLO.ELF\n");
        syscall::write_str("  The process runs in the background. Use 'ps' to see tasks.\n");
        return;
    }

    let abs_path = resolve_path(&state.cwd_text, filename);

    syscall::write_str("Spawning ");
    syscall::write_str(&abs_path);
    syscall::write_str("...\n");

    let result = syscall::spawn(&abs_path);

    if result < 0 {
        syscall::write_str("Error: Failed to spawn process\n");
        return;
    }

    syscall::write_str("Process spawned as task ");
    write_number(result as u64);
    syscall::write_str(" (background)\n");
    syscall::write_str("Use 'ps' to see running tasks.\n");
}

/// kill コマンド: タスクを強制終了
///
/// 使い方:
///   kill <task_id>
///
/// ps コマンドでタスク ID を確認してから使う。
/// 自分自身（シェル）の kill はカーネル側で拒否される。
fn cmd_kill(args: &str) {
    let id_str = args.trim();
    if id_str.is_empty() {
        syscall::write_str("Usage: kill <task_id>\n");
        syscall::write_str("  Use 'ps' to see task IDs.\n");
        return;
    }

    let task_id = match parse_u64(id_str) {
        Some(id) => id,
        None => {
            syscall::write_str("Error: invalid task ID\n");
            return;
        }
    };

    let result = syscall::kill(task_id);
    if result == 0 {
        syscall::write_str("Task ");
        write_number(task_id);
        syscall::write_str(" killed.\n");
    } else {
        syscall::write_str("Error: failed to kill task ");
        write_number(task_id);
        syscall::write_str(" (error ");
        write_number((-result) as u64);
        syscall::write_str(")\n");
    }
}

/// sleep コマンド: 指定ミリ秒スリープ
fn cmd_sleep(args: &str) {
    let ms_str = args.trim();
    if ms_str.is_empty() {
        syscall::write_str("Usage: sleep <milliseconds>\n");
        syscall::write_str("  Example: sleep 1000  (sleep for 1 second)\n");
        return;
    }

    // 文字列を数値に変換
    let ms = match parse_u64(ms_str) {
        Some(n) => n,
        None => {
            syscall::write_str("Error: Invalid number\n");
            return;
        }
    };

    syscall::write_str("Sleeping for ");
    write_number(ms);
    syscall::write_str(" ms...\n");

    syscall::sleep(ms);

    syscall::write_str("Done.\n");
}

// =================================================================
// ユーティリティ関数
// =================================================================

/// 数値を文字列として出力
fn write_number(n: u64) {
    if n == 0 {
        syscall::write_str("0");
        return;
    }

    // 数字を逆順に格納
    let mut buf = [0u8; 20];  // u64 最大は 20 桁
    let mut i = 0;
    let mut num = n;

    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }

    // 逆順に出力
    while i > 0 {
        i -= 1;
        syscall::write(&[buf[i]]);
    }
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

/// 文字列を指定幅で出力（左寄せ、スペースで埋める）
fn write_padded(s: &str, width: usize) {
    syscall::write_str(s);
    let len = s.len();
    if len < width {
        for _ in 0..(width - len) {
            syscall::write_str(" ");
        }
    }
}

/// 16 進数の 1 桁を出力
fn write_hex_digit(v: u8) {
    let c = if v < 10 { b'0' + v } else { b'a' + (v - 10) };
    syscall::write(&[c]);
}

/// 16 進数の 2 桁を出力（u8）
fn write_hex_u8(v: u8) {
    write_hex_digit((v >> 4) & 0x0F);
    write_hex_digit(v & 0x0F);
}

/// 16 進数の 4 桁を出力（u16）
fn write_hex_u16(v: u16) {
    write_hex_u8((v >> 8) as u8);
    write_hex_u8((v & 0xFF) as u8);
}

/// PCI Configuration Space を 1 バイト読み取る
fn pci_config_read_u8(bus: u8, device: u8, function: u8, offset: u8) -> Option<u8> {
    let result = syscall::pci_config_read(bus, device, function, offset, 1);
    if result < 0 {
        None
    } else {
        Some(result as u8)
    }
}

/// PCI Configuration Space を 2 バイト読み取る
fn pci_config_read_u16(bus: u8, device: u8, function: u8, offset: u8) -> Option<u16> {
    let result = syscall::pci_config_read(bus, device, function, offset, 2);
    if result < 0 {
        None
    } else {
        Some(result as u16)
    }
}

/// PCI Configuration Space を 4 バイト読み取る
fn pci_config_read_u32(bus: u8, device: u8, function: u8, offset: u8) -> Option<u32> {
    let result = syscall::pci_config_read(bus, device, function, offset, 4);
    if result < 0 {
        None
    } else {
        Some(result as u32)
    }
}

// =================================================================
// ネットワークコマンド
// =================================================================

/// dns コマンド: DNS 解決
///
/// ドメイン名を IP アドレスに解決する。
fn cmd_dns(args: &str) {
    let domain = args.trim();
    if domain.is_empty() {
        syscall::write_str("Usage: dns <domain>\n");
        syscall::write_str("  Example: dns example.com\n");
        return;
    }

    syscall::write_str("Resolving '");
    syscall::write_str(domain);
    syscall::write_str("'...\n");

    let mut ip = [0u8; 4];
    if netd_dns_lookup(domain, &mut ip).is_err() {
        syscall::write_str("Error: DNS lookup failed\n");
        return;
    }

    syscall::write_str(domain);
    syscall::write_str(" -> ");
    write_ip(&ip);
    syscall::write_str("\n");
}

/// http コマンド: HTTP GET リクエスト
///
/// 指定したホストに HTTP GET リクエストを送信し、レスポンスを表示する。
fn cmd_http(args: &str) {
    // 引数をパース: host[:port] [path]
    let (host_arg, path) = split_command(args);

    if host_arg.is_empty() {
        syscall::write_str("Usage: http <host[:port]> [path]\n");
        syscall::write_str("  Example: http example.com /\n");
        syscall::write_str("  Example: http localhost:8080 /\n");
        return;
    }

    let path = if path.is_empty() { "/" } else { path };

    // host:port を分離する
    let (host, port) = if let Some(colon_pos) = host_arg.rfind(':') {
        // ":" の右側が数字ならポート指定
        let maybe_port = &host_arg[colon_pos + 1..];
        if !maybe_port.is_empty() && maybe_port.as_bytes().iter().all(|b| b.is_ascii_digit()) {
            let port_num = parse_u16(maybe_port).unwrap_or(80);
            (&host_arg[..colon_pos], port_num)
        } else {
            (host_arg, 80u16)
        }
    } else {
        (host_arg, 80u16)
    };

    // IP アドレスを解決または直接パース
    // "localhost" は自分自身の IP として扱う
    let ip = if host == "localhost" {
        [10, 0, 2, 15] // MY_IP — ループバックで折り返される
    } else {
        match parse_ip(host) {
            Some(ip) => ip,
            None => {
                // DNS で解決
                syscall::write_str("Resolving ");
                syscall::write_str(host);
                syscall::write_str("...\n");

                let mut resolved_ip = [0u8; 4];
                if netd_dns_lookup(host, &mut resolved_ip).is_err() {
                    syscall::write_str("Error: DNS lookup failed\n");
                    return;
                }

                syscall::write_str("Resolved to ");
                write_ip(&resolved_ip);
                syscall::write_str("\n");
                resolved_ip
            }
        }
    };

    // TCP 接続
    syscall::write_str("Connecting to ");
    write_ip(&ip);
    syscall::write_str(":");
    write_number(port as u64);
    syscall::write_str("...\n");

    let conn_id = match netd_tcp_connect(&ip, port) {
        Ok(id) => id,
        Err(_) => {
            syscall::write_str("Error: TCP connect failed\n");
            return;
        }
    };
    syscall::write_str("Connected!\n");

    // HTTP リクエストを構築
    syscall::write_str("Sending HTTP request...\n");

    // GET line — httpd が HTTP/1.1 を期待するので 1.1 にする
    let _ = netd_tcp_send(conn_id, b"GET ");
    let _ = netd_tcp_send(conn_id, path.as_bytes());
    let _ = netd_tcp_send(conn_id, b" HTTP/1.1\r\n");

    // Host header
    let _ = netd_tcp_send(conn_id, b"Host: ");
    let _ = netd_tcp_send(conn_id, host.as_bytes());
    let _ = netd_tcp_send(conn_id, b"\r\n");

    // Connection header and end of headers
    let _ = netd_tcp_send(conn_id, b"Connection: close\r\n\r\n");

    // レスポンスを受信
    syscall::write_str("Receiving response...\n");
    syscall::write_str("--- Response ---\n");

    let mut buf = [0u8; 1024];
    loop {
        let n = match netd_tcp_recv(conn_id, &mut buf, 5000) {
            Ok(n) => n,
            Err(_) => -1,
        };
        if n <= 0 {
            break;
        }
        let n = n as usize;
        // UTF-8 として表示（バイナリデータも可能な限り表示）
        if let Ok(text) = core::str::from_utf8(&buf[..n]) {
            syscall::write_str(text);
        } else {
            syscall::write_str("[binary data]");
        }
    }

    syscall::write_str("\n--- End ---\n");

    // 接続を閉じる
    let _ = netd_tcp_close(conn_id);
}

/// nc コマンド: TCP の生データ送受信（netcat 風）
///
/// ## 使い方
/// - クライアントモード: `nc <host> <port>` — 指定ホスト:ポートに接続
/// - サーバーモード: `nc -l <port>` — 指定ポートで待ち受け
///
/// キーボード入力をそのまま送信し、受信データをそのまま表示する。
/// 相手が切断（recv が 0 またはエラー）すると終了する。
fn cmd_nc(args: &str) {
    let args = args.trim();
    if args.is_empty() {
        syscall::write_str("Usage: nc <host> <port>    (client mode)\n");
        syscall::write_str("       nc -l <port>        (server mode)\n");
        return;
    }

    // 引数パース: "-l" で始まればサーバーモード
    let (first, rest) = split_command(args);

    if first == "-l" {
        // サーバーモード
        let port_str = rest.trim();
        let port = match parse_u16(port_str) {
            Some(p) if p > 0 => p,
            _ => {
                syscall::write_str("Error: invalid port number\n");
                return;
            }
        };
        nc_server(port);
    } else {
        // クライアントモード: nc <host> <port>
        let host = first;
        let port_str = rest.trim();
        if port_str.is_empty() {
            syscall::write_str("Usage: nc <host> <port>\n");
            return;
        }
        let port = match parse_u16(port_str) {
            Some(p) if p > 0 => p,
            _ => {
                syscall::write_str("Error: invalid port number\n");
                return;
            }
        };
        nc_client(host, port);
    }
}

/// nc クライアントモード: 指定ホスト:ポートに TCP 接続して双方向中継する
fn nc_client(host: &str, port: u16) {
    // IP アドレスを解決
    let ip = if host == "localhost" {
        [10, 0, 2, 15] // QEMU 環境での自分自身の IP
    } else {
        match parse_ip(host) {
            Some(ip) => ip,
            None => {
                // DNS で解決
                syscall::write_str("Resolving ");
                syscall::write_str(host);
                syscall::write_str("...\n");
                let mut resolved_ip = [0u8; 4];
                if netd_dns_lookup(host, &mut resolved_ip).is_err() {
                    syscall::write_str("Error: DNS lookup failed\n");
                    return;
                }
                resolved_ip
            }
        }
    };

    syscall::write_str("Connecting to ");
    write_ip(&ip);
    syscall::write_str(":");
    write_number(port as u64);
    syscall::write_str("...\n");

    let conn_id = match netd_tcp_connect(&ip, port) {
        Ok(id) => id,
        Err(_) => {
            syscall::write_str("Error: TCP connect failed\n");
            return;
        }
    };
    syscall::write_str("Connected! (peer will close to exit)\n");

    // キーボード入力↔ソケットの双方向中継
    nc_relay_loop(conn_id);

    let _ = netd_tcp_close(conn_id);
    syscall::write_str("Connection closed.\n");
}

/// nc サーバーモード: 指定ポートで待ち受けて双方向中継する
fn nc_server(port: u16) {
    syscall::write_str("Listening on port ");
    write_number(port as u64);
    syscall::write_str("...\n");

    if netd_tcp_listen(port).is_err() {
        syscall::write_str("Error: listen failed\n");
        return;
    }

    // accept で接続を待つ（タイムアウト 0 = ブロッキング）
    let conn_id = match netd_tcp_accept(0) {
        Ok(id) => id,
        Err(_) => {
            syscall::write_str("Error: accept failed\n");
            return;
        }
    };
    syscall::write_str("Client connected! (peer will close to exit)\n");

    // キーボード入力↔ソケットの双方向中継
    nc_relay_loop(conn_id);

    let _ = netd_tcp_close(conn_id);
    syscall::write_str("Connection closed.\n");
}

/// nc のメインループ: キーボード入力を送信し、受信データを表示する
///
/// ノンブロッキング key_read でキーボードをポーリングし、
/// 短いタイムアウトの TCP recv で受信データを確認する。
/// 相手が切断したら（recv エラー）ループを抜ける。
fn nc_relay_loop(conn_id: u32) {
    // キーボードフォーカスを取得（GUI 環境で nc がキーを読めるように）
    syscall::console_grab(true);

    let mut key_buf = [0u8; 64];
    let mut recv_buf = [0u8; 1024];
    // 1行分のキーボード入力バッファ（Enter で送信するモード）
    let mut line_buf = [0u8; 256];
    let mut line_len: usize = 0;

    loop {
        // --- キーボード入力をノンブロッキングで読む ---
        let key_n = syscall::key_read(&mut key_buf);
        if key_n > 0 {
            for i in 0..(key_n as usize) {
                let c = key_buf[i];
                match c {
                    // Enter: 行バッファの内容 + 改行を送信
                    b'\n' | b'\r' => {
                        if line_len > 0 {
                            let _ = netd_tcp_send(conn_id, &line_buf[..line_len]);
                            line_len = 0;
                        }
                        let _ = netd_tcp_send(conn_id, b"\r\n");
                        syscall::write_str("\n");
                    }
                    // Backspace
                    0x08 | 0x7f => {
                        if line_len > 0 {
                            line_len -= 1;
                            syscall::write_str("\x08 \x08");
                        }
                    }
                    // 通常の印字可能文字
                    c if c >= 0x20 && c < 0x7f => {
                        if line_len < line_buf.len() {
                            line_buf[line_len] = c;
                            line_len += 1;
                            syscall::write(&[c]);
                        }
                    }
                    _ => {}
                }
            }
        }

        // --- TCP 受信（短タイムアウトでポーリング） ---
        match netd_tcp_recv(conn_id, &mut recv_buf, 50) {
            Ok(n) if n > 0 => {
                let n = n as usize;
                // 受信データをそのまま表示
                if let Ok(text) = core::str::from_utf8(&recv_buf[..n]) {
                    syscall::write_str(text);
                } else {
                    // バイナリデータもベストエフォートで表示
                    syscall::write(&recv_buf[..n]);
                }
            }
            Ok(_) => {
                // 0 バイト = 正常だが何もない、次のポーリングへ
            }
            Err(_) => {
                // エラー = 相手が切断した可能性が高い
                break;
            }
        }
    }

    // キーボードフォーカスを解放
    syscall::console_grab(false);
}

/// gui コマンド: GUI サービスに描画要求を送る
///
/// 例:
///   gui demo
///   gui rect 10 10 80 40 255 0 0
///   gui circle 120 120 40 255 255 0
///   gui fillcircle 160 160 30 0 180 255
///   gui text 20 20 255 255 255 0 0 0 Hello
///   gui meminfo
///   gui hud on|off [interval]
fn cmd_gui(args: &str) {
    let (sub, rest) = split_command(args);
    let mut gui = gui_client::GuiClient::new();
    match sub {
        "demo" => {
            let _ = gui.clear(16, 16, 40);
            let _ = gui.rect(40, 40, 320, 200, 32, 120, 220);
            let _ = gui.circle(120, 120, 50, 255, 220, 64, false);
            let _ = gui.circle(280, 120, 40, 64, 200, 255, true);
            let _ = gui.text(70, 70, (255, 255, 255), (16, 16, 40), "Hello GUI");
            let _ = gui.present();
        }
        "meminfo" => {
            let mut buf = [0u8; FILE_BUFFER_SIZE];
            let result = syscall::get_mem_info(&mut buf);
            if result < 0 {
                syscall::write_str("Error: Failed to get memory info\n");
                return;
            }

            let len = result as usize;
            let Ok(s) = core::str::from_utf8(&buf[..len]) else {
                syscall::write_str("Error: Invalid meminfo\n");
                return;
            };

            let total = json::json_find_u64(s, "total_frames");
            let allocated = json::json_find_u64(s, "allocated_frames");
            let free = json::json_find_u64(s, "free_frames");
            let free_kib = json::json_find_u64(s, "free_kib");
            let heap_start = json::json_find_u64(s, "heap_start");
            let heap_size = json::json_find_u64(s, "heap_size");
            let heap_source = json::json_find_str(s, "heap_source").unwrap_or("-");

            let mut text = String::new();
            text.push_str("Memory Information\n");
            text.push_str("------------------\n");
            if let Some(v) = total {
                text.push_str(&format!("total_frames: {}\n", v));
            }
            if let Some(v) = allocated {
                text.push_str(&format!("allocated_frames: {}\n", v));
            }
            if let Some(v) = free {
                text.push_str(&format!("free_frames: {}\n", v));
            }
            if let Some(v) = free_kib {
                text.push_str(&format!("free_kib: {}\n", v));
            }
            if let Some(v) = heap_start {
                text.push_str(&format!("heap_start: {}\n", v));
            }
            if let Some(v) = heap_size {
                text.push_str(&format!("heap_size: {}\n", v));
            }
            text.push_str(&format!("heap_source: {}\n", heap_source));

            let _ = gui.clear(16, 16, 40);
            if gui.text(16, 16, (255, 255, 255), (16, 16, 40), text.as_str()).is_err() {
                syscall::write_str("Error: gui meminfo failed\n");
                return;
            }
            let _ = gui.present();
        }
        "hud" => {
            let mut parts = rest.split_whitespace();
            match parts.next() {
                Some("on") => {
                    if let Some(interval) = parts.next() {
                        if parts.next().is_some() {
                            return print_gui_usage();
                        }
                        let Some(interval) = parse_u32_arg(Some(interval)) else { return print_gui_usage(); };
                        if gui.hud_with_interval(true, interval).is_err() {
                            syscall::write_str("Error: gui hud on failed\n");
                            return;
                        }
                    } else if gui.hud(true).is_err() {
                        syscall::write_str("Error: gui hud on failed\n");
                        return;
                    }
                }
                Some("off") => {
                    if parts.next().is_some() {
                        return print_gui_usage();
                    }
                    if gui.hud(false).is_err() {
                        syscall::write_str("Error: gui hud off failed\n");
                        return;
                    }
                }
                _ => return print_gui_usage(),
            }
            let _ = gui.present();
        }
        "rect" => {
            let mut parts = rest.split_whitespace();
            let Some(x) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(y) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(w) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(h) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(r) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(g) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(b) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            if r > 255 || g > 255 || b > 255 {
                syscall::write_str("Error: r g b must be 0-255\n");
                return;
            }
            if gui.rect(x, y, w, h, r as u8, g as u8, b as u8).is_err() {
                syscall::write_str("Error: gui rect failed\n");
                return;
            }
            let _ = gui.present();
        }
        "circle" | "fillcircle" => {
            let mut parts = rest.split_whitespace();
            let Some(cx) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(cy) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(rad) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(r) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(g) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(b) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            if r > 255 || g > 255 || b > 255 {
                syscall::write_str("Error: r g b must be 0-255\n");
                return;
            }
            let filled = sub == "fillcircle";
            if gui.circle(cx, cy, rad, r as u8, g as u8, b as u8, filled).is_err() {
                syscall::write_str("Error: gui circle failed\n");
                return;
            }
            let _ = gui.present();
        }
        "text" => {
            let mut parts = rest.split_whitespace();
            let Some(x) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(y) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(fr) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(fg) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(fb) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(br) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(bg) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            let Some(bb) = parse_u32_arg(parts.next()) else { return print_gui_usage(); };
            if fr > 255 || fg > 255 || fb > 255 || br > 255 || bg > 255 || bb > 255 {
                syscall::write_str("Error: color must be 0-255\n");
                return;
            }
            let text = parts.collect::<Vec<&str>>().join(" ");
            if text.is_empty() {
                syscall::write_str("Error: text is required\n");
                return;
            }
            if gui.text(
                x,
                y,
                (fr as u8, fg as u8, fb as u8),
                (br as u8, bg as u8, bb as u8),
                text.as_str(),
            ).is_err() {
                syscall::write_str("Error: gui text failed\n");
                return;
            }
            let _ = gui.present();
        }
        _ => {
            print_gui_usage();
        }
    }
}

fn print_gui_usage() {
    syscall::write_str("Usage:\n");
    syscall::write_str("  gui demo\n");
    syscall::write_str("  gui meminfo\n");
    syscall::write_str("  gui hud on|off [interval]\n");
    syscall::write_str("  gui rect x y w h r g b\n");
    syscall::write_str("  gui circle cx cy r red green blue\n");
    syscall::write_str("  gui fillcircle cx cy r red green blue\n");
    syscall::write_str("  gui text x y fr fg fb br bg bb <text>\n");
}

fn parse_u32_arg(s: Option<&str>) -> Option<u32> {
    let s = s?;
    let v = parse_u64(s)?;
    if v > u32::MAX as u64 {
        return None;
    }
    Some(v as u32)
}

/// rect コマンド: 矩形塗りつぶし描画（GUI デモ）
///
/// 使用例:
///   rect 10 10 80 40 255 0 0
fn cmd_rect(args: &str) {
    let mut parts = args.split_whitespace();

    let parse_u32 = |s: Option<&str>| -> Result<u32, ()> {
        let s = s.ok_or(())?;
        let v = parse_u64(s).ok_or(())?;
        if v > u32::MAX as u64 {
            return Err(());
        }
        Ok(v as u32)
    };

    let x = match parse_u32(parts.next()) {
        Ok(v) => v,
        Err(_) => {
            syscall::write_str("Usage: rect x y w h r g b\n");
            return;
        }
    };
    let y = match parse_u32(parts.next()) {
        Ok(v) => v,
        Err(_) => {
            syscall::write_str("Usage: rect x y w h r g b\n");
            return;
        }
    };
    let w = match parse_u32(parts.next()) {
        Ok(v) => v,
        Err(_) => {
            syscall::write_str("Usage: rect x y w h r g b\n");
            return;
        }
    };
    let h = match parse_u32(parts.next()) {
        Ok(v) => v,
        Err(_) => {
            syscall::write_str("Usage: rect x y w h r g b\n");
            return;
        }
    };
    let r = match parse_u32(parts.next()) {
        Ok(v) => v,
        Err(_) => {
            syscall::write_str("Usage: rect x y w h r g b\n");
            return;
        }
    };
    let g = match parse_u32(parts.next()) {
        Ok(v) => v,
        Err(_) => {
            syscall::write_str("Usage: rect x y w h r g b\n");
            return;
        }
    };
    let b = match parse_u32(parts.next()) {
        Ok(v) => v,
        Err(_) => {
            syscall::write_str("Usage: rect x y w h r g b\n");
            return;
        }
    };

    if r > 255 || g > 255 || b > 255 {
        syscall::write_str("Error: r g b must be 0-255\n");
        return;
    }

    if syscall::draw_rect(x, y, w, h, r as u8, g as u8, b as u8) < 0 {
        syscall::write_str("Error: draw_rect failed\n");
    }
}

// =================================================================
// netd クライアント
// =================================================================

const OPCODE_DNS_LOOKUP: u32 = 1;
const OPCODE_TCP_CONNECT: u32 = 2;
const OPCODE_TCP_SEND: u32 = 3;
const OPCODE_TCP_RECV: u32 = 4;
const OPCODE_TCP_CLOSE: u32 = 5;
const OPCODE_TCP_LISTEN: u32 = 6;
const OPCODE_TCP_ACCEPT: u32 = 7;

const IPC_REQ_HEADER: usize = 8;
const IPC_RESP_HEADER: usize = 12;

fn netd_dns_lookup(domain: &str, ip_out: &mut [u8; 4]) -> Result<(), ()> {
    let payload = domain.as_bytes();
    let mut resp = [0u8; 2048];
    let (status, len) = netd_request(OPCODE_DNS_LOOKUP, payload, &mut resp)?;
    if status < 0 || len != 4 {
        return Err(());
    }
    ip_out.copy_from_slice(&resp[..4]);
    Ok(())
}

fn netd_tcp_connect(ip: &[u8; 4], port: u16) -> Result<u32, ()> {
    let mut payload = [0u8; 6];
    payload[0..4].copy_from_slice(ip);
    payload[4..6].copy_from_slice(&port.to_le_bytes());
    let mut resp = [0u8; 2048];
    let (status, len) = netd_request(OPCODE_TCP_CONNECT, &payload, &mut resp)?;
    if status < 0 || len != 4 {
        Err(())
    } else {
        Ok(u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]))
    }
}

fn netd_tcp_send(conn_id: u32, data: &[u8]) -> Result<(), ()> {
    let mut resp = [0u8; 2048];
    let mut payload = [0u8; 2048];
    if 4 + data.len() > payload.len() {
        return Err(());
    }
    payload[0..4].copy_from_slice(&conn_id.to_le_bytes());
    payload[4..4 + data.len()].copy_from_slice(data);
    let (status, _) = netd_request(OPCODE_TCP_SEND, &payload[..4 + data.len()], &mut resp)?;
    if status < 0 {
        Err(())
    } else {
        Ok(())
    }
}

fn netd_tcp_recv(conn_id: u32, buf: &mut [u8], timeout_ms: u64) -> Result<i64, ()> {
    let mut payload = [0u8; 16];
    let max_len = buf.len() as u32;
    payload[0..4].copy_from_slice(&conn_id.to_le_bytes());
    payload[4..8].copy_from_slice(&max_len.to_le_bytes());
    payload[8..16].copy_from_slice(&timeout_ms.to_le_bytes());

    let mut resp = [0u8; 2048];
    let (status, len) = netd_request(OPCODE_TCP_RECV, &payload, &mut resp)?;
    if status < 0 {
        return Err(());
    }
    let copy_len = core::cmp::min(buf.len(), len);
    buf[..copy_len].copy_from_slice(&resp[..copy_len]);
    Ok(copy_len as i64)
}

fn netd_tcp_close(conn_id: u32) -> Result<(), ()> {
    let mut resp = [0u8; 2048];
    let payload = conn_id.to_le_bytes();
    let (status, _) = netd_request(OPCODE_TCP_CLOSE, &payload, &mut resp)?;
    if status < 0 {
        Err(())
    } else {
        Ok(())
    }
}

/// 指定ポートで TCP リッスンを開始する（netd に listen を依頼）
fn netd_tcp_listen(port: u16) -> Result<(), ()> {
    let payload = port.to_le_bytes();
    let (status, _) = netd_request(OPCODE_TCP_LISTEN, &payload, &mut [0u8; 32])?;
    if status < 0 { Err(()) } else { Ok(()) }
}

/// TCP 接続を受け付ける（netd に accept を依頼）
/// 成功時は接続 ID を返す。timeout_ms=0 でブロッキング待ち。
fn netd_tcp_accept(timeout_ms: u64) -> Result<u32, ()> {
    let payload = timeout_ms.to_le_bytes();
    let mut resp = [0u8; 32];
    let (status, len) = netd_request(OPCODE_TCP_ACCEPT, &payload, &mut resp)?;
    if status < 0 || len != 4 {
        Err(())
    } else {
        Ok(u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]))
    }
}

fn netd_request(opcode: u32, payload: &[u8], resp_buf: &mut [u8]) -> Result<(i32, usize), ()> {
    let mut req = [0u8; 2048];
    if IPC_REQ_HEADER + payload.len() > req.len() {
        return Err(());
    }
    req[0..4].copy_from_slice(&opcode.to_le_bytes());
    req[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    req[8..8 + payload.len()].copy_from_slice(payload);

    let mut netd_id = unsafe { NETD_TASK_ID };
    if netd_id == 0 {
        find_netd();
        netd_id = unsafe { NETD_TASK_ID };
        if netd_id == 0 {
            return Err(());
        }
    }

    if syscall::ipc_send(netd_id, &req[..8 + payload.len()]) < 0 {
        // netd の PID が変わった可能性があるので再解決して1回だけリトライ
        find_netd();
        netd_id = unsafe { NETD_TASK_ID };
        if netd_id == 0 {
            return Err(());
        }
        if syscall::ipc_send(netd_id, &req[..8 + payload.len()]) < 0 {
            return Err(());
        }
    }

    let mut sender = 0u64;
    let n = syscall::ipc_recv(&mut sender, resp_buf, 5000);
    if n < 0 {
        return Err(());
    }
    let n = n as usize;
    if n < IPC_RESP_HEADER {
        return Err(());
    }

    let resp_opcode = u32::from_le_bytes([resp_buf[0], resp_buf[1], resp_buf[2], resp_buf[3]]);
    if resp_opcode != opcode {
        return Err(());
    }
    let status = i32::from_le_bytes([resp_buf[4], resp_buf[5], resp_buf[6], resp_buf[7]]);
    let len = u32::from_le_bytes([resp_buf[8], resp_buf[9], resp_buf[10], resp_buf[11]]) as usize;
    if IPC_RESP_HEADER + len > n {
        return Err(());
    }
    resp_buf.copy_within(IPC_RESP_HEADER..IPC_RESP_HEADER + len, 0);

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

/// IP アドレスを表示
fn write_ip(ip: &[u8; 4]) {
    write_number(ip[0] as u64);
    syscall::write_str(".");
    write_number(ip[1] as u64);
    syscall::write_str(".");
    write_number(ip[2] as u64);
    syscall::write_str(".");
    write_number(ip[3] as u64);
}

/// IP アドレス文字列をパース (例: "192.168.1.1")
/// 文字列を u16 にパースする
fn parse_u16(s: &str) -> Option<u16> {
    let n = parse_u64(s)?;
    if n > u16::MAX as u64 {
        return None;
    }
    Some(n as u16)
}

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut part_index = 0;

    for part in s.split('.') {
        if part_index >= 4 {
            return None;
        }
        let n = parse_u64(part)?;
        if n > 255 {
            return None;
        }
        ip[part_index] = n as u8;
        part_index += 1;
    }

    if part_index != 4 {
        return None;
    }

    Some(ip)
}

// =================================================================
// システム制御コマンド
// =================================================================

/// halt コマンド: システム停止
///
/// システムを停止する。この関数は戻らない。
/// cal コマンド: 指定した月のカレンダーを表示する。
///
/// 使い方: cal <月> <年>
/// 例: cal 2 2026
///
/// ツェラーの公式（Zeller's congruence）で曜日を計算する。
/// ツェラーの公式は「年月日から曜日を求める」数学的な公式で、
/// グレゴリオ暦に対応している。
fn cmd_cal(args: &str) {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.len() != 2 {
        println!("Usage: cal <month> <year>");
        println!("  Example: cal 2 2026");
        return;
    }

    let month: u32 = match parse_u64(parts[0]) {
        Some(m) if (1..=12).contains(&m) => m as u32,
        _ => {
            println!("Invalid month (1-12)");
            return;
        }
    };

    let year: u32 = match parse_u64(parts[1]) {
        Some(y) if y >= 1 && y <= u32::MAX as u64 => y as u32,
        _ => {
            println!("Invalid year");
            return;
        }
    };

    // 月の名前
    let month_name = match month {
        1 => "January", 2 => "February", 3 => "March",
        4 => "April", 5 => "May", 6 => "June",
        7 => "July", 8 => "August", 9 => "September",
        10 => "October", 11 => "November", 12 => "December",
        _ => unreachable!(),
    };

    // ヘッダー（月名と年をセンタリング）
    // "Su Mo Tu We Th Fr Sa" は 20 文字
    let header = alloc::format!("{} {}", month_name, year);
    let pad = if header.len() < 20 {
        (20 - header.len()) / 2
    } else {
        0
    };
    for _ in 0..pad {
        print!(" ");
    }
    println!("{}", header);
    println!("Su Mo Tu We Th Fr Sa");

    // その月の日数を計算
    let days_in_month = cal_days_in_month(year, month);

    // 月初日の曜日を計算（0=日曜, 1=月曜, ..., 6=土曜）
    let first_dow = cal_day_of_week(year, month, 1);

    // 月初日までの空白を出力
    for _ in 0..first_dow {
        print!("   ");
    }

    // 各日を出力
    let mut dow = first_dow;
    for day in 1..=days_in_month {
        if day < 10 {
            print!(" {}", day);
        } else {
            print!("{}", day);
        }

        dow += 1;
        if dow == 7 {
            // 土曜日の後は改行
            println!();
            dow = 0;
        } else {
            print!(" ");
        }
    }

    // 最終行が改行で終わっていなければ改行
    if dow != 0 {
        println!();
    }
}

/// ツェラーの公式で曜日を計算する。
/// 戻り値: 0=日曜, 1=月曜, ..., 6=土曜
fn cal_day_of_week(year: u32, month: u32, day: u32) -> u32 {
    // ツェラーの公式では 1月・2月を前年の 13月・14月として扱う
    let (y, m) = if month <= 2 {
        (year as i32 - 1, month as i32 + 12)
    } else {
        (year as i32, month as i32)
    };

    let q = day as i32;
    let k = y % 100;
    let j = y / 100;

    // ツェラーの公式（グレゴリオ暦）:
    // h = (q + floor(13*(m+1)/5) + K + floor(K/4) + floor(J/4) - 2*J) mod 7
    // h: 0=土曜, 1=日曜, 2=月曜, ..., 6=金曜
    let h = (q + (13 * (m + 1)) / 5 + k + k / 4 + j / 4 - 2 * j) % 7;

    // h を調整して 0=日曜, 1=月曜, ..., 6=土曜 にする
    // h が負になることがあるので +7 してから mod 7
    let h = ((h + 7) % 7) as u32;
    // h: 0=土曜, 1=日曜, ..., 6=金曜 → 0=日曜 に変換
    // 日曜=1→0, 月曜=2→1, ..., 土曜=0→6
    match h {
        0 => 6, // 土曜
        1 => 0, // 日曜
        2 => 1, // 月曜
        3 => 2, // 火曜
        4 => 3, // 水曜
        5 => 4, // 木曜
        6 => 5, // 金曜
        _ => unreachable!(),
    }
}

/// 指定した年月の日数を返す。うるう年も考慮する。
fn cal_days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            // うるう年判定:
            // 4 で割り切れる年はうるう年。
            // ただし 100 で割り切れる年は平年。
            // ただし 400 で割り切れる年はうるう年。
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn cmd_halt() {
    syscall::write_str("System halted.\n");
    syscall::halt();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::write_str("Shell panic!\n");
    syscall::exit();
}
