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

use crate::syscall;

/// 行バッファの最大サイズ
const LINE_BUFFER_SIZE: usize = 256;

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
    syscall::write_str("  echo <text>  - Print text to console\n");
    syscall::write_str("  help         - Show this help message\n");
    syscall::write_str("  clear        - Clear the screen\n");
    syscall::write_str("  exit         - Exit the shell\n");
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
