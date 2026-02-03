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
        kprintln!("  ls [path]       - List files on FAT16 disk (e.g., ls /SUBDIR)");
        kprintln!("  cat <path>      - Display file contents (e.g., cat /SUBDIR/FILE.TXT)");
        kprintln!("  write <name> <text> - Create a file with text content");
        kprintln!("  rm <name>       - Delete a file");
        kprintln!("  run <path>      - Load and run ELF binary (e.g., run /SUBDIR/APP.ELF)");
        kprintln!("  spawn <path>    - Spawn ELF as background process (e.g., spawn HELLO.ELF)");
        kprintln!("  netpoll [n]     - Poll network for n seconds (default 10)");
        kprintln!("  ip              - Show IP configuration");
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

        // セクタ 0〜163 は FAT16 のメタデータ領域なので警告
        if sector < 164 {
            framebuffer::set_global_colors((255, 255, 0), (0, 0, 128));
            kprintln!("WARNING: Sector {} is in FAT16 metadata area!", sector);
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

    /// ls コマンド: FAT16 ディスクのディレクトリにあるファイル一覧を表示する。
    ///
    /// 引数なし: ルートディレクトリを表示
    /// 引数あり: 指定パスのディレクトリを表示（例: ls /SUBDIR）
    fn cmd_ls(&self, args: &str) {
        let path = args.trim();

        let fs = match crate::fat16::Fat16::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT16 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        // パスが指定されていれば大文字に変換（FAT16 は大文字のみ）
        let path_upper: alloc::string::String = if path.is_empty() {
            String::from("/")
        } else {
            path.chars().map(|c| c.to_ascii_uppercase()).collect()
        };

        match fs.list_dir(&path_upper) {
            Ok(entries) => {
                // ディレクトリパスを表示
                if path_upper == "/" {
                    kprintln!("Directory: /");
                } else {
                    kprintln!("Directory: {}", path_upper);
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

    /// cat コマンド: FAT16 ディスクのファイル内容を表示する。
    /// ファイル名は大文字の 8.3 形式で指定（例: cat HELLO.TXT）
    fn cmd_cat(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: cat <FILENAME>");
            kprintln!("  File names are in 8.3 format (e.g., HELLO.TXT)");
            return;
        }

        // ファイル名を大文字に変換（FAT16 は大文字のみ）
        let filename_upper: alloc::string::String = filename.chars()
            .map(|c| c.to_ascii_uppercase())
            .collect();

        let fs = match crate::fat16::Fat16::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT16 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        match fs.read_file(&filename_upper) {
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

    /// write コマンド: FAT16 ディスクに新しいファイルを作成する。
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

        // ファイル名を大文字に変換
        let filename_upper: alloc::string::String = filename
            .chars()
            .map(|c| c.to_ascii_uppercase())
            .collect();

        let fs = match crate::fat16::Fat16::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT16 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        // テキストの末尾に改行を追加
        let mut content = text.as_bytes().to_vec();
        content.push(b'\n');

        match fs.create_file(&filename_upper, &content) {
            Ok(()) => {
                framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                kprintln!("File '{}' created ({} bytes)", filename_upper, content.len());
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error creating file: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// rm コマンド: FAT16 ディスクのファイルを削除する。
    fn cmd_rm(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: rm <FILENAME>");
            return;
        }

        // ファイル名を大文字に変換
        let filename_upper: alloc::string::String = filename
            .chars()
            .map(|c| c.to_ascii_uppercase())
            .collect();

        let fs = match crate::fat16::Fat16::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT16 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        match fs.delete_file(&filename_upper) {
            Ok(()) => {
                framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                kprintln!("File '{}' deleted", filename_upper);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error deleting file: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// run コマンド: FAT16 ディスクから ELF バイナリを読み込んでユーザーモードで実行する。
    ///
    /// ファイル名は大文字の 8.3 形式で指定（例: run HELLO.ELF）
    ///
    /// 手順:
    ///   1. FAT16 からファイルを読み込む
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

        // ファイル名を大文字に変換（FAT16 は大文字のみ）
        let filename_upper: alloc::string::String = filename.chars()
            .map(|c| c.to_ascii_uppercase())
            .collect();

        // FAT16 からファイルを読み込む
        kprintln!("Loading {} from disk...", filename_upper);
        let fs = match crate::fat16::Fat16::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT16 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        let elf_data = match fs.read_file(&filename_upper) {
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

    /// spawn コマンド: FAT16 ディスクから ELF バイナリを読み込んで、
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

        // ファイル名を大文字に変換（FAT16 は大文字のみ）
        let filename_upper: alloc::string::String = filename.chars()
            .map(|c| c.to_ascii_uppercase())
            .collect();

        // FAT16 からファイルを読み込む
        kprintln!("Loading {} from disk...", filename_upper);
        let fs = match crate::fat16::Fat16::new() {
            Ok(fs) => fs,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("FAT16 error: {}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
                return;
            }
        };

        let elf_data = match fs.read_file(&filename_upper) {
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
        let process_name = filename_upper
            .rsplit('/')
            .next()
            .unwrap_or(&filename_upper);

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

        let drv = crate::virtio_net::VIRTIO_NET.lock();
        if let Some(ref d) = *drv {
            kprintln!("  MAC:        {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                d.mac_address[0], d.mac_address[1], d.mac_address[2],
                d.mac_address[3], d.mac_address[4], d.mac_address[5]);
        } else {
            kprintln!("  MAC:        (no network device)");
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
