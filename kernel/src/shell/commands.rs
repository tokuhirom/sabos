// shell/commands.rs — シェルコマンド実装
//
// 各 cmd_* メソッドの実装。selftest 以外の全コマンドをここに集約する。

use alloc::vec::Vec;

use crate::framebuffer;
use crate::memory::FRAME_ALLOCATOR;
use crate::paging;
use crate::scheduler;
use crate::{kprint, kprintln};
use x86_64::VirtAddr;

use super::rdtsc;

impl super::Shell {
    /// help コマンド: 使えるコマンドの一覧を表示する。
    pub(super) fn cmd_help(&self) {
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
        kprintln!("  ip              - Show IP configuration");
        kprintln!("  linkstatus        - Show network link status");
        kprintln!("  selftest [target] - Run automated self-tests (target: all/base/core/fs/net/gui/service/list)");
        kprintln!("  ipc_bench [n]   - IPC round-trip benchmark (default: 1000 iterations)");
        kprintln!("  beep [freq] [ms] - Play beep sound (default: 440Hz 200ms)");
        kprintln!("  panic           - Trigger a kernel panic (for testing)");
        kprintln!("  shutdown        - ACPI S5 shutdown (power off)");
        kprintln!("  reboot          - ACPI reboot (system reset)");
        kprintln!("  halt            - Halt the system (HLT loop, no power off)");
        kprintln!("  exit_qemu [code] - Exit QEMU via ISA debug exit (0=success, 1=failure)");
    }

    /// clear コマンド: 画面をクリアする。
    pub(super) fn cmd_clear(&self) {
        framebuffer::clear_global_screen();
    }

