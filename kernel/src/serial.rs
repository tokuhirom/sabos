// serial.rs — シリアルポート (UART 16550) ドライバ
//
// UART (Universal Asynchronous Receiver/Transmitter) は
// PC の最も基本的なシリアル通信インターフェース。
// COM1 は I/O ポート 0x3F8 に配置されている。
//
// QEMU は `-serial stdio` オプションで仮想シリアルポートを
// ホストの標準出力に接続してくれるので、ここに書き込んだ内容は
// `make run` を実行したターミナルに表示される。
// Exit Boot Services 後のデバッグに非常に便利。
//
// UART 16550 のレジスタ構成（ベースアドレス 0x3F8 の場合）:
//   0x3F8: データ送受信レジスタ（DLAB=0 時）/ ボーレート除数 下位（DLAB=1 時）
//   0x3F9: 割り込み有効レジスタ（DLAB=0 時）/ ボーレート除数 上位（DLAB=1 時）
//   0x3FA: FIFO 制御レジスタ
//   0x3FB: ライン制御レジスタ（データビット数、パリティ等）
//   0x3FC: モデム制御レジスタ
//   0x3FD: ライン状態レジスタ（送信バッファが空かどうか等）

use core::fmt;
use lazy_static::lazy_static;
use spin::Mutex;
use x86_64::instructions::port::Port;

/// COM1 のベースアドレス。PC の標準的な設定。
const COM1_BASE: u16 = 0x3F8;

/// シリアルポートを表す構造体。
/// I/O ポートのベースアドレスを保持する。
pub struct SerialPort {
    data: Port<u8>,          // ベース+0: データ送受信
    int_enable: Port<u8>,    // ベース+1: 割り込み有効
    fifo_ctrl: Port<u8>,     // ベース+2: FIFO 制御
    line_ctrl: Port<u8>,     // ベース+3: ライン制御
    modem_ctrl: Port<u8>,    // ベース+4: モデム制御
    line_status: Port<u8>,   // ベース+5: ライン状態
}

impl SerialPort {
    /// 指定されたベースアドレスで SerialPort を作成する。
    pub const fn new(base: u16) -> Self {
        Self {
            data: Port::new(base),
            int_enable: Port::new(base + 1),
            fifo_ctrl: Port::new(base + 2),
            line_ctrl: Port::new(base + 3),
            modem_ctrl: Port::new(base + 4),
            line_status: Port::new(base + 5),
        }
    }

    /// UART を初期化する。115200 baud, 8N1 (8データビット, パリティなし, 1ストップビット)。
    pub fn init(&mut self) {
        unsafe {
            // 割り込みを無効化
            self.int_enable.write(0x00);

            // DLAB (Divisor Latch Access Bit) を有効にして
            // ボーレート除数を設定できるようにする
            self.line_ctrl.write(0x80);

            // ボーレート除数を設定: 115200 baud の場合、除数 = 1
            // （基準クロック 1.8432 MHz / (16 * 115200) = 1）
            self.data.write(0x01);       // 除数の下位バイト
            self.int_enable.write(0x00); // 除数の上位バイト

            // 8N1 (8データビット, パリティなし, 1ストップビット) に設定
            // DLAB も同時にクリアされる
            self.line_ctrl.write(0x03);

            // FIFO を有効化、14バイト閾値でクリア
            self.fifo_ctrl.write(0xC7);

            // RTS と DTR をアサート、OUT2 を有効化
            // OUT2 は割り込み有効化に必要（使わないけど念のため）
            self.modem_ctrl.write(0x0B);
        }
    }

    /// 送信バッファが空になるまで待つ。
    /// ライン状態レジスタのビット5が「送信保持レジスタが空」を示す。
    fn wait_for_transmit_empty(&mut self) {
        unsafe {
            while self.line_status.read() & 0x20 == 0 {
                // ビジーウェイト。UART はすぐ空くので問題ない。
                core::hint::spin_loop();
            }
        }
    }

    /// 1バイトを送信する。
    pub fn write_byte(&mut self, byte: u8) {
        self.wait_for_transmit_empty();
        unsafe {
            self.data.write(byte);
        }
    }

    /// 文字列を送信する。'\n' を '\r\n' に変換する（シリアルの慣例）。
    pub fn write_str(&mut self, s: &str) {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
    }
}

/// core::fmt::Write を実装して write!() マクロが使えるようにする。
impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_str(s);
        Ok(())
    }
}

lazy_static! {
    /// COM1 シリアルポートのグローバルインスタンス。
    /// spin::Mutex で排他制御する。
    pub static ref SERIAL1: Mutex<SerialPort> = {
        let mut serial_port = SerialPort::new(COM1_BASE);
        serial_port.init();
        Mutex::new(serial_port)
    };
}

/// シリアルポートに出力する内部関数。
/// spin::Mutex で排他制御する。
/// 割り込みハンドラは SERIAL1 のロックを取得しないため without_interrupts は不要。
#[doc(hidden)]
pub fn _serial_print(args: fmt::Arguments) {
    use core::fmt::Write;
    SERIAL1
        .lock()
        .write_fmt(args)
        .expect("Printing to serial failed");
}

/// シリアル用 print! マクロ。
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => ({
        $crate::serial::_serial_print(format_args!($($arg)*));
    });
}

/// シリアル用 println! マクロ。
#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}
