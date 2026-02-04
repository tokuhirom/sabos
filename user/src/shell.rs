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
// - lspci: PCI デバイス一覧を表示
// - run <file>: ELF プログラムをフォアグラウンドで実行
// - spawn <file>: ELF プログラムをバックグラウンドで実行
// - sleep <ms>: 指定ミリ秒スリープ
// - dns <domain>: DNS 解決
// - http <host> [path]: HTTP GET リクエスト
// - halt: システム停止

use crate::syscall;
use crate::fat16::Fat16;

/// netd のタスクID（起動できた場合のみ設定）
static mut NETD_TASK_ID: u64 = 0;

/// 行バッファの最大サイズ
const LINE_BUFFER_SIZE: usize = 256;

/// ファイル読み取り/ディレクトリ一覧用のバッファサイズ
const FILE_BUFFER_SIZE: usize = 4096;

/// シェルのメインループを実行
pub fn run() -> ! {
    print_welcome();
    init_net_service();

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
        "mkdir" => cmd_mkdir(args),
        "rmdir" => cmd_rmdir(args),
        "mem" => cmd_mem(),
        "ps" => cmd_ps(),
        "ip" => cmd_ip(),
        "lspci" => cmd_lspci(),
        "run" => cmd_run(args),
        "spawn" => cmd_spawn(args),
        "sleep" => cmd_sleep(args),
        "dns" => cmd_dns(args),
        "http" => cmd_http(args),
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
    syscall::write_str("  mkdir <dir>       - Create directory (root only)\n");
    syscall::write_str("  rmdir <dir>       - Remove empty directory (root only)\n");
    syscall::write_str("  mem               - Show memory information\n");
    syscall::write_str("  ps                - Show task list\n");
    syscall::write_str("  ip                - Show network information\n");
    syscall::write_str("  lspci             - List PCI devices\n");
    syscall::write_str("  run <file>        - Run ELF program (foreground)\n");
    syscall::write_str("  spawn <file>      - Run ELF program (background)\n");
    syscall::write_str("  sleep <ms>        - Sleep for milliseconds\n");
    syscall::write_str("  dns <domain>      - DNS lookup\n");
    syscall::write_str("  http <host> [path] - HTTP GET request\n");
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

// =================================================================
// ファイルシステムコマンド
// =================================================================

/// ls コマンド: ディレクトリ一覧を表示
fn cmd_ls(args: &str) {
    // パスが指定されなければルートディレクトリ
    let path = if args.is_empty() { "/" } else { args };

    let fs = match Fat16::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT16 not available\n");
            return;
        }
    };

    let entries = match fs.list_dir(path) {
        Ok(v) => v,
        Err(_) => {
            syscall::write_str("Error: Failed to list directory\n");
            return;
        }
    };

    for entry in entries {
        syscall::write_str(&entry.name);
        if (entry.attr & 0x10) != 0 {
            syscall::write_str("/");
        }
        syscall::write_str("\n");
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

    let fs = match Fat16::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT16 not available\n");
            return;
        }
    };

    let data = match fs.read_file(path.trim_start_matches('/')) {
        Ok(d) => d,
        Err(_) => {
            syscall::write_str("Error: File not found or cannot be read\n");
            return;
        }
    };

    if !data.is_empty() {
        syscall::write(&data);
        if *data.last().unwrap() != b'\n' {
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

    let fs = match Fat16::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT16 not available\n");
            return;
        }
    };

    let name = filename.trim_start_matches('/');
    if fs.create_file(name, data.as_bytes()).is_err() {
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

    let fs = match Fat16::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT16 not available\n");
            return;
        }
    };

    let name = args.trim_start_matches('/');
    if fs.delete_file(name).is_err() {
        syscall::write_str("Error: Failed to delete file\n");
        return;
    }

    syscall::write_str("File deleted successfully\n");
}

/// mkdir コマンド: ディレクトリを作成
fn cmd_mkdir(args: &str) {
    let name = args.trim();
    if name.is_empty() {
        syscall::write_str("Usage: mkdir <dirname>\n");
        return;
    }

    let fs = match Fat16::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT16 not available\n");
            return;
        }
    };

    if fs.create_dir(name).is_err() {
        syscall::write_str("Error: Failed to create directory\n");
        return;
    }

    syscall::write_str("Directory created successfully\n");
}

