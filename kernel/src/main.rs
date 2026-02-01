#![no_main]
#![no_std]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod allocator;
mod framebuffer;
mod gdt;
mod interrupts;
mod memory;
mod paging;
mod serial;
mod shell;

// kprint! / kprintln! マクロを使えるようにする。
// #[macro_export] で定義されたマクロはクレートルートに配置される。

use core::fmt::Write;
use uefi::prelude::*;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::mem::memory_map::{MemoryMap, MemoryType};

use crate::framebuffer::FramebufferInfo;

#[entry]
fn main() -> Status {
    // --- シリアルコンソールに挨拶 ---
    uefi::system::with_stdout(|stdout| {
        let _ = stdout.write_str("Hello, SABOS!\r\n");
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
            width, height, mode_info.pixel_format()).ok();
    });

    // --- Exit Boot Services の前にフレームバッファ情報を保存する ---
    // Exit 後は GOP プロトコルが使えなくなるが、
    // フレームバッファの物理アドレス自体は有効なまま残る。
    // 今のうちにアドレス・サイズ・解像度・ピクセルフォーマットを控えておく。
    let fb_info = FramebufferInfo::from_gop(&mut gop);

    uefi::system::with_stdout(|stdout| {
        let _ = write!(stdout, "FB saved: {:#x}\r\n", fb_info.fb_addr);
    });

    // --- メモリマップのサマリーを表示する ---
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
                entry_count, usable_mib).ok();
        });
    }

    // --- GOP のプロトコルハンドルを解放する ---
    drop(gop);

    // =================================================================
    // Exit Boot Services — ここが UEFI アプリからカーネルへの分岐点
    // =================================================================
    uefi::system::with_stdout(|stdout| {
        let _ = stdout.write_str("Exiting boot services...\r\n");
    });

    let memory_map = unsafe { uefi::boot::exit_boot_services(None) };

    // =================================================================
    // ここからはカーネルの世界。UEFI の助けはもう借りられない。
    // =================================================================

    // --- GDT (Global Descriptor Table) の初期化 ---
    gdt::init();

    // --- IDT + PIC の初期化 ---
    // CPU 例外ハンドラと、ハードウェア割り込み（タイマー、キーボード）のハンドラを登録。
    // PIC を初期化して IRQ 0〜15 を IDT の 32〜47 番にリマップする。
    interrupts::init();

    // --- ヒープアロケータの初期化 ---
    allocator::init();

    // --- ページング管理の初期化 ---
    // UEFI が設定済みのページテーブルを OffsetPageTable でラップし、
    // 物理フレームアロケータも初期化する。
    // ヒープが必要（Vec を使うため）なので allocator::init() の後に呼ぶ。
    paging::init(&memory_map);

    // --- グローバルフレームバッファライターの初期化 ---
    // これ以降は kprint!/kprintln! マクロでどこからでも画面に出力できる。
    // 割り込みハンドラ（キーボード）からも安全に書ける。
    framebuffer::init_global_writer(fb_info);

    // タイトルを黄色で表示
    framebuffer::set_global_colors((255, 255, 0), (0, 0, 128));
    kprintln!("=== SABOS ===");
    kprintln!();

    // 画面情報を白色で表示
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprintln!("Framebuffer: {}x{}", fb_info.width, fb_info.height);
    kprintln!("Pixel format: {:?}", fb_info.pixel_format);
    kprintln!();

    // Boot Services を抜けたことを表示
    framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
    kprintln!("Boot services exited successfully!");
    kprintln!("Kernel is now in control.");
    kprintln!();

    // メモリマップのサマリーを表示
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprintln!("Memory map:");
    let mut usable_pages: u64 = 0;
    for desc in memory_map.entries() {
        if desc.ty == MemoryType::CONVENTIONAL {
            usable_pages += desc.page_count;
        }
    }
    let usable_mib = usable_pages * 4096 / 1024 / 1024;
    kprintln!("  Usable memory: {} MiB ({} pages)", usable_mib, usable_pages);
    kprintln!("  Total entries: {}", memory_map.entries().len());
    kprintln!();

    // 初期化成功を表示
    framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
    kprintln!("GDT initialized.");
    kprintln!("IDT initialized.");
    kprintln!("PIC initialized.");
    kprintln!("Heap allocator initialized.");
    kprintln!("Paging initialized (CR3: {:#x}).", paging::read_cr3().as_u64());
    kprintln!();

    // int3 テスト
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprint!("Testing int3 breakpoint... ");
    x86_64::instructions::interrupts::int3();
    framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
    kprintln!("OK!");
    kprintln!();

    // ページングのテスト
    // 仮想アドレスへのマッピング作成 → 変換確認 → 解除の一連を検証する。
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    paging::demo_mapping();
    kprintln!();

    // --- 割り込みを有効化 (sti 命令) ---
    // ここで CPU の割り込みフラグを立てる。
    // これ以降、タイマー割り込みとキーボード割り込みが CPU に届くようになる。
    // sti の前にすべての初期化を終えておくこと。
    framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
    kprintln!("Enabling hardware interrupts...");

    x86_64::instructions::interrupts::enable();

    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprintln!("Hardware interrupts enabled!");
    kprintln!();

    // --- シェルの起動 ---
    framebuffer::set_global_colors((255, 255, 0), (0, 0, 128));
    kprintln!("Welcome to SABOS shell! Type 'help' for commands.");
    kprintln!();
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));

    let mut shell = shell::Shell::new(usable_mib, usable_pages);
    shell.print_prompt();

    // --- メインループ ---
    // キーボード割り込みで KEY_QUEUE にプッシュされた文字を読み取り、
    // シェルに渡す。キーがなければ hlt で CPU を省電力モードにして待つ。
    //
    // enable_and_hlt() は sti と hlt をアトミックに実行する。
    // これにより「キューチェック → hlt の間に割り込みが来て取りこぼす」
    // というレースコンディションを防ぐ。
    loop {
        // 割り込みを無効化してキューをチェック
        x86_64::instructions::interrupts::disable();

        if let Some(c) = interrupts::get_key() {
            // キーがあった場合は割り込みを再有効化してから処理
            x86_64::instructions::interrupts::enable();
            shell.handle_char(c);
        } else {
            // キーがない場合は sti+hlt をアトミックに実行して割り込みを待つ
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }
}
