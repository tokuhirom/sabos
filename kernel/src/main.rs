#![no_main]
#![no_std]

mod framebuffer;

use core::fmt::Write;
use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;

use crate::framebuffer::FramebufferWriter;

#[entry]
fn main() -> Status {
    // --- シリアルコンソールに挨拶 ---
    uefi::system::with_stdout(|stdout| {
        stdout.write_str("Hello, SABOS!\r\n").unwrap();
    });

    // --- GOP (Graphics Output Protocol) を取得 ---
    // GOP は UEFI が提供するグラフィックス描画の仕組み。
    // フレームバッファ（ピクセルデータを書き込むメモリ領域）にアクセスできる。
    let gop_handle = uefi::boot::get_handle_for_protocol::<GraphicsOutput>()
        .expect("GOP not found");
    let mut gop = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle)
        .expect("Failed to open GOP");

    // 画面情報を取得してシリアルに表示
    let mode_info = gop.current_mode_info();
    let (width, height) = mode_info.resolution();
    uefi::system::with_stdout(|stdout| {
        write!(stdout, "GOP: {}x{}, format: {:?}\r\n",
            width, height, mode_info.pixel_format()).unwrap();
    });

    // --- フレームバッファに直接テキストを描画する ---
    // BltOp は矩形塗りつぶしには便利だけど、文字を描くには
    // フレームバッファに直接ピクセルを書き込むほうが効率的。
    // font8x8 crate の 8x8 ビットマップフォントを使う。
    let mut fb = FramebufferWriter::new(&mut gop);

    // 紺色の背景で画面をクリア
    fb.clear();

    // タイトルを黄色で表示
    fb.set_colors((255, 255, 0), (0, 0, 128));
    fb.write_str("=== SABOS ===\n\n");

    // 画面情報を白色で表示
    fb.set_colors((255, 255, 255), (0, 0, 128));
    write!(fb, "Framebuffer: {}x{}\n", width, height).unwrap();
    write!(fb, "Pixel format: {:?}\n", mode_info.pixel_format()).unwrap();
    write!(fb, "Stride: {} pixels/line\n", mode_info.stride()).unwrap();
    fb.write_str("\n");

    // メッセージ
    fb.set_colors((0, 255, 0), (0, 0, 128));
    fb.write_str("Hello from the framebuffer!\n");
    fb.write_str("Text rendering with font8x8 works!\n");

    uefi::system::with_stdout(|stdout| {
        stdout.write_str("Framebuffer text rendered!\r\n").unwrap();
    });

    // 描画した状態で停止。画面を見て楽しむ。
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
