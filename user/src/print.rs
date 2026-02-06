// print.rs — println! / print! マクロ
//
// no_std 環境でも `println!("Hello, {}!", name)` のように
// フォーマット付き出力ができるようにするモジュール。
// バックエンドは SYS_WRITE syscall。
//
// 使い方:
//   print!("Hello");
//   println!("count = {}", 42);
//
// 各バイナリで以下のように取り込む:
//   #[path = "../print.rs"]
//   mod print;

// println! マクロから参照されるが、未使用のバイナリでも警告を出さない
#![allow(dead_code)]

use core::fmt;

/// コンソール出力用の Write 実装
///
/// `core::fmt::Write` トレイトを実装することで、
/// `write!` / `writeln!` マクロが使えるようになる。
/// 内部で `syscall::write()` を呼んでシリアルコンソールに出力する。
pub struct ConsoleWriter;

impl fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // syscall モジュールは親モジュール（各バイナリ）で定義されている
        super::syscall::write(s.as_bytes());
        Ok(())
    }
}

/// フォーマット付きコンソール出力（改行なし）
///
/// std の `print!` マクロと同じ使い勝手。
/// 内部で `core::fmt::Write` を使ってフォーマットし、
/// `SYS_WRITE` syscall でコンソールに出力する。
///
/// # 例
/// ```
/// print!("Hello, {}!", "SABOS");
/// ```
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut writer = $crate::print::ConsoleWriter;
        let _ = write!(writer, $($arg)*);
    }};
}

/// フォーマット付きコンソール出力（改行あり）
///
/// std の `println!` マクロと同じ使い勝手。
/// 最後に自動で改行 (`\n`) を追加する。
///
/// # 例
/// ```
/// println!("count = {}", 42);
/// println!();  // 空行
/// ```
#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut writer = $crate::print::ConsoleWriter;
        let _ = writeln!(writer, $($arg)*);
    }};
}
