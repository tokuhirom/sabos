#![no_main]
#![no_std]
#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]

extern crate alloc;

mod allocator;
mod console;
mod elf;
mod fat16;
mod framebuffer;
mod gdt;
mod handle;
mod interrupts;
mod ipc;
mod memory;
mod mouse;
mod paging;
mod panic;
mod procfs;
mod scheduler;
mod serial;
mod pci;
mod shell;
mod syscall;
mod net;
mod user_ptr;
mod usermode;
mod vfs;
mod virtio_blk;
mod virtio_net;

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
    allocator::init(&memory_map);

    // --- ページング管理の初期化 ---
    // UEFI が設定済みのページテーブルを OffsetPageTable でラップし、
    // 物理フレームアロケータも初期化する。
    // ヒープが必要（Vec を使うため）なので allocator::init() の後に呼ぶ。
    paging::init(&memory_map);

    // --- グローバルフレームバッファライターの初期化 ---
    // これ以降は kprint!/kprintln! マクロでどこからでも画面に出力できる。
    // 割り込みハンドラ（キーボード）からも安全に書ける。
    framebuffer::init_global_writer(fb_info);

    // --- PS/2 マウスの初期化 ---
    // IRQ12 を有効化し、マウスからのパケット受信を開始する。
    if mouse::init() {
        kprintln!("Mouse initialized.");
    } else {
        kprintln!("Mouse not available.");
    }

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

    // --- スケジューラの初期化 ---
    // 現在の実行コンテキストを task 0 ("kernel") として登録する。
    // 割り込み有効化の後に呼ぶ（タスク内で割り込みが必要になるため）。
    scheduler::init();
    framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
    kprintln!("Scheduler initialized.");
    kprintln!();

    // --- virtio-blk ドライバの初期化 ---
    // PCI バスから virtio-blk デバイスを探して初期化する。
    // ヒープアロケータとページング初期化の後に呼ぶ必要がある
    // （Virtqueue のメモリを確保するため）。
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprint!("Initializing virtio-blk... ");
    virtio_blk::init();
    {
        let drv = virtio_blk::VIRTIO_BLK.lock();
        if let Some(ref d) = *drv {
            framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
            kprintln!("OK ({} sectors, {} MiB)", d.capacity(), d.capacity() * 512 / 1024 / 1024);
        } else {
            framebuffer::set_global_colors((255, 255, 0), (0, 0, 128));
            kprintln!("not found");
        }
    }
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprintln!();

    // --- virtio-net の初期化 ---
    // virtio-net デバイスが存在する場合のみ初期化する。
    kprint!("Initializing virtio-net... ");
    virtio_net::init();
    {
        let drv = virtio_net::VIRTIO_NET.lock();
        if let Some(ref d) = *drv {
            framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
            kprintln!(
                "OK (MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
                d.mac_address[0], d.mac_address[1], d.mac_address[2],
                d.mac_address[3], d.mac_address[4], d.mac_address[5]
            );
        } else {
            framebuffer::set_global_colors((255, 255, 0), (0, 0, 128));
            kprintln!("not found (add -device virtio-net-pci to QEMU)");
        }
    }
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprintln!();

    // --- マルチタスクのデモ ---
    // 2つのデモタスクを生成して、協調的マルチタスクの動作を確認する。
    // 各タスクはメッセージを表示して yield を繰り返し、最後に return する。
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprintln!("Spawning demo tasks...");
    scheduler::spawn("demo_a", demo_task_a);
    scheduler::spawn("demo_b", demo_task_b);

    kprintln!("Running demo tasks:");
    // yield_now() で kernel → demo_a → demo_b → kernel ... とラウンドロビンする。
    // 全タスクが Finished になるまで繰り返す。
    while scheduler::has_ready_tasks() {
        scheduler::yield_now();
    }

    framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
    kprintln!("All demo tasks finished!");
    kprintln!();

    // --- プリエンプティブマルチタスクのデモ ---
    // yield_now() を呼ばないビジーループタスクを2つ生成する。
    // タイマー割り込みによる強制切り替え（プリエンプション）が正しく動いていれば、
    // 各タスクが交互にメッセージを出力するはず。
    // yield を使わずに切り替わることがプリエンプティブの証明。
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprintln!("Spawning preemptive demo tasks (no yield)...");
    scheduler::spawn("preempt_x", preemptive_task_x);
    scheduler::spawn("preempt_y", preemptive_task_y);

    kprintln!("Running preemptive demo tasks:");
    // kernel タスクも yield_now() で Ready に戻り、
    // タイマー割り込みがラウンドロビンで全タスクを切り替える。
    // ただし kernel タスクはここで Ready タスクの完了を待つ必要があるので、
    // yield_now() でループする。
    while scheduler::has_ready_tasks() {
        scheduler::yield_now();
    }

    framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
    kprintln!("All preemptive demo tasks finished!");
    let (calls, switches) = scheduler::preempt_stats();
    let ticks = interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    kprintln!("  timer ticks: {}, preempt() called: {}, switched: {}", ticks, calls, switches);
    kprintln!();

    // --- sleep デモ ---
    // sleep_ms() を使ってタスクを一定時間停止させるデモ。
    // busy-wait ではなくタスクを Sleeping 状態にするので、
    // スリープ中は CPU を他のタスクに譲れる（CPU 時間を無駄にしない）。
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    kprintln!("Spawning sleep demo tasks...");
    scheduler::spawn("sleep_1", sleep_demo_1);
    scheduler::spawn("sleep_2", sleep_demo_2);

    kprintln!("Running sleep demo tasks:");
    while scheduler::has_ready_tasks() {
        scheduler::yield_now();
    }

    framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
    kprintln!("All sleep demo tasks finished!");
    kprintln!();

    // --- init プロセスの起動 ---
    // disk.img から INIT.ELF を読み込んで最初のユーザープロセスとして起動する。
    // init は netd と shell を起動し、supervisor として常駐する。
    // init が終了した場合はカーネルシェルにフォールバックする。
    framebuffer::set_global_colors((255, 255, 0), (0, 0, 128));
    kprintln!("Loading init from disk...");
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));

    // FAT16 から INIT.ELF を読み込む
    match fat16::Fat16::new() {
        Ok(fs) => {
            match fs.read_file("/INIT.ELF") {
                Ok(elf_data) => {
                    kprintln!("Loaded INIT.ELF ({} bytes)", elf_data.len());

                    // init をバックグラウンドで起動
                    match scheduler::spawn_user("init", &elf_data) {
                        Ok(task_id) => {
                            framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                            kprintln!("Init process started (task {})", task_id);
                            kprintln!();
                            framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                        }
                        Err(e) => {
                            framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                            kprintln!("Failed to start init: {}", e);
                            framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                        }
                    }
                }
                Err(e) => {
                    framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                    kprintln!("Failed to load INIT.ELF: {:?}", e);
                    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                }
            }
        }
        Err(e) => {
            framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
            kprintln!("Failed to initialize FAT16: {:?}", e);
            framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
        }
    }

    // --- カーネルタスクは idle として待機 ---
    // init が起動したら、カーネルタスクは yield して他のタスクに CPU を譲る。
    // ユーザープロセスがすべて終了したらカーネルシェルにフォールバックする。
    kprintln!("Kernel entering idle mode...");
    kprintln!();

    // ユーザープロセスが動いている間は yield で待機
    while scheduler::has_ready_tasks() {
        scheduler::yield_now();
    }

    // 全ユーザープロセスが終了したらカーネルシェルにフォールバック
    framebuffer::set_global_colors((255, 255, 0), (0, 0, 128));
    kprintln!("All user processes exited.");
    kprintln!("Falling back to kernel shell. Type 'help' for commands.");
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

