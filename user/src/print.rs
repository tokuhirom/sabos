// print.rs — println! / print! マクロ + log クレート統合
//
// no_std 環境でも `println!("Hello, {}!", name)` のように
// フォーマット付き出力ができるようにするモジュール。
// バックエンドは SYS_WRITE syscall。
//
// さらに `log` クレートのロガーを統合しており、
// `log::info!("message")` 等でレベル付きログ出力ができる。
//
// 使い方:
//   print!("Hello");
//   println!("count = {}", 42);
//   log::info!("starting up");
//   log::warn!("something unexpected");
//
// 各バイナリで以下のように取り込む:
//   #[path = "../print.rs"]
//   mod print;
//
// ロガーの初期化は `print::init_logger()` を呼ぶ。
// 初期化しなくても println! は使える（log マクロが無視されるだけ）。

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

// =================================================================
// log クレート統合
// =================================================================
//
// `log` クレートは Rust のデファクトスタンダードなロギングファサード。
// ライブラリ側は `log::info!()` 等を使い、アプリケーション側で
// 具体的なロガー実装を選ぶ設計になっている。
//
// ここでは ConsoleWriter を使ってシリアルコンソールに出力するロガーを実装する。
// 出力形式: "[LEVEL] メッセージ\n"

/// コンソール出力ロガー
///
/// `log::Log` トレイトを実装し、シリアルコンソールに
/// "[LEVEL] message" 形式でログを出力する。
struct ConsoleLogger;

impl log::Log for ConsoleLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        // すべてのレベルを有効にする（フィルタリングは max_level で制御）
        true
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            use core::fmt::Write;
            let mut writer = ConsoleWriter;
            let _ = write!(writer, "[{}] {}\n", record.level(), record.args());
        }
    }

    fn flush(&self) {
        // シリアル出力はバッファリングしないので何もしない
    }
}

/// グローバルロガーインスタンス
static LOGGER: ConsoleLogger = ConsoleLogger;

/// ロガーを初期化する
///
/// この関数を呼ぶと `log::info!()` 等のマクロが使えるようになる。
/// 呼ばなくても `println!` は使える（log マクロが無視されるだけ）。
///
/// # 例
/// ```
/// print::init_logger();
/// log::info!("Hello from SABOS!");
/// ```
pub fn init_logger() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}
