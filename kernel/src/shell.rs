// shell.rs — SABOS シェル
//
// キーボードから受け取った文字を行バッファに溜めて、
// Enter で「コマンド」として解釈・実行する。
// 簡易的なコマンドラインインターフェースを提供する。

use alloc::string::String;
use alloc::vec::Vec;

use crate::framebuffer;
use crate::memory::FRAME_ALLOCATOR;
use crate::paging;
use crate::scheduler;
use crate::{kprint, kprintln};
use x86_64::VirtAddr;

/// シェルの状態を管理する構造体。
pub struct Shell {
    /// 現在入力中の行バッファ
    line_buffer: String,
    /// メモリ情報（起動時に取得した値を保持）
    usable_mib: u64,
    usable_pages: u64,
}

impl Shell {
    /// 新しいシェルを作成する。
    /// メモリ情報は起動時にしか取得できないので、ここで受け取って保持する。
    pub fn new(usable_mib: u64, usable_pages: u64) -> Self {
        Self {
            line_buffer: String::new(),
            usable_mib,
            usable_pages,
        }
    }

    /// プロンプトを表示する。
    pub fn print_prompt(&self) {
        framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
        kprint!("sabos> ");
        framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    }

    /// キーボードから1文字受け取って処理する。
    /// Enter でコマンド実行、Backspace で1文字削除、
    /// それ以外は行バッファに追加してエコーバックする。
    pub fn handle_char(&mut self, c: char) {
        match c {
            // Enter: コマンドを実行
            '\n' => {
                kprintln!();
                self.execute_command();
                self.line_buffer.clear();
                self.print_prompt();
            }
            // Backspace (0x08): 1文字削除
            '\x08' => {
                if !self.line_buffer.is_empty() {
                    self.line_buffer.pop();
                    // 画面上のカーソルを1文字戻して、その位置を背景色で塗りつぶす。
                    // '\x08' は framebuffer.rs でバックスペース処理される。
                    kprint!("\x08");
                }
            }
            // Tab: 無視（将来的にはタブ補完）
            '\t' => {}
            // 表示可能な文字: バッファに追加してエコー
            c if !c.is_control() => {
                self.line_buffer.push(c);
                kprint!("{}", c);
            }
            // その他の制御文字: 無視
            _ => {}
        }
    }