// =================================================================
// マルチタスクのデモ用タスク
// =================================================================
//
// 各タスクはメッセージを表示して yield_now() で CPU を譲る。
// これを数回繰り返してから return する。
// return すると task_trampoline → task_exit_handler で自動的に Finished になる。

/// デモタスク A: メッセージを3回表示する。
fn demo_task_a() {
    framebuffer::set_global_colors((100, 200, 255), (0, 0, 128));
    kprintln!("  [Task A] Hello! (1/3)");
    scheduler::yield_now();
    kprintln!("  [Task A] Running! (2/3)");
    scheduler::yield_now();
    kprintln!("  [Task A] Done! (3/3)");
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
}

/// デモタスク B: メッセージを3回表示する。
fn demo_task_b() {
    framebuffer::set_global_colors((255, 200, 100), (0, 0, 128));
    kprintln!("  [Task B] Hello! (1/3)");
    scheduler::yield_now();
    kprintln!("  [Task B] Running! (2/3)");
    scheduler::yield_now();
    kprintln!("  [Task B] Done! (3/3)");
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
}

// =================================================================
// プリエンプティブマルチタスクのデモ用タスク
// =================================================================
//
// これらのタスクは yield_now() を一切呼ばない。
// それでもタイマー割り込み（IRQ 0）でプリエンプションが発生し、
// 強制的に他のタスクに切り替わる。
// 協調的デモと違い「自発的に譲らなくても切り替わる」ことを実証する。
//
// ビジーループで一定回数待ってからメッセージを表示する方式。
// ループ回数は PIT の周波数（約 18.2 Hz = 約 55ms 間隔）を考慮して、
// タイマー割り込みが何回か発火する程度の長さにしている。

