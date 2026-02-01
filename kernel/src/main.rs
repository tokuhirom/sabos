#![no_main]
#![no_std]

use core::fmt::Write;
use uefi::prelude::*;
use uefi::proto::console::gop::{BltOp, BltPixel, GraphicsOutput};

#[entry]
fn main() -> Status {
    // --- シリアルコンソールに挨拶 ---
    uefi::system::with_stdout(|stdout| {
        stdout.write_str("Hello, SABOS!\r\n").unwrap();
    });

    // --- GOP (Graphics Output Protocol) を取得して画面に描画する ---
    // GOP は UEFI が提供するグラフィックス描画の仕組み。
    // フレームバッファ（ピクセルデータを書き込むメモリ領域）にアクセスできる。
    // まずは GOP プロトコルを持つハンドルを探して、排他的にオープンする。
    let gop_handle = uefi::boot::get_handle_for_protocol::<GraphicsOutput>()
        .expect("GOP not found");
    let mut gop = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle)
        .expect("Failed to open GOP");

    // 現在の画面モードの情報を取得する。
    // resolution() は (幅, 高さ) のピクセル数を返す。
    let mode_info = gop.current_mode_info();
    let (width, height) = mode_info.resolution();

    // シリアルコンソールに画面情報を表示
    uefi::system::with_stdout(|stdout| {
        write!(stdout, "GOP: {}x{}, format: {:?}\r\n",
            width, height, mode_info.pixel_format()).unwrap();
    });

    // --- 画面を青で塗りつぶす ---
    // BltOp::VideoFill は指定した矩形を単色で塗りつぶす操作。
    // Blt = Block Transfer の略。GPU のハードウェア機能で高速に塗りつぶせる。
    let blue = BltPixel::new(0, 0, 255);
    gop.blt(BltOp::VideoFill {
        color: blue,
        dest: (0, 0),           // 左上 (0,0) から
        dims: (width, height),  // 画面全体を
    }).expect("Failed to fill screen");

    // --- 画面中央に白い矩形を描く ---
    // 青一色だと寂しいので、中央に白い四角を置いてみる。
    let white = BltPixel::new(255, 255, 255);
    let rect_w = width / 4;
    let rect_h = height / 4;
    let rect_x = (width - rect_w) / 2;
    let rect_y = (height - rect_h) / 2;
    gop.blt(BltOp::VideoFill {
        color: white,
        dest: (rect_x, rect_y),
        dims: (rect_w, rect_h),
    }).expect("Failed to draw rectangle");

    // シリアルにも描画完了を報告
    uefi::system::with_stdout(|stdout| {
        stdout.write_str("Screen painted!\r\n").unwrap();
    });

    // 描画した状態で停止。画面を見て楽しむ。
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