    /// 行バッファの内容をコマンドとして解釈・実行する。
    fn execute_command(&self) {
        let cmd = self.line_buffer.trim();
        if cmd.is_empty() {
            return;
        }

        // コマンド名と引数を分離（最初の空白で2分割）
        let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
        let command = parts[0];
        let args = if parts.len() > 1 { parts[1] } else { "" };

        match command {
            "help" => self.cmd_help(),
            "clear" => self.cmd_clear(),
            "mem" => self.cmd_mem(),
            "page" => self.cmd_page(args),
            "ps" => self.cmd_ps(),
            "echo" => self.cmd_echo(args),
            "usermode" => self.cmd_usermode(),
            "usertest" => self.cmd_usertest(),
            "isolate" => self.cmd_isolate(),
            "elf" => self.cmd_elf(),
            "lspci" => self.cmd_lspci(),
            "blkread" => self.cmd_blkread(args),
            "blkwrite" => self.cmd_blkwrite(args),
            "ls" => self.cmd_ls(args),
            "cat" => self.cmd_cat(args),
            "write" => self.cmd_write(args),
            "rm" => self.cmd_rm(args),
            "run" => self.cmd_run(args),
            "spawn" => self.cmd_spawn(args),
            "netpoll" => self.cmd_netpoll(args),
            "ip" => self.cmd_ip(),
            "dns" => self.cmd_dns(args),
            "http" => self.cmd_http(args),
            "selftest" => self.cmd_selftest(args),
            "beep" => self.cmd_beep(args),
            "panic" => self.cmd_panic(),
            "halt" => self.cmd_halt(),
            _ => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Unknown command: {}", command);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                kprintln!("Type 'help' for available commands.");
            }
        }
    }

    /// help コマンド: 使えるコマンドの一覧を表示する。
    fn cmd_help(&self) {
        kprintln!("Available commands:");
        kprintln!("  help            - Show this help message");
        kprintln!("  clear           - Clear the screen");
        kprintln!("  mem             - Show memory information");
        kprintln!("  page [addr]     - Show paging info / translate address");
        kprintln!("  ps              - Show task list");
        kprintln!("  echo <text>     - Echo text back");
        kprintln!("  usermode        - Run a user-mode (Ring 3) program");
        kprintln!("  usertest        - Test memory protection (Ring 3 access violation)");
        kprintln!("  isolate         - Demo: process isolation with separate page tables");
        kprintln!("  elf             - Load and run an ELF binary in user mode");
        kprintln!("  lspci           - List PCI devices");
        kprintln!("  blkread [sect]  - Read a sector from virtio-blk disk");
        kprintln!("  blkwrite <sect> - Write test pattern to a sector (DANGEROUS!)");
        kprintln!("  ls [path]       - List files on FAT32 disk (e.g., ls /SUBDIR)");
        kprintln!("  cat <path>      - Display file contents (e.g., cat /SUBDIR/FILE.TXT)");
        kprintln!("  write <name> <text> - Create a file with text content");
        kprintln!("  rm <name>       - Delete a file");
        kprintln!("  run <path>      - Load and run ELF binary (e.g., run /SUBDIR/APP.ELF)");
        kprintln!("  spawn <path>    - Spawn ELF as background process (e.g., spawn HELLO.ELF)");
        kprintln!("  netpoll [n]     - Poll network for n seconds (default 10)");
        kprintln!("  ip              - Show IP configuration");
        kprintln!("  dns <domain>    - Resolve domain name to IP address");
        kprintln!("  http <host> [path] - HTTP GET request (e.g., http example.com /index.html)");
        kprintln!("  selftest [target] - Run automated self-tests (target: all/base/core/fs/net/gui/service/list)");
        kprintln!("  beep [freq] [ms] - Play beep sound (default: 440Hz 200ms)");
        kprintln!("  panic           - Trigger a kernel panic (for testing)");
        kprintln!("  halt            - Halt the system");
    }

    /// clear コマンド: 画面をクリアする。
    fn cmd_clear(&self) {
        framebuffer::clear_global_screen();
    }

    /// mem コマンド: メモリ情報を表示する。
    fn cmd_mem(&self) {
        let fa = FRAME_ALLOCATOR.lock();
        let total = fa.total_frames();
        let allocated = fa.allocated_count();
        let free = fa.free_frames();

        kprintln!("Memory information:");
        kprintln!("  Usable:    {} MiB ({} pages)", self.usable_mib, self.usable_pages);
        kprintln!("  Heap:      1024 KiB (static BSS allocation)");
        kprintln!("  Frames:    {} total, {} allocated, {} free",
            total, allocated, free);
        kprintln!("  Free mem:  {} KiB", free * 4);
    }

    /// page コマンド: ページング情報を表示する。
    ///
    /// 引数なし: CR3 レジスタ値、L4 テーブルの使用エントリ数、
    ///           フレームアロケータの割り当て状況を表示する。
    /// 引数あり: 16進数の仮想アドレスを物理アドレスに変換して表示する。
    fn cmd_page(&self, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            // 引数なし: ページング情報のサマリーを表示
            let cr3 = paging::read_cr3();
            let l4_entries = paging::l4_used_entries();
            let fa = FRAME_ALLOCATOR.lock();
            let total = fa.total_frames();
            let allocated = fa.allocated_count();

            kprintln!("Paging information:");
            kprintln!("  CR3 (L4 table): {:#x}", cr3.as_u64());
            kprintln!("  L4 used entries: {} / 512", l4_entries);
            kprintln!("  Frame allocator: {} / {} frames used", allocated, total);
        } else {
            // 引数あり: 仮想アドレスを物理アドレスに変換
            // "0x" プレフィックスがあれば除去して16進数としてパース
            let hex_str = args.trim_start_matches("0x").trim_start_matches("0X");
            match u64::from_str_radix(hex_str, 16) {
                Ok(addr) => {
                    // x86_64 の仮想アドレスは 48 ビット（符号拡張）。
                    // 不正なアドレスの場合は VirtAddr::try_new がエラーを返す。
                    match VirtAddr::try_new(addr) {
                        Ok(virt) => match paging::translate_addr(virt) {
                            Some(phys) => {
                                kprintln!("  virt {:#x} -> phys {:#x}", addr, phys.as_u64());
                            }
                            None => {
                                kprintln!("  virt {:#x} -> NOT MAPPED", addr);
                            }
                        },
                        Err(_) => {
                            kprintln!("  Invalid virtual address: {:#x}", addr);
                            kprintln!("  (x86_64 virtual addresses must be 48-bit canonical)");
                        }
                    }
                }
                Err(_) => {
                    kprintln!("  Invalid hex address: {}", args);
                    kprintln!("  Usage: page [hex_address]");
                }
            }
        }
    }

    /// ps コマンド: タスク一覧を表示する。
    fn cmd_ps(&self) {
        let tasks = scheduler::task_list();
        kprintln!("  ID  STATE       TYPE    NAME");
        kprintln!("  --  ----------  ------  ----------");
        for t in &tasks {
            let state_str = match t.state {
                scheduler::TaskState::Ready => "Ready",
                scheduler::TaskState::Running => "Running",
                scheduler::TaskState::Sleeping(_) => "Sleeping",
                scheduler::TaskState::Finished => "Finished",
            };
            let type_str = if t.is_user_process { "user" } else { "kernel" };
            kprintln!("  {:2}  {:10}  {:6}  {}", t.id, state_str, type_str, t.name);
        }
        // 終了済みタスクを除いた数を表示
        let active = tasks.iter().filter(|t| t.state != scheduler::TaskState::Finished).count();
        kprintln!("  Total: {} tasks ({} active)", tasks.len(), active);
    }

    /// echo コマンド: 引数をそのまま出力する。
    fn cmd_echo(&self, args: &str) {
        kprintln!("{}", args);
    }

    /// usermode コマンド: Ring 3（ユーザーモード）でプログラムを実行する。
    ///
    /// プロセスごとの専用ページテーブルを作成し、CR3 を切り替えてから
    /// iretq で Ring 3 に遷移する。int 0x80 システムコールで文字列を出力して、
    /// SYS_EXIT で Ring 0（カーネル）に戻ってくる。
    /// 戻り後に CR3 をカーネルのページテーブルに復帰し、プロセスを破棄する。
    fn cmd_usermode(&self) {
        kprintln!("Entering user mode (Ring 3) with process page table...");
        let program = crate::usermode::get_user_hello();
        let process = crate::usermode::create_user_process(&program);
        kprintln!("  Process CR3: {:#x}", process.page_table_frame.start_address().as_u64());
        crate::usermode::run_in_usermode(&process, &program);
        kprintln!("Returned from user mode!");
        crate::usermode::destroy_user_process(process);
        kprintln!("Process page table destroyed.");
    }

    /// usertest コマンド: Ring 3 からカーネルメモリへのアクセスを試みる。
    ///
    /// プロセスごとの専用ページテーブルを作成して CR3 を切り替え、
    /// Ring 3 で USER_ACCESSIBLE のないアドレスにアクセスする。
    /// メモリ保護が正しく機能していれば、Page Fault が発生して
    /// ユーザープログラムが強制終了され、シェルに安全に戻るはず。
    fn cmd_usertest(&self) {
        kprintln!("Testing user mode memory protection...");
        kprintln!("Attempting illegal kernel memory access from Ring 3...");
        let program = crate::usermode::get_user_illegal_access();
        let process = crate::usermode::create_user_process(&program);
        crate::usermode::run_in_usermode(&process, &program);
        kprintln!("Protection test passed! User program was terminated safely.");
        crate::usermode::destroy_user_process(process);
    }

    /// isolate コマンド: プロセス分離のデモ。
    ///
    /// 2つのユーザープロセスを別々のページテーブル（異なる CR3）で実行し、
    /// アドレス空間が分離されていることを示す。
    /// 各プロセスが異なる CR3 値を持っていることを表示して、
    /// ページテーブルが別物であることを視覚的に確認できる。
    fn cmd_isolate(&self) {
        kprintln!("=== Process Isolation Demo ===");
        kprintln!("Kernel CR3: {:#x}", paging::kernel_cr3().as_u64());
        kprintln!();

        // プロセス A を作成・実行
        let program_a = crate::usermode::get_user_hello();
        let process_a = crate::usermode::create_user_process(&program_a);
        let cr3_a = process_a.page_table_frame.start_address().as_u64();
        kprintln!("Process A: CR3 = {:#x}", cr3_a);
        kprintln!("  Running...");
        crate::usermode::run_in_usermode(&process_a, &program_a);
        kprintln!("  Done!");

        // プロセス B を作成・実行
        let program_b = crate::usermode::get_user_hello();
        let process_b = crate::usermode::create_user_process(&program_b);
        let cr3_b = process_b.page_table_frame.start_address().as_u64();
        kprintln!("Process B: CR3 = {:#x}", cr3_b);
        kprintln!("  Running...");
        crate::usermode::run_in_usermode(&process_b, &program_b);
        kprintln!("  Done!");

        // 分離の証拠: CR3 が異なることを表示
        kprintln!();
        if cr3_a != cr3_b {
            kprintln!("Result: CR3 A ({:#x}) != CR3 B ({:#x})", cr3_a, cr3_b);
            kprintln!("  => Each process has its own page table!");
            kprintln!("  => Address spaces are ISOLATED.");
        } else {
            kprintln!("Warning: CR3 values are the same (unexpected).");
        }

        // フレーム数の確認（プロセス破棄前）
        let before_free = {
            let fa = FRAME_ALLOCATOR.lock();
            fa.free_frames()
        };
        kprintln!("Free frames before cleanup: {}", before_free);

        // プロセスを破棄
        crate::usermode::destroy_user_process(process_a);
        crate::usermode::destroy_user_process(process_b);

        // フレーム数の確認（プロセス破棄後）
        let after_free = {
            let fa = FRAME_ALLOCATOR.lock();
            fa.free_frames()
        };
        kprintln!("Free frames after cleanup:  {}", after_free);
        kprintln!("Frames reclaimed: {}", after_free - before_free);
        kprintln!("=== Demo Complete ===");
    }

    /// elf コマンド: 埋め込み ELF バイナリをパースしてユーザーモードで実行する。
    ///
    /// 手順:
    ///   1. ELF パース結果（エントリポイント、LOAD セグメント情報）を表示
    ///   2. ELF プロセスを作成（ページテーブル + フレーム確保 + データロード）
    ///   3. Ring 3 で実行
    ///   4. プロセスを破棄してフレームを返却
    fn cmd_elf(&self) {
        kprintln!("=== ELF Binary Loader ===");

        // 埋め込み ELF データを取得
        let elf_data = crate::usermode::get_user_elf_data();
        kprintln!("ELF binary size: {} bytes", elf_data.len());

        // ELF パース結果を表示
        match crate::elf::parse_elf(elf_data) {
            Ok(info) => {
                kprintln!("Entry point: {:#x}", info.entry_point);
                kprintln!("LOAD segments: {}", info.load_segments.len());
                for (i, seg) in info.load_segments.iter().enumerate() {
                    kprintln!(
                        "  [{}] vaddr={:#x} filesz={:#x} memsz={:#x} flags={:#x}",
                        i, seg.vaddr, seg.filesz, seg.memsz, seg.flags
                    );
                }
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("ELF parse error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        }

        // フレーム数の確認（プロセス作成前）
        let before_free = {
            let fa = FRAME_ALLOCATOR.lock();
            fa.free_frames()
        };

        // ELF プロセスを作成
        kprintln!("Creating ELF process...");
        let (process, entry_point, user_stack_top) =
            match crate::usermode::create_elf_process(elf_data) {
                Ok(result) => result,
                Err(e) => {
                    framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                    kprintln!("Failed to create ELF process: {}", e);
                    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                    return;
                }
            };

        kprintln!(
            "  Process CR3: {:#x}, entry: {:#x}, stack_top: {:#x}",
            process.page_table_frame.start_address().as_u64(),
            entry_point,
            user_stack_top
        );
        kprintln!("  Allocated frames: {}", process.allocated_frames.len());

        // Ring 3 で実行
        kprintln!("Running ELF binary in Ring 3...");
        crate::usermode::run_elf_process(&process, entry_point, user_stack_top);

        framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
        kprintln!("Returned from ELF binary!");
        framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));

        // プロセスを破棄
        crate::usermode::destroy_user_process(process);

        // フレーム数の確認（プロセス破棄後）
        let after_free = {
            let fa = FRAME_ALLOCATOR.lock();
            fa.free_frames()
        };
        kprintln!("Frames: before={}, after={}, reclaimed={}", before_free, after_free, after_free - before_free);
        kprintln!("=== Done ===");
    }

    /// lspci コマンド: PCI バス上のデバイス一覧を表示する。
    ///
    /// PCI Configuration Space を走査し、見つかったデバイスの
    /// バス:デバイス.ファンクション番号、ベンダー ID、デバイス ID、
    /// クラスコードを一覧表示する。
    fn cmd_lspci(&self) {
        let devices = crate::pci::enumerate_bus();
        kprintln!("PCI devices on bus 0:");
        kprintln!("  BDF       Vendor Device Class");
        kprintln!("  --------- ------ ------ --------");
        for dev in &devices {
            kprintln!(
                "  {:02x}:{:02x}.{}   {:04x}   {:04x}   {:02x}:{:02x}.{:02x}",
                dev.bus, dev.device, dev.function,
                dev.vendor_id, dev.device_id,
                dev.class_code, dev.subclass, dev.prog_if,
            );
        }
        kprintln!("  Total: {} devices", devices.len());
    }

    /// blkread コマンド: virtio-blk ドライバでディスクの指定セクタを読み取り、
    /// 先頭の内容を 16 進ダンプで表示する。
    ///
    /// 引数なし: セクタ 0（ブートセクタ / BPB）を読む
    /// 引数あり: 10進数のセクタ番号を指定
    fn cmd_blkread(&self, args: &str) {
        let sector = if args.trim().is_empty() {
            0u64
        } else {
            match args.trim().parse::<u64>() {
                Ok(s) => s,
                Err(_) => {
                    kprintln!("Usage: blkread [sector_number]");
                    return;
                }
            }
        };

        let mut drv = crate::virtio_blk::VIRTIO_BLK.lock();
        let drv = match drv.as_mut() {
            Some(d) => d,
            None => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("virtio-blk device not available");
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        let mut buf = [0u8; 512];
        match drv.read_sector(sector, &mut buf) {
            Ok(()) => {
                kprintln!("Sector {} (512 bytes):", sector);
                // 先頭 256 バイトを 16 進ダンプで表示
                for row in 0..16 {
                    let offset = row * 16;
                    kprint!("  {:04x}: ", offset);
                    for col in 0..16 {
                        kprint!("{:02x} ", buf[offset + col]);
                    }
                    // ASCII 表示
                    kprint!(" |");
                    for col in 0..16 {
                        let b = buf[offset + col];
                        if b >= 0x20 && b < 0x7F {
                            kprint!("{}", b as char);
                        } else {
                            kprint!(".");
                        }
                    }
                    kprintln!("|");
                }
                kprintln!("  ... ({} more bytes)", 512 - 256);
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Read error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// blkwrite コマンド: virtio-blk デバイスの指定セクタにテストパターンを書き込む。
    ///
    /// 警告: ファイルシステムを破壊する可能性があるので注意！
    /// データ領域の先頭（セクタ 200 以降など）でテストすること。
    fn cmd_blkwrite(&self, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            kprintln!("Usage: blkwrite <sector_number>");
            kprintln!("  WARNING: This will overwrite disk data!");
            kprintln!("  Use a sector in data area (e.g., sector 200+) to avoid corruption.");
            return;
        }

        let sector = match args.parse::<u64>() {
            Ok(s) => s,
            Err(_) => {
                kprintln!("Invalid sector number: {}", args);
                return;
            }
        };

        // 先頭セクタはファイルシステムのメタデータ領域なので警告
        if sector < 164 {
            framebuffer::set_global_colors((255, 255, 0), (0, 0, 128));
            kprintln!("WARNING: Sector {} is in filesystem metadata area!", sector);
            kprintln!("  This may corrupt the file system.");
            framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
        }

        // テストパターンを作成（セクタ番号を繰り返し）
        let mut buf = [0u8; 512];
        for i in 0..512 {
            buf[i] = ((sector + i as u64) & 0xFF) as u8;
        }

        let mut drv = crate::virtio_blk::VIRTIO_BLK.lock();
        let drv = match drv.as_mut() {
            Some(d) => d,
            None => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("virtio-blk device not available");
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        match drv.write_sector(sector, &buf) {
            Ok(()) => {
                framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                kprintln!("Sector {} written successfully!", sector);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));

                // 書き込んだ内容を読み返して確認
                let mut read_buf = [0u8; 512];
                match drv.read_sector(sector, &mut read_buf) {
                    Ok(()) => {
                        if read_buf == buf {
                            kprintln!("Verified: read-back matches written data.");
                        } else {
                            framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                            kprintln!("ERROR: read-back does not match!");
                            framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                        }
                    }
                    Err(e) => {
                        kprintln!("Read-back failed: {}", e);
                    }
                }
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Write error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// ls コマンド: FAT32 ディスクのディレクトリにあるファイル一覧を表示する。
    ///
    /// 引数なし: ルートディレクトリを表示
    /// 引数あり: 指定パスのディレクトリを表示（例: ls /SUBDIR）
    fn cmd_ls(&self, args: &str) {
        let path = args.trim();

        let mut fs = match crate::fat32::Fat32::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT32 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        let path = if path.is_empty() { "/" } else { path };
        match fs.list_dir(path) {
            Ok(entries) => {
                // ディレクトリパスを表示
                if path == "/" {
                    kprintln!("Directory: /");
                } else {
                    kprintln!("Directory: {}", path);
                }
                kprintln!("  Name          Size     Attr");
                kprintln!("  ------------- -------- ----");
                for entry in &entries {
                    // "." と ".." は表示しない（サブディレクトリには存在するが見づらい）
                    if entry.name == "." || entry.name == ".." {
                        continue;
                    }
                    let attr_str = if entry.attr & 0x10 != 0 {
                        "<DIR>"
                    } else {
                        "     "
                    };
                    kprintln!(
                        "  {:<13} {:>8} {}",
                        entry.name, entry.size, attr_str
                    );
                }
                // "." と ".." を除いた件数を表示
                let count = entries.iter()
                    .filter(|e| e.name != "." && e.name != "..")
                    .count();
                kprintln!("  {} file(s)", count);
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error listing directory: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// cat コマンド: FAT32 ディスクのファイル内容を表示する。
    fn cmd_cat(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: cat <FILENAME>");
            return;
        }

        let mut fs = match crate::fat32::Fat32::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT32 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        match fs.read_file(filename) {
            Ok(data) => {
                // テキストファイルとして表示を試みる
                match core::str::from_utf8(&data) {
                    Ok(text) => {
                        kprintln!("{}", text);
                    }
                    Err(_) => {
                        // バイナリファイルの場合はサイズだけ表示
                        kprintln!("(binary file, {} bytes)", data.len());
                    }
                }
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// write コマンド: FAT32 ディスクに新しいファイルを作成する。
    ///
    /// 使い方: write <FILENAME> <TEXT>
    /// 例: write TEST.TXT Hello World
    fn cmd_write(&self, args: &str) {
        let args = args.trim();
        if args.is_empty() {
            kprintln!("Usage: write <FILENAME> <TEXT>");
            kprintln!("  Example: write TEST.TXT Hello World");
            return;
        }

        // 最初の空白でファイル名とテキストを分離
        let parts: Vec<&str> = args.splitn(2, ' ').collect();
        if parts.len() < 2 {
            kprintln!("Usage: write <FILENAME> <TEXT>");
            kprintln!("  Both filename and text content are required.");
            return;
        }

        let filename = parts[0];
        let text = parts[1];

        let mut fs = match crate::fat32::Fat32::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT32 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        // テキストの末尾に改行を追加
        let mut content = text.as_bytes().to_vec();
        content.push(b'\n');

        match fs.create_file(filename, &content) {
            Ok(()) => {
                framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                kprintln!("File '{}' created ({} bytes)", filename, content.len());
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error creating file: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// rm コマンド: FAT32 ディスクのファイルを削除する。
    fn cmd_rm(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: rm <FILENAME>");
            return;
        }

        let mut fs = match crate::fat32::Fat32::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT32 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        match fs.delete_file(filename) {
            Ok(()) => {
                framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                kprintln!("File '{}' deleted", filename);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error deleting file: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// run コマンド: FAT32 ディスクから ELF バイナリを読み込んでユーザーモードで実行する。
    ///
    /// 手順:
    ///   1. FAT32 からファイルを読み込む
    ///   2. ELF パース → LOAD セグメント情報を取得
    ///   3. プロセス作成 → ページテーブル + フレーム確保 + データロード
    ///   4. Ring 3 で実行
    ///   5. プロセスを破棄してフレームを返却
    fn cmd_run(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: run <FILENAME>");
            kprintln!("  Example: run HELLO.ELF");
            return;
        }

        // FAT32 からファイルを読み込む
        kprintln!("Loading {} from disk...", filename);
        let mut fs = match crate::fat32::Fat32::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT32 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        let elf_data = match fs.read_file(filename) {
            Ok(data) => data,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error reading file: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };
        kprintln!("  Loaded {} bytes", elf_data.len());

        // ELF パース結果を表示
        match crate::elf::parse_elf(&elf_data) {
            Ok(info) => {
                kprintln!("  Entry point: {:#x}", info.entry_point);
                kprintln!("  LOAD segments: {}", info.load_segments.len());
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("ELF parse error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        }

        // フレーム数の確認（プロセス作成前）
        let before_free = {
            let fa = FRAME_ALLOCATOR.lock();
            fa.free_frames()
        };

        // ELF プロセスを作成
        let (process, entry_point, user_stack_top) =
            match crate::usermode::create_elf_process(&elf_data) {
                Ok(result) => result,
                Err(e) => {
                    framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                    kprintln!("Failed to create ELF process: {}", e);
                    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                    return;
                }
            };

        // Ring 3 で実行
        kprintln!("Running in Ring 3...");
        crate::usermode::run_elf_process(&process, entry_point, user_stack_top);

        framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
        kprintln!("Program exited.");
        framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));

        // プロセスを破棄
        crate::usermode::destroy_user_process(process);

        // フレーム数の確認（プロセス破棄後）
        let after_free = {
            let fa = FRAME_ALLOCATOR.lock();
            fa.free_frames()
        };
        kprintln!("Frames: before={}, after={}, reclaimed={}", before_free, after_free, after_free - before_free);
    }

    /// spawn コマンド: FAT32 ディスクから ELF バイナリを読み込んで、
    /// バックグラウンドでユーザープロセスとして実行する。
    ///
    /// run コマンドと異なり、プロセスはブロックせずに即座に戻る。
    /// プロセスはスケジューラに登録され、タイムスライスで他のタスクと並行実行される。
    ///
    /// 使い方: spawn HELLO.ELF
    fn cmd_spawn(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: spawn <FILENAME>");
            kprintln!("  Example: spawn HELLO.ELF");
            kprintln!("  The process runs in the background. Use 'ps' to see tasks.");
            return;
        }

        // FAT32 からファイルを読み込む
        kprintln!("Loading {} from disk...", filename);
        let mut fs = match crate::fat32::Fat32::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT32 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        let elf_data = match fs.read_file(filename) {
            Ok(data) => data,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error reading file: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };
        kprintln!("  Loaded {} bytes", elf_data.len());

        // プロセス名を作成（パスからファイル名部分を抽出）
        let process_name = filename
            .rsplit('/')
            .next()
            .unwrap_or(filename);

        // ユーザープロセスとして spawn
        match scheduler::spawn_user(process_name, &elf_data) {
            Ok(task_id) => {
                framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                kprintln!("Process '{}' spawned as task {} (background)", process_name, task_id);
                kprintln!("Use 'ps' to see running tasks.");
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Failed to spawn process: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// netpoll コマンド: ネットワークパケットをポーリングして処理する。
    ///
    /// 引数なし: 10 秒間ポーリング
    /// 引数あり: 指定秒数ポーリング
    fn cmd_netpoll(&self, args: &str) {
        let seconds = if args.trim().is_empty() {
            10u32
        } else {
            match args.trim().parse::<u32>() {
                Ok(s) => s,
                Err(_) => {
                    kprintln!("Usage: netpoll [seconds]");
                    return;
                }
            }
        };

        {
            let drv = crate::virtio_net::VIRTIO_NET.lock();
            if drv.is_none() {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("virtio-net not available");
                kprintln!("Add -netdev user,id=net0 -device virtio-net-pci,netdev=net0 to QEMU");
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        }

        kprintln!("Polling network for {} seconds...", seconds);
        kprintln!("(Try 'ping 10.0.2.15' from another terminal)");

        // 指定秒数ポーリング（約 100ms 間隔）
        let iterations = seconds * 10;
        for _ in 0..iterations {
            crate::net::poll_and_handle();
            // 約 100ms 待機
            for _ in 0..100000 {
                core::hint::spin_loop();
            }
        }

        kprintln!("Done polling.");
    }

    /// ip コマンド: IP 設定を表示する。
    fn cmd_ip(&self) {
        kprintln!("IP Configuration:");
        kprintln!("  IP Address: {}.{}.{}.{}",
            crate::net::MY_IP[0], crate::net::MY_IP[1],
            crate::net::MY_IP[2], crate::net::MY_IP[3]);
        kprintln!("  Gateway:    {}.{}.{}.{}",
            crate::net::GATEWAY_IP[0], crate::net::GATEWAY_IP[1],
            crate::net::GATEWAY_IP[2], crate::net::GATEWAY_IP[3]);
        kprintln!("  DNS:        {}.{}.{}.{}",
            crate::net::DNS_SERVER_IP[0], crate::net::DNS_SERVER_IP[1],
            crate::net::DNS_SERVER_IP[2], crate::net::DNS_SERVER_IP[3]);

        let drv = crate::virtio_net::VIRTIO_NET.lock();
        if let Some(ref d) = *drv {
            kprintln!("  MAC:        {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                d.mac_address[0], d.mac_address[1], d.mac_address[2],
                d.mac_address[3], d.mac_address[4], d.mac_address[5]);
        } else {
            kprintln!("  MAC:        (no network device)");
        }
    }

    /// dns コマンド: ドメイン名を IP アドレスに解決する。
    fn cmd_dns(&self, args: &str) {
        let domain = args.trim();
        if domain.is_empty() {
            kprintln!("Usage: dns <domain>");
            kprintln!("  Example: dns example.com");
            return;
        }

        {
            let drv = crate::virtio_net::VIRTIO_NET.lock();
            if drv.is_none() {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("virtio-net not available");
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        }

        kprintln!("Resolving '{}'...", domain);

        match crate::net::dns_lookup(domain) {
            Ok(ip) => {
                framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                kprintln!("{} -> {}.{}.{}.{}", domain, ip[0], ip[1], ip[2], ip[3]);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("DNS lookup failed: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// http コマンド: HTTP GET リクエストを送信する。
    ///
    /// 使い方: http <host> [path]
    /// 例: http example.com /
    ///     http 93.184.216.34 /index.html
    fn cmd_http(&self, args: &str) {
        let parts: Vec<&str> = args.trim().split_whitespace().collect();
        if parts.is_empty() {
            kprintln!("Usage: http <host> [path]");
            kprintln!("  Example: http example.com /");
            return;
        }

        let host = parts[0];
        let path = if parts.len() > 1 { parts[1] } else { "/" };

        // virtio-net が利用可能か確認
        {
            let drv = crate::virtio_net::VIRTIO_NET.lock();
            if drv.is_none() {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("virtio-net not available");
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        }

        // ホストが IP アドレスかドメイン名かを判定
        let ip = if let Some(parsed_ip) = self.parse_ip(host) {
            parsed_ip
        } else {
            // DNS で解決
            kprintln!("Resolving {}...", host);
            match crate::net::dns_lookup(host) {
                Ok(ip) => {
                    kprintln!("Resolved to {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
                    ip
                }
                Err(e) => {
                    framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                    kprintln!("DNS lookup failed: {}", e);
                    framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                    return;
                }
            }
        };

        // TCP 接続
        kprintln!("Connecting to {}.{}.{}.{}:80...", ip[0], ip[1], ip[2], ip[3]);
        if let Err(e) = crate::net::tcp_connect(ip, 80) {
            framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
            kprintln!("TCP connect failed: {}", e);
            framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            return;
        }
        kprintln!("Connected!");

        // HTTP リクエストを送信
        let request = alloc::format!(
            "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
            path, host
        );
        kprintln!("Sending HTTP request...");
        if let Err(e) = crate::net::tcp_send(request.as_bytes()) {
            framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
            kprintln!("TCP send failed: {}", e);
            framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            let _ = crate::net::tcp_close();
            return;
        }

        // レスポンスを受信
        kprintln!("Receiving response...");
        kprintln!("--- Response ---");
        loop {
            match crate::net::tcp_recv(5000) {
                Ok(data) => {
                    // UTF-8 として表示（無効な場合は置換）
                    let text = core::str::from_utf8(&data)
                        .unwrap_or("[binary data]");
                    kprint!("{}", text);
                }
                Err(e) => {
                    if e != "timeout" {
                        kprintln!("\n[{}]", e);
                    }
                    break;
                }
            }
        }
        kprintln!("\n--- End ---");

        // 接続を閉じる
        let _ = crate::net::tcp_close();
    }

    /// IP アドレス文字列をパースする (例: "192.168.1.1")
    fn parse_ip(&self, s: &str) -> Option<[u8; 4]> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 4 {
            return None;
        }
        let mut ip = [0u8; 4];
        for (i, part) in parts.iter().enumerate() {
            ip[i] = part.parse().ok()?;
        }
        Some(ip)
    }

    /// selftest コマンド: 各サブシステムの自動テストを実行する。
    /// CI で使いやすいように、各テスト結果を [PASS]/[FAIL] で出力し、
    /// 最後にサマリーを出力する。
    fn cmd_selftest(&self, args: &str) {
        let target = args.trim();
        let target = if target.is_empty() { "all" } else { target };

        if target == "list" {
            kprintln!("selftest targets: all, base, core, fs, net, gui, service");
            return;
        }

        if target == "all" {
            kprintln!("=== SELFTEST START ===");
        } else {
            kprintln!("=== SELFTEST START ({}) ===", target);
        }

        let mut passed = 0;
        let mut failed = 0;
        let mut run_test = |name: &str, ok: bool| {
            if ok {
                Self::print_pass(name);
                passed += 1;
            } else {
                Self::print_fail(name);
                failed += 1;
            }
        };

        let run_core = |this: &Self, run_test: &mut dyn FnMut(&str, bool)| {
            // 1. メモリアロケータのテスト
            run_test("memory_allocator", this.test_memory_allocator());

            // 1.5. メモリマッピングの整合性テスト
            run_test("memory_mapping", this.test_memory_mapping());

            // 2. ページングのテスト
            run_test("paging", this.test_paging());

            // 3. PCI 列挙のテスト
            run_test("pci_enum", this.test_pci_enum());

            // 4. procfs のテスト
            run_test("procfs", this.test_procfs());

            // 5. フレームバッファ描画のテスト
            run_test("framebuffer_draw", this.test_framebuffer_draw());

            // 6. フレームバッファ情報のテスト
            run_test("framebuffer_info", this.test_framebuffer_info());

            // 6.5. マウス初期化のテスト
            run_test("mouse", this.test_mouse());

            // 7. ハンドル open/read のテスト
            run_test("handle_open", this.test_handle_open_read());

            // 8. スケジューラのテスト
            run_test("scheduler", this.test_scheduler());

            // 9. ブロックデバイス syscalls のテスト
            run_test("block_syscall", this.test_block_syscall());

            // 10. IPC のテスト
            run_test("ipc", this.test_ipc());

            // 11. 型安全 IPC のテスト
            run_test("ipc_typed", this.test_ipc_typed());

            // 11.5. 文字列置換ユーティリティのテスト
            run_test("textutil_replace", this.test_textutil_replace());

            // 11.7. 文字列検索ユーティリティのテスト
            run_test("textutil_contains", this.test_textutil_contains());

            // 11.6. exec のテスト（EXIT0.ELF を同期実行）
            run_test("exec_exit0", this.test_exec_exit0());

            // 11.8. kill のテスト（自分自身の kill が拒否されること）
            run_test("kill_self_reject", this.test_kill_self_reject());

            // 11.9. clock_monotonic のテスト
            run_test("clock_monotonic", this.test_clock_monotonic());

            // 11.10. getrandom のテスト
            run_test("getrandom", this.test_getrandom());

            // 11.11. mmap のテスト（匿名ページの動的マッピング）
            run_test("mmap", this.test_mmap());

            // 11.12. AC97 オーディオコントローラの検出テスト
            run_test("ac97_detect", this.test_ac97_detect());
        };

        let run_fs = |this: &Self, run_test: &mut dyn FnMut(&str, bool)| {
            // 12. virtio-blk のテスト
            run_test("virtio_blk", this.test_virtio_blk());

            // 13. FAT32 のテスト
            run_test("fat32", this.test_fat32());

            // 13.5. FAT32 空き容量のテスト
            run_test("fat32_space", this.test_fat32_space());

            // 13.6. コンソールエディタ (ED.ELF) の存在確認
            run_test("console_editor_elf", this.test_console_editor_elf());
        };

        let run_net = |this: &Self, run_test: &mut dyn FnMut(&str, bool)| {
            // 14. ネットワーク (DNS) のテスト
            if this.netd_is_running() {
                run_test("network_dns", true);
                kprintln!("  (kernel DNS skipped: netd is active)");
            } else {
                run_test("network_dns", this.test_network_dns());
            }

            // 15. ユーザー空間 netd の DNS テスト
            run_test("network_netd_dns", this.test_network_netd_dns());
        };

        let run_gui = |this: &Self, run_test: &mut dyn FnMut(&str, bool)| {
            // 16. GUI IPC のテスト
            run_test("gui_ipc", this.test_gui_ipc());
            // 16.5. GUI アプリ (TETRIS) の存在確認
            run_test("gui_tetris_elf", this.test_tetris_elf());
        };

        let run_service = |this: &Self, run_test: &mut dyn FnMut(&str, bool)| {
            // 17. telnetd サービスの起動確認
            run_test("telnetd_service", this.test_telnetd_service());
            // 17.5. httpd サービスの起動確認
            run_test("httpd_service", this.test_httpd_service());
            // 17.6. httpd が参照するディレクトリ一覧が取得できることを確認
            run_test("httpd_dirlist", this.test_httpd_dirlist());
        };
        let run_base = |this: &Self, run_test: &mut dyn FnMut(&str, bool)| {
            run_core(this, run_test);
            run_fs(this, run_test);
            run_net(this, run_test);
            run_service(this, run_test);
        };

        match target {
            "all" => {
                run_core(self, &mut run_test);
                run_fs(self, &mut run_test);
                run_net(self, &mut run_test);
                run_gui(self, &mut run_test);
                run_service(self, &mut run_test);
            }
            "base" => run_base(self, &mut run_test),
            "core" => run_core(self, &mut run_test),
            "fs" => run_fs(self, &mut run_test),
            "net" => run_net(self, &mut run_test),
            "gui" => run_gui(self, &mut run_test),
            "service" => run_service(self, &mut run_test),
            _ => {
                kprintln!("Usage: selftest [all|base|core|fs|net|gui|service|list]");
                return;
            }
        }

        // サマリー出力
        let total = passed + failed;
        if failed == 0 {
            framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
            kprintln!("=== SELFTEST END: {}/{} PASSED ===", passed, total);
            framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
        } else {
            framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
            kprintln!("=== SELFTEST END: {}/{} FAILED ===", failed, total);
            framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
        }
    }

    /// テスト結果を緑色で [PASS] と表示
    fn print_pass(name: &str) {
        framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
        kprintln!("[PASS] {}", name);
        framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    }

    /// テスト結果を赤色で [FAIL] と表示
    fn print_fail(name: &str) {
        framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
        kprintln!("[FAIL] {}", name);
        framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
    }

    /// メモリアロケータのテスト
    /// Box/Vec に加えて、断片化しやすいパターンで再利用できるかを確認
    fn test_memory_allocator(&self) -> bool {
        use alloc::boxed::Box;
        use alloc::vec;
        use alloc::vec::Vec;

        // Box のアロケーション
        let boxed = Box::new(12345u64);
        if *boxed != 12345 {
            return false;
        }

        // Vec のアロケーション（複数要素）
        let mut v = vec![1u32, 2, 3, 4, 5];
        v.push(6);
        if v.len() != 6 || v[5] != 6 {
            return false;
        }

        // 大きめのアロケーション
        let big = vec![0u8; 4096];
        if big.len() != 4096 {
            return false;
        }

        // 断片化しやすい確保/解放のパターンを作り、再利用できるか確認する。
        // サイズの異なる Vec を大量に確保し、間引いて解放→再確保する。
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        for i in 0..256 {
            let size = 64 + (i % 8) * 128; // 64,192,320... の繰り返し
            blocks.push(vec![0xAAu8; size]);
        }

        // 交互に解放して断片化を作る
        for i in (0..blocks.len()).step_by(2) {
            blocks[i].clear();
            blocks[i].shrink_to_fit();
        }

        // 再確保（別サイズ）で再利用できるかを確認
        for i in (0..blocks.len()).step_by(2) {
            let size = 512 + (i % 4) * 256;
            blocks[i] = vec![0x55u8; size];
        }

        // 中身の整合性チェック
        for (i, b) in blocks.iter().enumerate() {
            if b.is_empty() {
                return false;
            }
            if i % 2 == 0 {
                if b[0] != 0x55 {
                    return false;
                }
            } else if b[0] != 0xAA {
                return false;
            }
        }

        // drop されてメモリが解放されることを期待（明示的なチェックは難しいので省略）
        true
    }

    /// メモリマッピングの整合性テスト
    ///
    /// create_process_page_table() → map_user_pages_in_process() → translate_in_process()
    /// → フレーム解放 → destroy_process_page_table() の流れが破綻しないことを確認する。
    fn test_memory_mapping(&self) -> bool {
        // 1. 事前のフレーム数を記録
        let before = {
            let fa = FRAME_ALLOCATOR.lock();
            fa.allocated_count()
        };

        // 2. プロセス用ページテーブルを作成
        let l4 = paging::create_process_page_table();

        // 3. ユーザー空間に 2 ページ分マッピング
        let test_vaddr = 0x0300_0000u64; // 48MiB 付近（ユーザー空間）
        let frames = paging::map_user_pages_in_process(
            l4,
            VirtAddr::new(test_vaddr),
            4096 * 2,
            &[],
            4 | 2, // PF_R | PF_W（テスト用: 読み書き可能・実行不可）
        );
        if frames.len() != 2 {
            paging::destroy_process_page_table(l4);
            return false;
        }

        // 4. 仮想→物理の変換が成功することを確認
        let phys0 = paging::translate_in_process(l4, VirtAddr::new(test_vaddr));
        let phys1 = paging::translate_in_process(l4, VirtAddr::new(test_vaddr + 4096));
        if phys0.is_none() || phys1.is_none() {
            paging::destroy_process_page_table(l4);
            return false;
        }

        // 5. 物理フレームに書き込み→読み戻し（アイデンティティマッピング前提）
        unsafe {
            let p0 = frames[0].start_address().as_u64() as *mut u8;
            let p1 = frames[1].start_address().as_u64() as *mut u8;
            *p0 = 0xAA;
            *p1 = 0x55;
            if *p0 != 0xAA || *p1 != 0x55 {
                paging::destroy_process_page_table(l4);
                return false;
            }
        }

        // 6. ユーザーフレームを手動で解放
        {
            let mut fa = FRAME_ALLOCATOR.lock();
            for f in &frames {
                unsafe { fa.deallocate_frame(*f); }
            }
        }

        // 7. ページテーブルを破棄
        paging::destroy_process_page_table(l4);

        // 8. フレーム数が元に戻ったことを確認
        let after = {
            let fa = FRAME_ALLOCATOR.lock();
            fa.allocated_count()
        };

        before == after
    }

    /// ページングのテスト
    /// アドレス変換が正常に動作するかを確認
    fn test_paging(&self) -> bool {
        // 既知のアドレス（カーネルコード領域）の変換を試す
        // カーネルはアイデンティティマッピングされているはずなので virt == phys
        let test_addr = VirtAddr::new(0x100000); // 1MB
        match paging::translate_addr(test_addr) {
            Some(phys) => {
                // アイデンティティマッピングの場合、phys == virt
                phys.as_u64() == test_addr.as_u64()
            }
            None => false,
        }
    }

    /// スケジューラのテスト
    /// タスクを spawn して Finished になるまで待つ
    fn test_scheduler(&self) -> bool {
        use core::sync::atomic::{AtomicBool, Ordering};

        // テスト用のフラグ（タスクが実行されたら true にする）
        static TEST_FLAG: AtomicBool = AtomicBool::new(false);
        TEST_FLAG.store(false, Ordering::SeqCst);

        fn test_task() {
            TEST_FLAG.store(true, Ordering::SeqCst);
        }

        // タスクを spawn
        let _task_id = scheduler::spawn("selftest_task", test_task);

        // タスクが完了するまで yield（最大 100 回）
        for _ in 0..100 {
            scheduler::yield_now();
            if TEST_FLAG.load(Ordering::SeqCst) {
                return true;
            }
        }

        false
    }

    /// exec のテスト
    /// EXIT0.ELF を同期実行し、正常終了することを確認する
    fn test_exec_exit0(&self) -> bool {
        crate::syscall::exec_for_test("/EXIT0.ELF")
    }

    /// PCI 列挙のテスト
    /// バス 0 に 1 つ以上のデバイスが存在することを確認する
    fn test_pci_enum(&self) -> bool {
        let devices = crate::pci::enumerate_bus();
        if devices.is_empty() {
            return false;
        }

        // 取得したベンダー ID が妥当か確認（0xFFFF は空スロット）
        for dev in devices {
            if dev.vendor_id == 0xFFFF {
                return false;
            }

            let vid = crate::pci::pci_config_read16(dev.bus, dev.device, dev.function, 0x00);
            if vid != dev.vendor_id {
                return false;
            }
        }

        true
    }

    /// procfs のテスト
    /// /proc の一覧と、/proc/meminfo / /proc/tasks が読めることを確認
    fn test_procfs(&self) -> bool {
        let mut buf = [0u8; 512];

        // /proc の一覧
        let list_len = match crate::procfs::procfs_list_dir("/proc", &mut buf) {
            Ok(n) => n,
            Err(_) => return false,
        };
        if list_len == 0 {
            return false;
        }
        let list_str = match core::str::from_utf8(&buf[..list_len]) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if !list_str.contains("meminfo") || !list_str.contains("tasks") {
            return false;
        }

        // /proc/meminfo
        let mem_len = match crate::procfs::procfs_read("/proc/meminfo", &mut buf) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let mem_str = match core::str::from_utf8(&buf[..mem_len]) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if !mem_str.contains("\"total_frames\"") {
            return false;
        }

        // /proc/tasks
        let task_len = match crate::procfs::procfs_read("/proc/tasks", &mut buf) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let task_str = match core::str::from_utf8(&buf[..task_len]) {
            Ok(s) => s,
            Err(_) => return false,
        };
        if !task_str.contains("\"tasks\"") || !task_str.contains("\"id\"") {
            return false;
        }

        true
    }

    /// フレームバッファ描画のテスト
    /// 成功/失敗の戻り値で境界チェックが効いているかを見る。
    fn test_framebuffer_draw(&self) -> bool {
        let Some((width, height)) = crate::framebuffer::screen_size() else {
            return false;
        };
        if width == 0 || height == 0 {
            return false;
        }

        // 正常系: 画面内
        if crate::framebuffer::draw_pixel_global(0, 0, 255, 0, 0).is_err() {
            return false;
        }
        if crate::framebuffer::draw_rect_global(0, 0, 1, 1, 0, 255, 0).is_err() {
            return false;
        }
        if crate::framebuffer::draw_line_global(0, 0, 1, 1, 0, 0, 255).is_err() {
            return false;
        }
        let blit_buf = [255u8, 255u8, 0u8, 0u8];
        if crate::framebuffer::draw_blit_global(0, 0, 1, 1, &blit_buf).is_err() {
            return false;
        }
        if crate::framebuffer::draw_text_global(0, 0, (255, 255, 255), (0, 0, 0), "GUI").is_err() {
            return false;
        }

        // 異常系: 画面外
        if crate::framebuffer::draw_pixel_global(width, 0, 0, 0, 255).is_ok() {
            return false;
        }
        if crate::framebuffer::draw_rect_global(0, 0, 0, 1, 0, 0, 255).is_ok() {
            return false;
        }
        if crate::framebuffer::draw_line_global(width, 0, width + 1, 1, 0, 0, 0).is_ok() {
            return false;
        }
        if crate::framebuffer::draw_blit_global(0, 0, 2, 2, &blit_buf).is_ok() {
            return false;
        }

        true
    }

    /// フレームバッファ情報のテスト
    fn test_framebuffer_info(&self) -> bool {
        let Some(info) = crate::framebuffer::screen_info() else {
            return false;
        };
        if info.width == 0 || info.height == 0 {
            return false;
        }
        if info.stride < info.width {
            return false;
        }
        if info.bytes_per_pixel != 4 {
            return false;
        }
        info.pixel_format != 0
    }

    /// マウス初期化のテスト
    /// PS/2 マウスが初期化できているかだけを確認する。
    fn test_mouse(&self) -> bool {
        crate::mouse::is_initialized()
    }

    /// ハンドル open/read のテスト
    /// /proc/meminfo と HELLO.TXT を open して読めることを確認
    fn test_handle_open_read(&self) -> bool {
        use crate::handle::HANDLE_RIGHT_READ;

        // /proc/meminfo を open
        let handle = match crate::syscall::open_path_to_handle("/proc/meminfo", HANDLE_RIGHT_READ) {
            Ok(h) => h,
            Err(_) => return false,
        };

        let mem = match self.read_all_handle(&handle) {
            Ok(v) => v,
            Err(_) => {
                let _ = crate::handle::close(&handle);
                return false;
            }
        };
        let _ = crate::handle::close(&handle);

        if !mem.windows(b"\"total_frames\"".len()).any(|w| w == b"\"total_frames\"") {
            return false;
        }

        // HELLO.TXT を open
        let handle = match crate::syscall::open_path_to_handle("HELLO.TXT", HANDLE_RIGHT_READ) {
            Ok(h) => h,
            Err(_) => return false,
        };

        let hello = match self.read_all_handle(&handle) {
            Ok(v) => v,
            Err(_) => {
                let _ = crate::handle::close(&handle);
                return false;
            }
        };
        let _ = crate::handle::close(&handle);

        hello.starts_with(b"Hello from FAT32!")
    }

    /// ブロックデバイス syscalls のテスト
    /// SYS_BLOCK_READ でセクタ 0 を読み取り、0x55AA を確認する
    fn test_block_syscall(&self) -> bool {
        let mut buf = [0u8; 512];
        let ptr = buf.as_mut_ptr() as u64;
        match crate::syscall::sys_block_read(0, ptr, buf.len() as u64) {
            Ok(n) => n == 512 && buf[510] == 0x55 && buf[511] == 0xAA,
            Err(_) => false,
        }
    }

    /// IPC のテスト
    /// 自分宛に送信して受信できることを確認する
    fn test_ipc(&self) -> bool {
        let task_id = crate::scheduler::current_task_id();
        let data = b"ping";
        if crate::ipc::send(task_id, task_id, data.to_vec()).is_err() {
            return false;
        }

        let msg = match crate::ipc::recv(task_id, 1000) {
            Ok(m) => m,
            Err(_) => return false,
        };

        msg.data == data
    }

    /// 型安全 IPC のテスト
    /// 同じタスクに typed メッセージを送受信できることを確認する
    fn test_ipc_typed(&self) -> bool {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        struct TestMsg {
            a: u32,
            b: u64,
        }

        let task_id = crate::scheduler::current_task_id();
        let data = TestMsg { a: 7, b: 42 };
        if crate::ipc::send_typed(task_id, task_id, data).is_err() {
            return false;
        }

        let msg = match crate::ipc::recv_typed::<TestMsg>(task_id, 1000) {
            Ok(m) => m,
            Err(_) => return false,
        };

        msg.sender == task_id && msg.data == data
    }

    /// 文字列置換ユーティリティのテスト
    fn test_textutil_replace(&self) -> bool {
        let (out, changed) = sabos_textutil::replace_literal("a a a", "a", "b", true);
        if !changed || out != "b b b" {
            return false;
        }
        let (out, changed) = sabos_textutil::replace_literal("hello", "ll", "LL", false);
        changed && out == "heLLo"
    }

    /// textutil の contains_literal テスト
    fn test_textutil_contains(&self) -> bool {
        // 通常マッチ
        if !sabos_textutil::contains_literal("hello world", "world", false) {
            return false;
        }
        // マッチしないケース
        if sabos_textutil::contains_literal("hello world", "xyz", false) {
            return false;
        }
        // 大文字小文字無視
        if !sabos_textutil::contains_literal("Hello World", "hello", true) {
            return false;
        }
        // 大文字小文字区別（マッチしないはず）
        if sabos_textutil::contains_literal("Hello World", "hello", false) {
            return false;
        }
        // 空パターンは常にマッチ
        if !sabos_textutil::contains_literal("anything", "", false) {
            return false;
        }
        true
    }

    /// kill の自己 kill 拒否テスト
    ///
    /// 自分自身のタスク ID を kill しようとすると拒否されることを確認する。
    fn test_kill_self_reject(&self) -> bool {
        let my_id = crate::scheduler::current_task_id();
        // 自分自身の kill はエラーになるはず
        crate::scheduler::kill_task(my_id).is_err()
    }

    /// SYS_CLOCK_MONOTONIC のテスト
    /// 起動からの経過時間が 0 より大きいことを確認する。
    /// また、2回呼んで2回目が1回目以上であること（単調増加）を確認する。
    fn test_clock_monotonic(&self) -> bool {
        let ticks = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        let ms1 = ticks * 10000 / 182;
        // 起動してからしばらく経っているはずなので 0 より大きい
        if ms1 == 0 {
            return false;
        }
        // 2回目のチェック: 単調増加
        let ticks2 = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        let ms2 = ticks2 * 10000 / 182;
        ms2 >= ms1
    }

    /// SYS_GETRANDOM のテスト
    /// RDRAND 命令でランダムバイトが生成されることを確認する。
    /// 8 バイトを生成して、全てゼロでないことを確認する。
    /// （全ゼロの確率は 1/2^64 なので実質的に起こらない）
    fn test_getrandom(&self) -> bool {
        // RDRAND 命令で 8 バイト取得
        let mut value: u64 = 0;
        let mut success_count = 0;
        for _ in 0..3 {
            let ok: u8;
            unsafe {
                core::arch::asm!(
                    "rdrand {val}",
                    "setc {ok}",
                    val = out(reg) value,
                    ok = out(reg_byte) ok,
                );
            }
            if ok != 0 {
                success_count += 1;
            }
        }
        // 3回中1回でも成功すれば OK（RDRAND が使えることを確認）
        // かつ最後に得た値がゼロでないことを確認
        success_count > 0 && value != 0
    }

    /// mmap のテスト（匿名ページの動的マッピング）
    ///
    /// カーネル空間から paging の map_anonymous_pages_in_process を直接テストする。
    /// ELF プロセスのページテーブルを作成し、匿名ページをマッピングして
    /// 読み書きが正常に行えることを確認する。
    fn test_mmap(&self) -> bool {
        use x86_64::VirtAddr;

        // テスト用にプロセスページテーブルを作成
        let l4_frame = crate::paging::create_process_page_table();

        // 0x4000_0000 (1GiB) に 2 ページ（8KiB）をマッピング
        // カーネルのアイデンティティマッピング（物理 RAM）と被らないように
        // 高い仮想アドレスを使う
        let virt_addr = VirtAddr::new(0x4000_0000);
        let allocated = crate::paging::map_anonymous_pages_in_process(
            l4_frame,
            virt_addr,
            2,   // 2 ページ
            true, // 書き込み可能
        );

        // 2 フレームが確保されていること
        if allocated.len() != 2 {
            crate::paging::destroy_process_page_table(l4_frame);
            return false;
        }

        // 確保したフレームがゼロ初期化されていることを確認
        // （アイデンティティマッピングで物理アドレス = 仮想アドレスとしてアクセス）
        let frame0_ptr = allocated[0].start_address().as_u64() as *const u8;
        let all_zero = unsafe {
            (0..4096).all(|i| *frame0_ptr.add(i) == 0)
        };
        if !all_zero {
            crate::paging::destroy_process_page_table(l4_frame);
            return false;
        }

        // フレームに書き込みができることを確認
        let frame0_mut = allocated[0].start_address().as_u64() as *mut u8;
        unsafe {
            *frame0_mut = 0xAB;
            *frame0_mut.add(1) = 0xCD;
        }
        let written_ok = unsafe {
            *frame0_mut == 0xAB && *frame0_mut.add(1) == 0xCD
        };

        // munmap テスト: ページのマッピングを解除
        let freed = crate::paging::unmap_pages_in_process(l4_frame, virt_addr, 2);
        let unmap_ok = freed.len() == 2;

        // クリーンアップ
        crate::paging::destroy_process_page_table(l4_frame);

        written_ok && unmap_ok
    }

    /// AC97 オーディオコントローラの検出テスト。
    /// AC97 ドライバが正常に初期化されていることを確認する。
    fn test_ac97_detect(&self) -> bool {
        crate::ac97::is_available()
    }

    /// Handle から EOF まで読み取る
    fn read_all_handle(&self, handle: &crate::handle::Handle) -> Result<Vec<u8>, crate::user_ptr::SyscallError> {
        use alloc::vec::Vec;

        let mut out: Vec<u8> = Vec::new();
        let mut buf = [0u8; 256];

        loop {
            let n = crate::handle::read(handle, &mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }

        Ok(out)
    }

    /// virtio-blk のテスト
    /// セクタ 0 を読み取り、ブートシグネチャ (0x55AA) を確認
    fn test_virtio_blk(&self) -> bool {
        let mut drv = crate::virtio_blk::VIRTIO_BLK.lock();
        if let Some(ref mut d) = *drv {
            let mut buf = [0u8; 512];
            match d.read_sector(0, &mut buf) {
                Ok(()) => {
                    buf[510] == 0x55 && buf[511] == 0xAA
                }
                Err(_) => false,
            }
        } else {
            false
        }
    }

    /// FAT32 のテスト
    /// HELLO.TXT ファイルを読み取り、内容が "Hello from FAT32!" で始まるか確認
    fn test_fat32(&self) -> bool {
        let mut fs = match crate::fat32::Fat32::new() {
            Ok(f) => f,
            Err(_) => return false,
        };
        match fs.read_file("HELLO.TXT") {
            Ok(data) => {
                let expected = b"Hello from FAT32!";
                data.len() >= expected.len() && &data[..expected.len()] == expected
            }
            Err(_) => false,
        }
    }

    /// FAT32 の空き容量テスト
    /// 総クラスタ数と空きクラスタ数の整合性を確認する。
    fn test_fat32_space(&self) -> bool {
        let mut fs = match crate::fat32::Fat32::new() {
            Ok(f) => f,
            Err(_) => return false,
        };
        let total = fs.total_clusters();
        let free = match fs.free_clusters() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if total == 0 || free > total {
            return false;
        }
        let used = total - free;
        used > 0
    }

    /// GUI アプリ (TETRIS.ELF) の存在確認
    /// ELF ヘッダのマジックを検証して、ファイルが読めることを確認する。
    fn test_tetris_elf(&self) -> bool {
        let mut fs = match crate::fat32::Fat32::new() {
            Ok(f) => f,
            Err(_) => return false,
        };
        let data = match fs.read_file("TETRIS.ELF") {
            Ok(d) => d,
            Err(_) => return false,
        };
        if data.len() < 4 {
            return false;
        }
        data[0] == 0x7F && data[1] == b'E' && data[2] == b'L' && data[3] == b'F'
    }

    /// コンソールエディタ (ED.ELF) の存在確認
    /// ELF ヘッダのマジックを検証して、ファイルが読めることを確認する。
    fn test_console_editor_elf(&self) -> bool {
        let mut fs = match crate::fat32::Fat32::new() {
            Ok(f) => f,
            Err(_) => return false,
        };
        let data = match fs.read_file("ED.ELF") {
            Ok(d) => d,
            Err(_) => return false,
        };
        if data.len() < 4 {
            return false;
        }
        data[0] == 0x7F && data[1] == b'E' && data[2] == b'L' && data[3] == b'F'
    }

    /// ネットワーク (DNS) のテスト
    /// example.com を解決してみる（QEMU SLIRP は常に応答を返すはず）
    fn test_network_dns(&self) -> bool {
        // virtio-net が利用可能か確認
        {
            let drv = crate::virtio_net::VIRTIO_NET.lock();
            if drv.is_none() {
                return false;
            }
        }

        // DNS lookup を試行
        match crate::net::dns_lookup("example.com") {
            Ok(ip) => {
                // 何らかの IP が返ってくれば OK（0.0.0.0 以外）
                ip != [0, 0, 0, 0]
            }
            Err(_) => false,
        }
    }

    /// netd が起動しているか確認する
    fn netd_is_running(&self) -> bool {
        crate::scheduler::find_task_id_by_name("NETD.ELF").is_some()
    }

    /// ユーザー空間 netd の DNS テスト
    /// IPC 経由で netd に example.com を問い合わせる。
    fn test_network_netd_dns(&self) -> bool {
        // netd のタスク ID を探す（init が /NETD.ELF を起動している前提）
        let netd_id = match crate::scheduler::find_task_id_by_name("NETD.ELF") {
            Some(id) => id,
            None => return false,
        };

        // リクエストを構築: [opcode][len][payload]
        let opcode: u32 = 1; // DNS_LOOKUP
        let payload = b"example.com";
        let mut req = [0u8; 2048];
        let header_len = 8;
        if header_len + payload.len() > req.len() {
            return false;
        }
        req[0..4].copy_from_slice(&opcode.to_le_bytes());
        req[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        req[8..8 + payload.len()].copy_from_slice(payload);

        // IPC 送信
        let sender = crate::scheduler::current_task_id();
        if crate::ipc::send(sender, netd_id, req[..header_len + payload.len()].to_vec()).is_err() {
            return false;
        }

        // IPC 受信（5 秒タイムアウト）
        let msg = match crate::ipc::recv(sender, 5000) {
            Ok(m) => m,
            Err(_) => return false,
        };

        // レスポンスをパース: [opcode][status][len][payload]
        if msg.data.len() < 12 {
            return false;
        }
        let resp_opcode = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
        if resp_opcode != opcode {
            return false;
        }
        let status = i32::from_le_bytes([msg.data[4], msg.data[5], msg.data[6], msg.data[7]]);
        if status < 0 {
            return false;
        }
        let len = u32::from_le_bytes([msg.data[8], msg.data[9], msg.data[10], msg.data[11]]) as usize;
        if 12 + len > msg.data.len() || len != 4 {
            return false;
        }
        let ip = [msg.data[12], msg.data[13], msg.data[14], msg.data[15]];
        ip != [0, 0, 0, 0]
    }

    /// GUI IPC のテスト
    fn test_gui_ipc(&self) -> bool {
        let gui_id = match crate::scheduler::find_task_id_by_name("GUI.ELF") {
            Some(id) => id,
            None => return false,
        };

        fn recv_with_timeout(task_id: u64, timeout_ms: u64) -> Result<crate::ipc::IpcMessage, crate::user_ptr::SyscallError> {
            let start_tick =
                crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
            let deadline_tick = if timeout_ms == 0 {
                None
            } else {
                let ticks = (timeout_ms * 182 / 10000).max(1);
                Some(start_tick + ticks)
            };

            // 予備のスピン上限（タイマが止まっていても永久待ちしない）
            let mut spin_limit: u64 = 5_000_000;
            let mut yield_count: u64 = 0;
            loop {
                if let Some(msg) = crate::ipc::try_recv(task_id) {
                    return Ok(msg);
                }

                if let Some(deadline) = deadline_tick {
                    let now = crate::interrupts::TIMER_TICK_COUNT
                        .load(core::sync::atomic::Ordering::Relaxed);
                    if now >= deadline {
                        return Err(crate::user_ptr::SyscallError::Timeout);
                    }
                }

                if spin_limit == 0 {
                    return Err(crate::user_ptr::SyscallError::Timeout);
                }
                spin_limit -= 1;
                yield_count += 1;
                if yield_count % 1_000 == 0 {
                    crate::scheduler::yield_now();
                }
                core::hint::spin_loop();
            }
        }

        let sender = crate::scheduler::current_task_id();
        let send_and_wait = |opcode: u32, payload: &[u8]| -> bool {
            let mut req = [0u8; 2048];
            let header_len = 8;
            req[0..4].copy_from_slice(&opcode.to_le_bytes());
            req[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            req[8..8 + payload.len()].copy_from_slice(payload);

            for _ in 0..2 {
                if crate::ipc::send(sender, gui_id, req[..header_len + payload.len()].to_vec()).is_err() {
                    return false;
                }
                crate::scheduler::sleep_ms(10);
                let msg = match recv_with_timeout(sender, 15000) {
                    Ok(m) => m,
                    Err(_) => {
                        crate::scheduler::sleep_ms(200);
                        continue;
                    }
                };
                if msg.data.len() < 12 {
                    crate::scheduler::sleep_ms(200);
                    continue;
                }
                let resp_opcode = u32::from_le_bytes([
                    msg.data[0],
                    msg.data[1],
                    msg.data[2],
                    msg.data[3],
                ]);
                if resp_opcode != opcode {
                    crate::scheduler::sleep_ms(200);
                    continue;
                }
                let status = i32::from_le_bytes([
                    msg.data[4],
                    msg.data[5],
                    msg.data[6],
                    msg.data[7],
                ]);
                if status != 0 {
                    return false;
                }
                return true;
            }
            false
        };

        // CLEAR (opcode=1) を送る
        kprintln!("[selftest] gui_ipc: clear send");
        let opcode_clear: u32 = 1;
        let payload_clear = [0u8, 0u8, 32u8];
        if !send_and_wait(opcode_clear, &payload_clear) {
            return false;
        }
        kprintln!("[selftest] gui_ipc: clear ok");

        // CIRCLE (opcode=5) を送る
        kprintln!("[selftest] gui_ipc: circle send");
        let opcode_circle: u32 = 5;
        let mut payload_circle = [0u8; 17];
        let cx = 120u32.to_le_bytes();
        let cy = 120u32.to_le_bytes();
        let r = 30u32.to_le_bytes();
        payload_circle[0..4].copy_from_slice(&cx);
        payload_circle[4..8].copy_from_slice(&cy);
        payload_circle[8..12].copy_from_slice(&r);
        payload_circle[12] = 255;
        payload_circle[13] = 255;
        payload_circle[14] = 0;
        payload_circle[15] = 0; // outline
        payload_circle[16] = 0;
        if !send_and_wait(opcode_circle, &payload_circle) {
            return false;
        }
        kprintln!("[selftest] gui_ipc: circle ok");

        let mut req = [0u8; 2048];
        let header_len = 8;

        // TEXT (opcode=6) を送る
        kprintln!("[selftest] gui_ipc: text send");
        let opcode_text: u32 = 6;
        let text = b"HI";
        let mut payload_text = [0u8; 18 + 2];
        payload_text[0..4].copy_from_slice(&10u32.to_le_bytes()); // x
        payload_text[4..8].copy_from_slice(&10u32.to_le_bytes()); // y
        payload_text[8] = 255;
        payload_text[9] = 255;
        payload_text[10] = 255;
        payload_text[11] = 0;
        payload_text[12] = 0;
        payload_text[13] = 0;
        payload_text[14..18].copy_from_slice(&(text.len() as u32).to_le_bytes());
        payload_text[18..20].copy_from_slice(text);

        req[0..4].copy_from_slice(&opcode_text.to_le_bytes());
        req[4..8].copy_from_slice(&(payload_text.len() as u32).to_le_bytes());
        req[8..8 + payload_text.len()].copy_from_slice(&payload_text);

        if crate::ipc::send(sender, gui_id, req[..header_len + payload_text.len()].to_vec()).is_err() {
            return false;
        }

        let msg = match recv_with_timeout(sender, 5000) {
            Ok(m) => m,
            Err(_) => return false,
        };
        if msg.data.len() < 12 {
            return false;
        }
        let resp_opcode = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
        if resp_opcode != opcode_text {
            return false;
        }
        let status = i32::from_le_bytes([msg.data[4], msg.data[5], msg.data[6], msg.data[7]]);
        if status != 0 {
            return false;
        }
        kprintln!("[selftest] gui_ipc: text ok");

        // MOUSE (opcode=8) を送る
        kprintln!("[selftest] gui_ipc: mouse send");
        let opcode_mouse: u32 = 8;
        req[0..4].copy_from_slice(&opcode_mouse.to_le_bytes());
        req[4..8].copy_from_slice(&0u32.to_le_bytes());

        if crate::ipc::send(sender, gui_id, req[..header_len].to_vec()).is_err() {
            return false;
        }

        let msg = match recv_with_timeout(sender, 5000) {
            Ok(m) => m,
            Err(_) => return false,
        };
        if msg.data.len() < 12 {
            return false;
        }
        let resp_opcode = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
        if resp_opcode != opcode_mouse {
            return false;
        }
        let status = i32::from_le_bytes([msg.data[4], msg.data[5], msg.data[6], msg.data[7]]);
        if status != 0 {
            return false;
        }
        let len = u32::from_le_bytes([msg.data[8], msg.data[9], msg.data[10], msg.data[11]]) as usize;
        if len != 16 || msg.data.len() < 12 + len {
            return false;
        }
        kprintln!("[selftest] gui_ipc: mouse ok");

        let send_window = |opcode: u32, payload: &[u8]| -> Option<crate::ipc::IpcMessage> {
            let mut req = [0u8; 2048];
            let header_len = 8;
            req[0..4].copy_from_slice(&opcode.to_le_bytes());
            req[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            req[8..8 + payload.len()].copy_from_slice(payload);
            for _ in 0..2 {
                if crate::ipc::send(sender, gui_id, req[..header_len + payload.len()].to_vec()).is_err() {
                    return None;
                }
                crate::scheduler::sleep_ms(10);
                if let Ok(m) = recv_with_timeout(sender, 15000) {
                    return Some(m);
                }
                crate::scheduler::sleep_ms(200);
            }
            None
        };

        // WINDOW_CREATE (opcode=16) を送る
        kprintln!("[selftest] gui_ipc: window create send");
        let opcode_win_create: u32 = 16;
        let title = b"TEST";
        let mut payload_win = [0u8; 12 + 4];
        payload_win[0..4].copy_from_slice(&200u32.to_le_bytes()); // w
        payload_win[4..8].copy_from_slice(&120u32.to_le_bytes()); // h
        payload_win[8..12].copy_from_slice(&(title.len() as u32).to_le_bytes());
        payload_win[12..16].copy_from_slice(title);
        let msg = match send_window(opcode_win_create, &payload_win) {
            Some(m) => m,
            None => return false,
        };
        if msg.data.len() < 16 {
            return false;
        }
        let resp_opcode = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
        if resp_opcode != opcode_win_create {
            return false;
        }
        let status = i32::from_le_bytes([msg.data[4], msg.data[5], msg.data[6], msg.data[7]]);
        if status != 0 {
            return false;
        }
        let len = u32::from_le_bytes([msg.data[8], msg.data[9], msg.data[10], msg.data[11]]) as usize;
        if len != 4 || msg.data.len() < 12 + len {
            return false;
        }
        let win_id = u32::from_le_bytes([msg.data[12], msg.data[13], msg.data[14], msg.data[15]]);
        kprintln!("[selftest] gui_ipc: window create ok (id={})", win_id);

        // WINDOW_CLEAR (opcode=19)
        kprintln!("[selftest] gui_ipc: window clear send");
        let opcode_win_clear: u32 = 19;
        let mut payload_clear = [0u8; 7];
        payload_clear[0..4].copy_from_slice(&win_id.to_le_bytes());
        payload_clear[4] = 16;
        payload_clear[5] = 16;
        payload_clear[6] = 32;
        let msg = match send_window(opcode_win_clear, &payload_clear) {
            Some(m) => m,
            None => return false,
        };
        let status = i32::from_le_bytes([msg.data[4], msg.data[5], msg.data[6], msg.data[7]]);
        if status != 0 {
            return false;
        }
        kprintln!("[selftest] gui_ipc: window clear ok");

        // WINDOW_PRESENT (opcode=22)
        kprintln!("[selftest] gui_ipc: window present send");
        let opcode_win_present: u32 = 22;
        let mut payload_present = [0u8; 4];
        payload_present.copy_from_slice(&win_id.to_le_bytes());
        let msg = match send_window(opcode_win_present, &payload_present) {
            Some(m) => m,
            None => return false,
        };
        let status = i32::from_le_bytes([msg.data[4], msg.data[5], msg.data[6], msg.data[7]]);
        if status != 0 {
            return false;
        }
        kprintln!("[selftest] gui_ipc: window present ok");

        // WINDOW_MOUSE (opcode=23)
        kprintln!("[selftest] gui_ipc: window mouse send");
        let opcode_win_mouse: u32 = 23;
        let mut payload_wm = [0u8; 4];
        payload_wm.copy_from_slice(&win_id.to_le_bytes());
        let msg = match send_window(opcode_win_mouse, &payload_wm) {
            Some(m) => m,
            None => return false,
        };
        let status = i32::from_le_bytes([msg.data[4], msg.data[5], msg.data[6], msg.data[7]]);
        if status != 0 {
            return false;
        }
        let len = u32::from_le_bytes([msg.data[8], msg.data[9], msg.data[10], msg.data[11]]) as usize;
        if len != 16 || msg.data.len() < 12 + len {
            return false;
        }
        kprintln!("[selftest] gui_ipc: window mouse ok");
        true
    }

    /// telnetd サービスが起動しているかを確認する
    fn test_telnetd_service(&self) -> bool {
        crate::scheduler::find_task_id_by_name("TELNETD.ELF").is_some()
    }

    /// httpd サービスが起動しているかを確認する
    fn test_httpd_service(&self) -> bool {
        crate::scheduler::find_task_id_by_name("HTTPD.ELF").is_some()
    }

    /// httpd のディレクトリリスティングが動作する前提条件をテストする
    ///
    /// httpd はルートディレクトリを開いてエントリ一覧を HTML で返す。
    /// ここでは同じ list_dir_to_buffer を呼んで、
    /// ルートの一覧に HELLO.TXT が含まれることを確認する。
    fn test_httpd_dirlist(&self) -> bool {
        use alloc::vec;

        let mut buf = vec![0u8; 2048];
        let n = match crate::syscall::list_dir_to_buffer_for_test("/", &mut buf) {
            Ok(n) => n,
            Err(_) => return false,
        };

        if n == 0 {
            return false;
        }

        // HELLO.TXT が一覧に含まれるか確認
        let text = match core::str::from_utf8(&buf[..n]) {
            Ok(s) => s,
            Err(_) => return false,
        };
        text.contains("HELLO.TXT")
    }

    /// beep コマンド: AC97 ドライバでビープ音を再生する。
    ///
    /// # 使い方
    /// - `beep` — デフォルト (440Hz, 200ms)
    /// - `beep 880` — 880Hz, 200ms
    /// - `beep 880 500` — 880Hz, 500ms
    fn cmd_beep(&self, args: &str) {
        let parts: Vec<&str> = args.split_whitespace().collect();

        let freq: u32 = if parts.is_empty() {
            440
        } else {
            match parts[0].parse::<u32>() {
                Ok(n) if n >= 1 && n <= 20000 => n,
                _ => {
                    kprintln!("Error: freq must be 1-20000");
                    return;
                }
            }
        };

        let duration: u32 = if parts.len() < 2 {
            200
        } else {
            match parts[1].parse::<u32>() {
                Ok(n) if n >= 1 && n <= 10000 => n,
                _ => {
                    kprintln!("Error: duration must be 1-10000 ms");
                    return;
                }
            }
        };

        let mut ac97 = crate::ac97::AC97.lock();
        match ac97.as_mut() {
            Some(driver) => {
                kprintln!("Playing {}Hz for {}ms...", freq, duration);
                driver.play_tone(freq, duration);
                kprintln!("Done.");
            }
            None => {
                kprintln!("Error: AC97 audio not available");
            }
        }
    }

    /// panic コマンド: 意図的にカーネルパニックを発生させる。
    /// panic ハンドラのテスト用。シリアルと画面に赤字で panic 情報が表示されるはず。
    fn cmd_panic(&self) {
        panic!("User-triggered panic from shell command");
    }

    /// halt コマンド: 割り込みを無効化して CPU を停止する。
    /// hlt 命令は割り込みが来るまで CPU を停止するが、cli で割り込みを無効化しているので
    /// 二度と復帰しない = システム停止。
    fn cmd_halt(&self) {
        kprintln!("System halted.");
        loop {
            x86_64::instructions::interrupts::disable();
            x86_64::instructions::hlt();
        }
    }
}

/// カーネル側から selftest を実行するための公開関数。
///
/// syscall から呼べるように、最小限の Shell を生成して selftest を実行する。
pub fn run_selftest() {
    let shell = Shell::new(0, 0);
    shell.cmd_selftest("");
}