/// rmdir コマンド: 空のディレクトリを削除
fn cmd_rmdir(args: &str) {
    let name = args.trim();
    if name.is_empty() {
        syscall::write_str("Usage: rmdir <dirname>\n");
        return;
    }

    let fs = match Fat16::new() {
        Ok(f) => f,
        Err(_) => {
            syscall::write_str("Error: FAT16 not available\n");
            return;
        }
    };

    if fs.remove_dir(name).is_err() {
        syscall::write_str("Error: Failed to remove directory\n");
        return;
    }

    syscall::write_str("Directory removed successfully\n");
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

    // 結果をパースして表示（JSON）
    let len = result as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..len]) {
        let total = json_find_u64(s, "total_frames");
        let allocated = json_find_u64(s, "allocated_frames");
        let free = json_find_u64(s, "free_frames");
        let free_kib = json_find_u64(s, "free_kib");

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

        let Some((tasks_start, tasks_end)) = json_find_array_bounds(s, "tasks") else {
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

            let Some(obj_end) = find_matching_brace(s, i) else {
                break;
            };
            if obj_end > tasks_end {
                break;
            }

            let obj = &s[i + 1..obj_end];
            let id = json_find_u64(obj, "id");
            let state = json_find_str(obj, "state");
            let ty = json_find_str(obj, "type");
            let name = json_find_str(obj, "name");

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

// =================================================================
// JSON パーサ（最小実装）
// =================================================================

/// JSON のキーに対応する値の開始位置を返す
fn json_find_key_value_start(s: &str, key: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let key_bytes = key.as_bytes();
    let mut i = 0;
    while i + key_bytes.len() + 2 <= bytes.len() {
        if bytes[i] == b'"'
            && bytes[i + 1..i + 1 + key_bytes.len()] == *key_bytes
            && bytes[i + 1 + key_bytes.len()] == b'"'
        {
            let mut j = i + 1 + key_bytes.len() + 1;
            // 空白をスキップ
            while j < bytes.len() && is_json_space(bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b':' {
                j += 1;
                while j < bytes.len() && is_json_space(bytes[j]) {
                    j += 1;
                }
                return Some(j);
            }
        }
        i += 1;
    }
    None
}

/// JSON から数値を取り出す
fn json_find_u64(s: &str, key: &str) -> Option<u64> {
    let start = json_find_key_value_start(s, key)?;
    let tail = &s[start..];
    parse_u64_prefix(tail)
}

/// JSON から文字列を取り出す（エスケープは展開しない）
fn json_find_str<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let start = json_find_key_value_start(s, key)?;
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut i = start + 1;
    let mut escape = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if b == b'\\' {
            escape = true;
            i += 1;
            continue;
        }
        if b == b'"' {
            return Some(&s[start + 1..i]);
        }
        i += 1;
    }
    None
}

/// JSON 配列の範囲を取得する
fn json_find_array_bounds(s: &str, key: &str) -> Option<(usize, usize)> {
    let start = json_find_key_value_start(s, key)?;
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let end = find_matching_delim(s, start, b'[', b']')?;
    Some((start + 1, end))
}

/// { ... } の対応する } を探す
fn find_matching_brace(s: &str, start: usize) -> Option<usize> {
    find_matching_delim(s, start, b'{', b'}')
}

/// 対応する閉じ括弧を探す（最小実装）
fn find_matching_delim(s: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&open) {
        return None;
    }
    let mut depth = 1usize;
    let mut i = start + 1;
    let mut in_string = false;
    let mut escape = false;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if b == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// JSON の空白判定
fn is_json_space(b: u8) -> bool {
    b == b' ' || b == b'\n' || b == b'\r' || b == b'\t'
}

/// 文字列先頭の数値を u64 にパース
fn parse_u64_prefix(s: &str) -> Option<u64> {
    let mut result: u64 = 0;
    let mut found = false;
    for b in s.bytes() {
        if b < b'0' || b > b'9' {
            break;
        }
        found = true;
        result = result.checked_mul(10)?;
        result = result.checked_add((b - b'0') as u64)?;
    }
    if found { Some(result) } else { None }
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
    // 引数をパース: host [path]
    let (host, path) = split_command(args);

    if host.is_empty() {
        syscall::write_str("Usage: http <host> [path]\n");
        syscall::write_str("  Example: http example.com /\n");
        return;
    }

    let path = if path.is_empty() { "/" } else { path };

    // IP アドレスを解決または直接パース
    let ip = match parse_ip(host) {
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
    };

    // TCP 接続
    syscall::write_str("Connecting to ");
    write_ip(&ip);
    syscall::write_str(":80...\n");

    if netd_tcp_connect(&ip, 80).is_err() {
        syscall::write_str("Error: TCP connect failed\n");
        return;
    }
    syscall::write_str("Connected!\n");

    // HTTP リクエストを構築
    // 簡易的に固定フォーマットで送信
    syscall::write_str("Sending HTTP request...\n");

    // GET line
    let _ = netd_tcp_send(b"GET ");
    let _ = netd_tcp_send(path.as_bytes());
    let _ = netd_tcp_send(b" HTTP/1.0\r\n");

    // Host header
    let _ = netd_tcp_send(b"Host: ");
    let _ = netd_tcp_send(host.as_bytes());
    let _ = netd_tcp_send(b"\r\n");

    // Connection header and end of headers
    let _ = netd_tcp_send(b"Connection: close\r\n\r\n");

    // レスポンスを受信
    syscall::write_str("Receiving response...\n");
    syscall::write_str("--- Response ---\n");

    let mut buf = [0u8; 1024];
    loop {
        let n = match netd_tcp_recv(&mut buf, 5000) {
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
    let _ = netd_tcp_close();
}

// =================================================================
// netd クライアント
// =================================================================

const OPCODE_DNS_LOOKUP: u32 = 1;
const OPCODE_TCP_CONNECT: u32 = 2;
const OPCODE_TCP_SEND: u32 = 3;
const OPCODE_TCP_RECV: u32 = 4;
const OPCODE_TCP_CLOSE: u32 = 5;

const IPC_REQ_HEADER: usize = 8;
const IPC_RESP_HEADER: usize = 12;

fn init_net_service() {
    // 既に起動済みなら何もしない
    unsafe {
        if NETD_TASK_ID != 0 {
            return;
        }
    }

    // disk.img から NETD.ELF を起動する
    let id = syscall::spawn("/NETD.ELF");
    if id < 0 {
        syscall::write_str("Warning: netd not available\n");
        return;
    }
    unsafe {
        NETD_TASK_ID = id as u64;
    }
}

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

fn netd_tcp_connect(ip: &[u8; 4], port: u16) -> Result<(), ()> {
    let mut payload = [0u8; 6];
    payload[0..4].copy_from_slice(ip);
    payload[4..6].copy_from_slice(&port.to_le_bytes());
    let mut resp = [0u8; 2048];
    let (status, _) = netd_request(OPCODE_TCP_CONNECT, &payload, &mut resp)?;
    if status < 0 {
        Err(())
    } else {
        Ok(())
    }
}

fn netd_tcp_send(data: &[u8]) -> Result<(), ()> {
    let mut resp = [0u8; 2048];
    let (status, _) = netd_request(OPCODE_TCP_SEND, data, &mut resp)?;
    if status < 0 {
        Err(())
    } else {
        Ok(())
    }
}

fn netd_tcp_recv(buf: &mut [u8], timeout_ms: u64) -> Result<i64, ()> {
    let mut payload = [0u8; 12];
    let max_len = buf.len() as u32;
    payload[0..4].copy_from_slice(&max_len.to_le_bytes());
    payload[4..12].copy_from_slice(&timeout_ms.to_le_bytes());

    let mut resp = [0u8; 2048];
    let (status, len) = netd_request(OPCODE_TCP_RECV, &payload, &mut resp)?;
    if status < 0 {
        return Err(());
    }
    let copy_len = core::cmp::min(buf.len(), len);
    buf[..copy_len].copy_from_slice(&resp[..copy_len]);
    Ok(copy_len as i64)
}

fn netd_tcp_close() -> Result<(), ()> {
    let mut resp = [0u8; 2048];
    let (status, _) = netd_request(OPCODE_TCP_CLOSE, &[], &mut resp)?;
    if status < 0 {
        Err(())
    } else {
        Ok(())
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

    let netd_id = unsafe { NETD_TASK_ID };
    if netd_id == 0 {
        return Err(());
    }

    if syscall::ipc_send(netd_id, &req[..8 + payload.len()]) < 0 {
        return Err(());
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

    Ok((status, len))
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
fn cmd_halt() {
    syscall::write_str("System halted.\n");
    syscall::halt();
}
