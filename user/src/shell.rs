// shell.rs — ユーザー空間シェル
//
// SABOS のユーザー空間で動作するシェル。
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
// - mem: メモリ情報を表示
// - ps: タスク一覧を表示
// - ip: ネットワーク情報を表示
// - run <file>: ELF プログラムをフォアグラウンドで実行
// - spawn <file>: ELF プログラムをバックグラウンドで実行
// - sleep <ms>: 指定ミリ秒スリープ

use crate::syscall;

/// 行バッファの最大サイズ
const LINE_BUFFER_SIZE: usize = 256;

/// ファイル読み取り/ディレクトリ一覧用のバッファサイズ
const FILE_BUFFER_SIZE: usize = 4096;

/// シェルのメインループを実行
pub fn run() -> ! {
    print_welcome();

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
        execute_command(line);
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
fn execute_command(line: &[u8]) {
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

    match cmd {
        "echo" => cmd_echo(args),
        "help" => cmd_help(),
        "clear" => cmd_clear(),
        "exit" => cmd_exit(),
        "ls" => cmd_ls(args),
        "cat" => cmd_cat(args),
        "write" => cmd_write(args),
        "rm" => cmd_rm(args),
        "mem" => cmd_mem(),
        "ps" => cmd_ps(),
        "ip" => cmd_ip(),
        "run" => cmd_run(args),
        "spawn" => cmd_spawn(args),
        "sleep" => cmd_sleep(args),
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
    syscall::write_str("  mem               - Show memory information\n");
    syscall::write_str("  ps                - Show task list\n");
    syscall::write_str("  ip                - Show network information\n");
    syscall::write_str("  run <file>        - Run ELF program (foreground)\n");
    syscall::write_str("  spawn <file>      - Run ELF program (background)\n");
    syscall::write_str("  sleep <ms>        - Sleep for milliseconds\n");
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

// =================================================================
// ファイルシステムコマンド
// =================================================================

/// ls コマンド: ディレクトリ一覧を表示
fn cmd_ls(args: &str) {
    // パスが指定されなければルートディレクトリ
    let path = if args.is_empty() { "/" } else { args };

    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::dir_list(path, &mut buf);

    if result < 0 {
        syscall::write_str("Error: Failed to list directory\n");
        return;
    }

    // 結果を表示
    let len = result as usize;
    if len > 0 {
        syscall::write(&buf[..len]);
    }
}

/// cat コマンド: ファイル内容を表示
fn cmd_cat(args: &str) {
    if args.is_empty() {
        syscall::write_str("Usage: cat <filename>\n");
        return;
    }

    // パスの正規化: "/" で始まらなければ "/" を付ける
    let path = if args.starts_with('/') {
        args
    } else {
        // 簡易実装: 先頭に "/" がない場合は一時バッファに結合
        // 注意: この実装ではスタック上のバッファを使うので長いパスは非対応
        &args  // とりあえずそのまま渡す（FAT16側で対応）
    };

    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let result = syscall::file_read(path, &mut buf);

    if result < 0 {
        syscall::write_str("Error: File not found or cannot be read\n");
        return;
    }

    // ファイル内容を表示
    let len = result as usize;
    if len > 0 {
        syscall::write(&buf[..len]);
        // 最後が改行でなければ改行を追加
        if buf[len - 1] != b'\n' {
            syscall::write_str("\n");
        }
    }
}

/// write コマンド: ファイルを作成/上書き
fn cmd_write(args: &str) {
    // ファイル名とデータを分割
    let (filename, data) = split_command(args);

    if filename.is_empty() {
        syscall::write_str("Usage: write <filename> <text>\n");
        return;
    }

    let result = syscall::file_write(filename, data.as_bytes());

    if result < 0 {
        syscall::write_str("Error: Failed to write file\n");
        return;
    }

    syscall::write_str("File written successfully\n");
}

/// rm コマンド: ファイルを削除
fn cmd_rm(args: &str) {
    if args.is_empty() {
        syscall::write_str("Usage: rm <filename>\n");
        return;
    }

    let result = syscall::file_delete(args);

    if result < 0 {
        syscall::write_str("Error: Failed to delete file\n");
        return;
    }

    syscall::write_str("File deleted successfully\n");
}

// =================================================================
// システム情報コマンド
// =================================================================

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

    // 結果をパースして表示
    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        // "key=value" 形式の行を整形して表示
        for line in s.lines() {
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "total_frames" => {
                        syscall::write_str("  Total frames:     ");
                        syscall::write_str(value);
                        syscall::write_str("\n");
                    }
                    "allocated_frames" => {
                        syscall::write_str("  Allocated frames: ");
                        syscall::write_str(value);
                        syscall::write_str("\n");
                    }
                    "free_frames" => {
                        syscall::write_str("  Free frames:      ");
                        syscall::write_str(value);
                        syscall::write_str("\n");
                    }
                    "free_kib" => {
                        syscall::write_str("  Free memory:      ");
                        syscall::write_str(value);
                        syscall::write_str(" KiB\n");
                    }
                    _ => {}
                }
            }
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

    // 結果を表示（CSV 形式をテーブル形式に変換）
    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        // ヘッダを表示
        syscall::write_str("  ID  STATE       TYPE    NAME\n");
        syscall::write_str("  --  ----------  ------  ----------\n");

        // 各行をパース（最初の行はヘッダなのでスキップ）
        for (i, line) in s.lines().enumerate() {
            if i == 0 {
                continue;  // ヘッダ行をスキップ
            }

            // CSV 形式: id,state,type,name
            let parts: [&str; 4] = parse_csv_line(line);
            if parts[0].is_empty() {
                continue;
            }

            // 整形して表示
            syscall::write_str("  ");
            write_padded(parts[0], 2);   // ID
            syscall::write_str("  ");
            write_padded(parts[1], 10);  // STATE
            syscall::write_str("  ");
            write_padded(parts[2], 6);   // TYPE
            syscall::write_str("  ");
            syscall::write_str(parts[3]); // NAME
            syscall::write_str("\n");
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

// =================================================================
// プロセス実行コマンド
// =================================================================

/// run コマンド: ELF プログラムをフォアグラウンドで実行
///
/// 指定した ELF ファイルを読み込んで同期実行する。
/// プログラムが終了するまでシェルはブロックする。
fn cmd_run(args: &str) {
    let filename = args.trim();
    if filename.is_empty() {
        syscall::write_str("Usage: run <FILENAME>\n");
        syscall::write_str("  Example: run HELLO.ELF\n");
        return;
    }

    syscall::write_str("Running ");
    syscall::write_str(filename);
    syscall::write_str("...\n");

    let result = syscall::exec(filename);

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
fn cmd_spawn(args: &str) {
    let filename = args.trim();
    if filename.is_empty() {
        syscall::write_str("Usage: spawn <FILENAME>\n");
        syscall::write_str("  Example: spawn HELLO.ELF\n");
        syscall::write_str("  The process runs in the background. Use 'ps' to see tasks.\n");
        return;
    }

    syscall::write_str("Spawning ");
    syscall::write_str(filename);
    syscall::write_str("...\n");

    let result = syscall::spawn(filename);

    if result < 0 {
        syscall::write_str("Error: Failed to spawn process\n");
        return;
    }

    syscall::write_str("Process spawned as task ");
    write_number(result as u64);
    syscall::write_str(" (background)\n");
    syscall::write_str("Use 'ps' to see running tasks.\n");
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

/// CSV 行を4つのフィールドにパース
///
/// カンマで区切って最大4つのフィールドを返す。
/// フィールドが足りない場合は空文字列になる。
fn parse_csv_line(line: &str) -> [&str; 4] {
    let mut parts = [""; 4];
    for (i, part) in line.splitn(4, ',').enumerate() {
        parts[i] = part;
    }
    parts
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