/// ビジーウェイト用のヘルパー関数。
/// インラインアセンブリの nop ループで、コンパイラの最適化に左右されない
/// 確実なビジーウェイトを行う。
fn busy_wait(iterations: u64) {
    // インラインアセンブリでカウントダウンループを実装する。
    // コンパイラの最適化で消されることがない。
    // `pause` 命令はスピンループ向けのヒントで、CPU のパイプラインを効率化する。
    unsafe {
        core::arch::asm!(
            "2:",
            "pause",
            "sub {0}, 1",
            "jnz 2b",
            inout(reg) iterations => _,
            options(nostack, nomem),
        );
    }
}

/// プリエンプティブデモタスク X:
/// yield を使わずにメッセージを3回表示する。
/// タイマー割り込みによるプリエンプションで強制的に切り替わる。
fn preemptive_task_x() {
    framebuffer::set_global_colors((200, 100, 255), (0, 0, 128));
    kprintln!("  [Preempt X] Start (1/3)");
    busy_wait(15_000_000);
    kprintln!("  [Preempt X] Middle (2/3)");
    busy_wait(15_000_000);
    kprintln!("  [Preempt X] Done (3/3)");
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
}

/// プリエンプティブデモタスク Y:
/// yield を使わずにメッセージを3回表示する。
/// タイマー割り込みによるプリエンプションで強制的に切り替わる。
fn preemptive_task_y() {
    framebuffer::set_global_colors((255, 100, 200), (0, 0, 128));
    kprintln!("  [Preempt Y] Start (1/3)");
    busy_wait(15_000_000);
    kprintln!("  [Preempt Y] Middle (2/3)");
    busy_wait(15_000_000);
    kprintln!("  [Preempt Y] Done (3/3)");
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
}

// =================================================================
// sleep デモ用タスク
// =================================================================
//
// sleep_ms() を使って一定時間スリープしてからメッセージを表示する。
// busy-wait と違い、スリープ中は CPU を他のタスクに譲る。
// タイマーティックで起床時刻に達すると自動的に Ready に戻される。

/// sleep デモタスク 1: 500ms スリープを挟んでメッセージを表示する。
fn sleep_demo_1() {
    framebuffer::set_global_colors((100, 255, 100), (0, 0, 128));
    kprintln!("  [Sleep 1] Start, sleeping 500ms...");
    scheduler::sleep_ms(500);
    kprintln!("  [Sleep 1] Woke up! Sleeping 500ms more...");
    scheduler::sleep_ms(500);
    kprintln!("  [Sleep 1] Done!");
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
}

/// sleep デモタスク 2: 300ms スリープを挟んでメッセージを表示する。
/// タスク 1 より短い間隔でスリープするので、先に起きることがある。
fn sleep_demo_2() {
    framebuffer::set_global_colors((255, 255, 100), (0, 0, 128));
    kprintln!("  [Sleep 2] Start, sleeping 300ms...");
    scheduler::sleep_ms(300);
    kprintln!("  [Sleep 2] Woke up! Sleeping 300ms more...");
    scheduler::sleep_ms(300);
    kprintln!("  [Sleep 2] Done!");
    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
}