    /// mem コマンド: メモリ情報を表示する。
    pub(super) fn cmd_mem(&self) {
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
    pub(super) fn cmd_page(&self, args: &str) {
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
    pub(super) fn cmd_ps(&self) {
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
    pub(super) fn cmd_echo(&self, args: &str) {
        kprintln!("{}", args);
    }

    /// usermode コマンド: Ring 3（ユーザーモード）でプログラムを実行する。
    ///
    /// プロセスごとの専用ページテーブルを作成し、CR3 を切り替えてから
    /// iretq で Ring 3 に遷移する。int 0x80 システムコールで文字列を出力して、
    /// SYS_EXIT で Ring 0（カーネル）に戻ってくる。
    /// 戻り後に CR3 をカーネルのページテーブルに復帰し、プロセスを破棄する。
    pub(super) fn cmd_usermode(&self) {
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
    pub(super) fn cmd_usertest(&self) {
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
    pub(super) fn cmd_isolate(&self) {
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
    pub(super) fn cmd_elf(&self) {
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

        // ELF プロセスを作成（引数なし・環境変数なし）
        kprintln!("Creating ELF process...");
        let (process, entry_point, user_stack_top, _argc, _argv, _envp) =
            match crate::usermode::create_elf_process(elf_data, &[], &[]) {
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
    pub(super) fn cmd_lspci(&self) {
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
    pub(super) fn cmd_blkread(&self, args: &str) {
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

        let mut devs = crate::virtio_blk::VIRTIO_BLKS.lock();
        let drv = match devs.get_mut(0) {
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
    pub(super) fn cmd_blkwrite(&self, args: &str) {
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

        let mut devs = crate::virtio_blk::VIRTIO_BLKS.lock();
        let drv = match devs.get_mut(0) {
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
    pub(super) fn cmd_ls(&self, args: &str) {
        let path = args.trim();
        let path = if path.is_empty() { "/" } else { path };

        match crate::vfs::list_dir(path) {
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
                    let attr_str = if entry.kind == crate::vfs::VfsNodeKind::Directory {
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
                kprintln!("Error listing directory: {:?}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// cat コマンド: VFS 経由でファイル内容を表示する。
    pub(super) fn cmd_cat(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: cat <FILENAME>");
            return;
        }

        match crate::vfs::read_file(filename) {
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
                kprintln!("Error: {:?}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// write コマンド: VFS 経由でファイルを作成する。
    ///
    /// 使い方: write <FILENAME> <TEXT>
    /// 例: write TEST.TXT Hello World
    pub(super) fn cmd_write(&self, args: &str) {
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

        // テキストの末尾に改行を追加
        let mut content = text.as_bytes().to_vec();
        content.push(b'\n');

        match crate::vfs::create_file(filename, &content) {
            Ok(()) => {
                framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                kprintln!("File '{}' created ({} bytes)", filename, content.len());
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error creating file: {:?}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// rm コマンド: VFS 経由でファイルを削除する。
    pub(super) fn cmd_rm(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: rm <FILENAME>");
            return;
        }

        match crate::vfs::delete_file(filename) {
            Ok(()) => {
                framebuffer::set_global_colors((0, 255, 0), (0, 0, 128));
                kprintln!("File '{}' deleted", filename);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error deleting file: {:?}", e);
                framebuffer::set_global_colors((255, 255, 255), (0, 0, 128));
            }
        }
    }

    /// run コマンド: VFS 経由で ELF バイナリを読み込んでユーザーモードで実行する。
    ///
    /// 手順:
    ///   1. VFS 経由でファイルを読み込む
    ///   2. ELF パース → LOAD セグメント情報を取得
    ///   3. プロセス作成 → ページテーブル + フレーム確保 + データロード
    ///   4. Ring 3 で実行
    ///   5. プロセスを破棄してフレームを返却
    pub(super) fn cmd_run(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: run <FILENAME>");
            kprintln!("  Example: run HELLO.ELF");
            return;
        }

        // VFS 経由でファイルを読み込む
        kprintln!("Loading {} from disk...", filename);
        let elf_data = match crate::vfs::read_file(filename) {
            Ok(data) => data,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error reading file: {:?}", e);
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

        // ELF プロセスを作成（引数なし・環境変数なし）
        let (process, entry_point, user_stack_top, _argc, _argv, _envp) =
            match crate::usermode::create_elf_process(&elf_data, &[], &[]) {
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

    /// spawn コマンド: VFS 経由で ELF バイナリを読み込んで、
    /// バックグラウンドでユーザープロセスとして実行する。
    ///
    /// run コマンドと異なり、プロセスはブロックせずに即座に戻る。
    /// プロセスはスケジューラに登録され、タイムスライスで他のタスクと並行実行される。
    ///
    /// 使い方: spawn HELLO.ELF
    pub(super) fn cmd_spawn(&self, args: &str) {
        let filename = args.trim();
        if filename.is_empty() {
            kprintln!("Usage: spawn <FILENAME>");
            kprintln!("  Example: spawn HELLO.ELF");
            kprintln!("  The process runs in the background. Use 'ps' to see tasks.");
            return;
        }

        // VFS 経由でファイルを読み込む
        kprintln!("Loading {} from disk...", filename);
        let elf_data = match crate::vfs::read_file(filename) {
            Ok(data) => data,
            Err(e) => {
                framebuffer::set_global_colors((255, 100, 100), (0, 0, 128));
                kprintln!("Error reading file: {:?}", e);
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
        match scheduler::spawn_user(process_name, &elf_data, &[]) {
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

    /// ip コマンド: IP 設定を表示する。
    pub(super) fn cmd_ip(&self) {
        let my_ip = crate::net_config::get_my_ip();
        let gw = crate::net_config::get_gateway_ip();
        let dns = crate::net_config::get_dns_server_ip();
        let mask = crate::net_config::get_subnet_mask();
        kprintln!("IP Configuration:");
        kprintln!("  IP Address:   {}.{}.{}.{}", my_ip[0], my_ip[1], my_ip[2], my_ip[3]);
        kprintln!("  Subnet Mask:  {}.{}.{}.{}", mask[0], mask[1], mask[2], mask[3]);
        kprintln!("  Gateway:      {}.{}.{}.{}", gw[0], gw[1], gw[2], gw[3]);
        kprintln!("  DNS:          {}.{}.{}.{}", dns[0], dns[1], dns[2], dns[3]);

        let drv = crate::virtio_net::VIRTIO_NET.lock();
        if let Some(ref d) = *drv {
            kprintln!("  MAC:        {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                d.mac_address[0], d.mac_address[1], d.mac_address[2],
                d.mac_address[3], d.mac_address[4], d.mac_address[5]);
        } else {
            kprintln!("  MAC:        (no network device)");
        }
    }

    /// linkstatus コマンド: ネットワークリンクの状態を表示する。
    pub(super) fn cmd_linkstatus(&self) {
        let link_up = crate::netstack::is_network_link_up();
        kprintln!("Network link: {}", if link_up { "UP" } else { "DOWN" });
    }

    /// beep コマンド: AC97 ドライバでビープ音を再生する。
    ///
    /// # 使い方
    /// - `beep` — デフォルト (440Hz, 200ms)
    /// - `beep 880` — 880Hz, 200ms
    /// - `beep 880 500` — 880Hz, 500ms
    pub(super) fn cmd_beep(&self, args: &str) {
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

    /// ipc_bench コマンド: IPC ラウンドトリップのベンチマーク
    ///
    /// 自分自身に N 回 send+recv して、TSC サイクル数で
    /// min/avg/max と推定スループットを表示する。
    ///
    /// # 使い方
    /// - `ipc_bench` — デフォルト (1000 回)
    /// - `ipc_bench 500` — 500 回
    pub(super) fn cmd_ipc_bench(&self, args: &str) {
        let n: usize = args.trim().parse().unwrap_or(1000);
        if n == 0 {
            kprintln!("Error: n must be > 0");
            return;
        }

        let task_id = crate::scheduler::current_task_id();
        let data = b"bench";

        // ウォームアップ: 10 回
        for _ in 0..10 {
            let _ = crate::ipc::send(task_id, task_id, data.to_vec());
            let _ = crate::ipc::recv(task_id, 1000);
        }

        let mut min_cycles: u64 = u64::MAX;
        let mut max_cycles: u64 = 0;
        let mut total_cycles: u64 = 0;

        for _ in 0..n {
            let start = rdtsc();
            let _ = crate::ipc::send(task_id, task_id, data.to_vec());
            let _ = crate::ipc::recv(task_id, 1000);
            let end = rdtsc();
            let elapsed = end.wrapping_sub(start);
            total_cycles += elapsed;
            if elapsed < min_cycles {
                min_cycles = elapsed;
            }
            if elapsed > max_cycles {
                max_cycles = elapsed;
            }
        }

        let avg_cycles = total_cycles / (n as u64);
        kprintln!("=== IPC Benchmark ===");
        kprintln!("  iterations: {}", n);
        kprintln!("  min: {} cycles", min_cycles);
        kprintln!("  avg: {} cycles", avg_cycles);
        kprintln!("  max: {} cycles", max_cycles);
        kprintln!("  total: {} cycles", total_cycles);
    }

    /// panic コマンド: 意図的にカーネルパニックを発生させる。
    /// panic ハンドラのテスト用。シリアルと画面に赤字で panic 情報が表示されるはず。
    pub(super) fn cmd_panic(&self) {
        panic!("User-triggered panic from shell command");
    }

    /// shutdown コマンド: ACPI S5 シャットダウンで電源を切る。
    /// PM1a_CNT レジスタに SLP_TYPa と SLP_EN を書き込んで S5 ステートに遷移する。
    pub(super) fn cmd_shutdown(&self) {
        kprintln!("Shutting down...");
        crate::acpi::acpi_shutdown();
    }

    /// reboot コマンド: ACPI リセットでシステムを再起動する。
    /// FADT reset register → 8042 キーボードコントローラ → トリプルフォルトの 3 段フォールバック。
    pub(super) fn cmd_reboot(&self) {
        kprintln!("Rebooting...");
        crate::acpi::acpi_reboot();
    }

    /// halt コマンド: 割り込みを無効化して CPU を停止する。
    /// hlt 命令は割り込みが来るまで CPU を停止するが、cli で割り込みを無効化しているので
    /// 二度と復帰しない = システム停止。
    pub(super) fn cmd_halt(&self) {
        kprintln!("System halted.");
        loop {
            x86_64::instructions::interrupts::disable();
            x86_64::instructions::hlt();
        }
    }

    /// exit_qemu コマンド: ISA debug exit デバイス経由で QEMU を終了する。
    /// QEMU の exit code は (code << 1) | 1 になる。
    ///   exit_qemu 0 → QEMU exit 1（成功）
    ///   exit_qemu 1 → QEMU exit 3（失敗）
    pub(super) fn cmd_exit_qemu(&self, args: &str) {
        let code: u32 = args.trim().parse().unwrap_or(0);
        kprintln!("Exiting QEMU with debug exit code {}...", code);
        crate::qemu::debug_exit(code);
        // ISA debug exit デバイスが設定されていない場合はここに到達する
        kprintln!("WARN: ISA debug exit device not available.");
        kprintln!("Start QEMU with: -device isa-debug-exit,iobase=0xf4,iosize=0x04");
    }
}
