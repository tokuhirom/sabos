// panic.rs — 自前のパニックハンドラ
//
// uefi crate のデフォルト panic_handler は UEFI の stdout に出力するため、
// ExitBootServices 後は何も表示されず、原因不明のフリーズになってしまう。
//
// この自前ハンドラは以下の2つの出力先に panic 情報を表示する:
//   1. シリアルポート (COM1) — `make run` のターミナルに表示される
//   2. フレームバッファ — 画面に赤字で表示される
//
// デッドロック対策:
//   panic は WRITER や SERIAL1 のロック保持中に起きる可能性がある。
//   lock() ではなく try_lock() を使い、ロック取得できない場合は:
//     - シリアル: I/O ポートに直接 1 バイトずつ書くフォールバックを使う
//     - フレームバッファ: スキップする（シリアルがあればデバッグには十分）

use core::fmt;
use core::fmt::Write;
use core::panic::PanicInfo;

/// COM1 データレジスタのアドレス（I/O ポート直接書き込み用）
const COM1_DATA: u16 = 0x3F8;
/// COM1 ライン状態レジスタのアドレス
const COM1_LINE_STATUS: u16 = 0x3FD;

/// I/O ポートに直接 1 バイトを書き込む。
/// Mutex を一切使わないので、デッドロックの心配がない。
/// UART の送信バッファが空くまで待ってから書き込む。
unsafe fn serial_write_byte_raw(byte: u8) {
    use x86_64::instructions::port::Port;

    // ライン状態レジスタのビット5 (送信保持レジスタが空) を待つ
    let mut status_port: Port<u8> = Port::new(COM1_LINE_STATUS);
    while unsafe { status_port.read() } & 0x20 == 0 {
        core::hint::spin_loop();
    }

    // データレジスタに書き込む
    let mut data_port: Port<u8> = Port::new(COM1_DATA);
    unsafe { data_port.write(byte) };
}

/// バイト列を I/O ポートに直接書き込む。
/// '\n' を '\r\n' に変換する（シリアルの慣例）。
unsafe fn serial_write_raw(s: &[u8]) {
    for &byte in s {
        if byte == b'\n' {
            unsafe { serial_write_byte_raw(b'\r') };
        }
        unsafe { serial_write_byte_raw(byte) };
    }
}

/// I/O ポート直接書き込み用の fmt::Write アダプタ。
/// try_lock() が失敗した場合のフォールバックとして使う。
/// PanicInfo を format_args! でフォーマットしてシリアルに書き出すために必要。
struct RawSerialWriter;

impl fmt::Write for RawSerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        unsafe { serial_write_raw(s.as_bytes()) };
        Ok(())
    }
}

/// カーネルパニックハンドラ。
///
/// panic!() が呼ばれたときに自動的に呼び出される。
/// シリアルとフレームバッファの両方に panic 情報を出力してから、
/// hlt ループで CPU を停止する。
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // 1. 割り込みを即座に無効化する。
    //    panic 中に割り込みが入ると二重例外やデッドロックの原因になる。
    x86_64::instructions::interrupts::disable();

    // 2. シリアルに出力する。
    //    try_lock() でデッドロックを回避する。
    //    ロック取得に失敗した場合は I/O ポートに直接書き込む。
    if let Some(mut serial) = crate::serial::SERIAL1.try_lock() {
        // Mutex 取得成功 → 通常の write_fmt で PanicInfo をフォーマット出力
        let _ = serial.write_str("\n========================================\n");
        let _ = serial.write_str("KERNEL PANIC!\n");
        let _ = serial.write_str("========================================\n");
        let _ = write!(serial, "{}\n", info);
        let _ = serial.write_str("========================================\n");
        let _ = serial.write_str("System halted.\n");
    } else {
        // Mutex がロック中 → I/O ポートに直接書き込み（フォールバック）
        // RawSerialWriter を使って PanicInfo もフォーマット出力できる
        unsafe { serial_write_raw(b"\n========================================\n") };
        unsafe { serial_write_raw(b"KERNEL PANIC!\n") };
        unsafe { serial_write_raw(b"========================================\n") };
        let _ = write!(RawSerialWriter, "{}\n", info);
        unsafe { serial_write_raw(b"========================================\n") };
        unsafe { serial_write_raw(b"System halted.\n") };
    }

    // 3. フレームバッファに出力する（赤字で目立つように）。
    //    try_lock() でデッドロックを回避する。
    //    ロック取得に失敗した場合はスキップ（シリアルがあればデバッグには十分）。
    if let Some(mut writer_guard) = crate::framebuffer::WRITER.try_lock() {
        if let Some(w) = writer_guard.as_mut() {
            // 赤字 + 黒背景で目立たせる
            w.set_colors((255, 0, 0), (0, 0, 0));
            let _ = w.write_str("\n========================================\n");
            let _ = w.write_str("KERNEL PANIC!\n");
            let _ = w.write_str("========================================\n");
            let _ = write!(w, "{}\n", info);
            let _ = w.write_str("========================================\n");
            let _ = w.write_str("System halted.\n");
        }
    }

    // 4. hlt ループで CPU を停止する。
    //    割り込みは既に無効化されているので、hlt から復帰することはない。
    //    ただし念のため loop で囲んでおく（NMI で起きる可能性があるため）。
    loop {
        x86_64::instructions::hlt();
    }
}
