#![no_main]
#![no_std]

mod framebuffer;

use core::fmt::Write;
use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::mem::memory_map::{MemoryMap, MemoryType};

use crate::framebuffer::{FramebufferInfo, FramebufferWriter};

#[entry]
fn main() -> Status {
    // --- シリアルコンソールに挨拶 ---
    uefi::system::with_stdout(|stdout| {
        stdout.write_str("Hello, SABOS!\r\n").unwrap();
    });

    // --- GOP (Graphics Output Protocol) を取得 ---
    let gop_handle = uefi::boot::get_handle_for_protocol::<GraphicsOutput>()
        .expect("GOP not found");
    let mut gop = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle)
        .expect("Failed to open GOP");

    // 画面情報をシリアルに表示
    let mode_info = gop.current_mode_info();
    let (width, height) = mode_info.resolution();
    uefi::system::with_stdout(|stdout| {
        write!(stdout, "GOP: {}x{}, format: {:?}\r\n",
            width, height, mode_info.pixel_format()).unwrap();
    });

    // --- Exit Boot Services の前にフレームバッファ情報を保存する ---
    // Exit 後は GOP プロトコルが使えなくなるが、
    // フレームバッファの物理アドレス自体は有効なまま残る。
    // 今のうちにアドレス・サイズ・解像度・ピクセルフォーマットを控えておく。
    let fb_info = FramebufferInfo::from_gop(&mut gop);

    uefi::system::with_stdout(|stdout| {
        write!(stdout, "FB saved: {:#x}\r\n", fb_info.fb_addr).unwrap();
    });

    // --- メモリマップのサマリーを表示する ---
    // Exit Boot Services の前に、UEFI にメモリマップを教えてもらう。
    // メモリマップは「この物理アドレス範囲はこういう種類のメモリです」という一覧。
    // カーネルが自分でメモリ管理をするために必要な情報。
    {
        let memory_map = uefi::boot::memory_map(MemoryType::LOADER_DATA)
            .expect("Failed to get memory map");

        let mut usable_pages: u64 = 0;
        let entry_count = memory_map.entries().len();
        for desc in memory_map.entries() {
            if desc.ty == MemoryType::CONVENTIONAL {
                usable_pages += desc.page_count;
            }
        }
        let usable_mib = usable_pages * 4096 / 1024 / 1024;

        uefi::system::with_stdout(|stdout| {
            write!(stdout, "Memory map: {} entries, {} MiB usable\r\n",
                entry_count, usable_mib).unwrap();
        });
        // memory_map はここで drop される。
        // exit_boot_services は自前で新しいメモリマップを取得する。
    }

    // --- GOP のプロトコルハンドルを解放する ---
    // Exit Boot Services を呼ぶ前に、UEFI プロトコルへの参照をすべて手放す必要がある。
    // ScopedProtocol は drop 時に close_protocol を呼ぶので、ここで明示的に drop する。
    drop(gop);

    // =================================================================
    // Exit Boot Services — ここが UEFI アプリからカーネルへの分岐点
    // =================================================================
    // この呼び出し以降:
    //   - UEFI の Boot Services（メモリ確保、プロトコル、コンソール出力等）は使えない
    //   - 全メモリ・全ハードウェアの管理責任がカーネルに移る
    //   - 唯一 UEFI Runtime Services（時刻取得等）だけは引き続き使える
    //   - シリアルコンソールへの UEFI 経由の出力もここで終わり
    uefi::system::with_stdout(|stdout| {
        stdout.write_str("Exiting boot services...\r\n").unwrap();
    });

    let _memory_map = unsafe { uefi::boot::exit_boot_services(None) };

    // =================================================================
    // ここからはカーネルの世界。UEFI の助けはもう借りられない。
    // =================================================================

    // Exit Boot Services 後でもフレームバッファは生きている。
    // 保存しておいた情報を使って FramebufferWriter を再構築する。
    let mut fb = FramebufferWriter::from_info(fb_info);

    // 紺色の背景で画面をクリア
    fb.clear();

    // タイトルを黄色で表示
    fb.set_colors((255, 255, 0), (0, 0, 128));
    fb.write_str("=== SABOS ===\n\n");

    // 画面情報を白色で表示
    fb.set_colors((255, 255, 255), (0, 0, 128));
    write!(fb, "Framebuffer: {}x{}\n", fb_info.width, fb_info.height).unwrap();
    write!(fb, "Pixel format: {:?}\n", fb_info.pixel_format).unwrap();
    fb.write_str("\n");

    // Boot Services を抜けたことを表示
    fb.set_colors((0, 255, 0), (0, 0, 128));
    fb.write_str("Boot services exited successfully!\n");
    fb.write_str("Kernel is now in control.\n\n");

    // メモリマップのサマリーを表示
    fb.set_colors((255, 255, 255), (0, 0, 128));
    fb.write_str("Memory map:\n");

    // 使用可能なメモリの合計を計算して表示
    let mut usable_pages: u64 = 0;
    for desc in _memory_map.entries() {
        // CONVENTIONAL_MEMORY が OS が自由に使えるメモリ
        if desc.ty == MemoryType::CONVENTIONAL {
            usable_pages += desc.page_count;
        }
    }
    // 1 ページ = 4KiB なので、ページ数 * 4096 / 1024 / 1024 = MiB
    let usable_mib = usable_pages * 4096 / 1024 / 1024;
    write!(fb, "  Usable memory: {} MiB ({} pages)\n", usable_mib, usable_pages).unwrap();
    write!(fb, "  Total entries: {}\n", _memory_map.entries().len()).unwrap();

    // カーネルとして停止。ここからページング、割り込み、ドライバへと進む。
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
