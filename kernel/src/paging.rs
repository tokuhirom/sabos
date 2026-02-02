// paging.rs — ページテーブル管理
//
// x86_64 の仮想メモリは 4 階層のページテーブルで管理される:
//   L4 (PML4) → L3 (PDPT) → L2 (PD) → L1 (PT) → 物理フレーム
//
// UEFI は Boot Services 終了時にアイデンティティマッピング（仮想 == 物理）の
// ページテーブルを設定してくれている。つまり仮想アドレス 0x1000 は
// そのまま物理アドレス 0x1000 にマッピングされている。
//
// ここでは UEFI が設定済みのページテーブルを x86_64 crate の OffsetPageTable で
// ラップし、仮想→物理アドレス変換やマッピング作成をできるようにする。
//
// OffsetPageTable は「物理メモリが仮想アドレス空間の特定のオフセットにマッピング
// されている」という前提で動く。アイデンティティマッピングの場合、
// オフセットは 0（物理 == 仮想）なので VirtAddr::new(0) を渡す。

use lazy_static::lazy_static;
use spin::Mutex;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3};
use x86_64::structures::paging::mapper::MapToError;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, Size4KiB,
    Translate,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::memory::{self, MemoryRegion, FRAME_ALLOCATOR};

// =================================================================
// CR3 レジスタの読み出し
// =================================================================

/// CR3 レジスタから現在のページテーブルの物理アドレスを読み出す。
///
/// CR3 (Control Register 3) は L4 ページテーブルの物理アドレスを保持する
/// 特殊なレジスタ。CPU はメモリアクセスのたびにこのレジスタが指す
/// ページテーブルを参照してアドレス変換を行う。
pub fn read_cr3() -> PhysAddr {
    let (frame, _flags) = Cr3::read();
    frame.start_address()
}

// =================================================================
// L4 ページテーブルへのアクセス
// =================================================================

/// CR3 から L4 ページテーブルの可変参照を取得する。
///
/// アイデンティティマッピングのおかげで、CR3 が指す物理アドレスを
/// そのまま仮想アドレスとしてアクセスできる。
///
/// # Safety
/// - この関数は一度だけ呼ぶこと（複数の &mut 参照を作るとUB）
/// - アイデンティティマッピングが有効であること
unsafe fn active_level_4_table() -> &'static mut PageTable {
    let cr3_phys = read_cr3();
    // アイデンティティマッピングなので物理アドレス == 仮想アドレス
    let virt_addr = VirtAddr::new(cr3_phys.as_u64());
    let page_table_ptr: *mut PageTable = virt_addr.as_mut_ptr();
    // Safety: CR3 が指すアドレスは有効な L4 ページテーブルであり、
    // アイデンティティマッピングが有効なので仮想アドレスとしてアクセス可能。
    unsafe { &mut *page_table_ptr }
}

/// ページテーブルを書き込み可能かつユーザーアクセス可能にする。
///
/// UEFI (OVMF) はページテーブルを 2MiB 巨大ページの読み取り専用領域に配置する。
/// カーネルがページテーブルを変更するには、この保護を解除する必要がある。
/// また、Ring 3（ユーザーモード）からメモリにアクセスできるよう
/// USER_ACCESSIBLE フラグも全エントリに追加する。
///
/// 手順:
///   1. CR0.WP (Write Protect) ビットを一時的にクリア
///      → ring 0 で読み取り専用ページに書き込めるようになる
///   2. L4 → L3 → L2 → L1 テーブルを辿り、各エントリに
///      WRITABLE と USER_ACCESSIBLE を追加する
///   3. CR0.WP ビットを元に戻す
///   4. TLB をフラッシュして変更を反映
///
/// USER_ACCESSIBLE (U/S ビット) は全階層 (L4, L3, L2, L1) に設定する必要がある。
/// どれか1箇所でも欠けると Ring 3 からのアクセスが拒否される。
/// セキュリティ的には全メモリが Ring 3 からアクセス可能になるが、
/// 学習用プロジェクトの最初のステップとしては十分。
///
/// # Safety
/// - CR0.WP を一時的に無効化するため、この間は全メモリが書き込み可能になる
/// - 割り込みが無効化された状態で呼ぶべき（初期化時なので問題ない）
unsafe fn make_page_tables_user_accessible() {
    // CR0.WP を一時的にクリア（ring 0 での書き込み保護を無効化）
    let cr0 = Cr0::read();
    unsafe {
        Cr0::write(cr0 & !Cr0Flags::WRITE_PROTECT);
    }

    let l4_table = unsafe { active_level_4_table() };

    // L4 テーブルのすべての有効なエントリを辿る
    for l4_idx in 0..512 {
        let l4_entry = &mut l4_table[l4_idx];
        if l4_entry.is_unused() {
            continue;
        }

        // L4 エントリに WRITABLE と USER_ACCESSIBLE を追加
        let needed = PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        if !l4_entry.flags().contains(needed) {
            l4_entry.set_flags(l4_entry.flags() | needed);
        }

        // HUGE_PAGE なら L3 テーブルはない（512GiB ページ、通常使われない）
        if l4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            continue;
        }

        let l3_table: &mut PageTable =
            unsafe { &mut *(l4_entry.addr().as_u64() as *mut PageTable) };

        for l3_idx in 0..512 {
            let l3_entry = &mut l3_table[l3_idx];
            if l3_entry.is_unused() {
                continue;
            }

            // L3 エントリに WRITABLE と USER_ACCESSIBLE を追加
            let needed = PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
            if !l3_entry.flags().contains(needed) {
                l3_entry.set_flags(l3_entry.flags() | needed);
            }

            // 1GiB 巨大ページなら L2 テーブルはない
            if l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                continue;
            }

            let l2_table: &mut PageTable =
                unsafe { &mut *(l3_entry.addr().as_u64() as *mut PageTable) };

            for l2_idx in 0..512 {
                let l2_entry = &mut l2_table[l2_idx];
                if l2_entry.is_unused() {
                    continue;
                }

                // L2 エントリに WRITABLE と USER_ACCESSIBLE を追加
                let needed = PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
                if !l2_entry.flags().contains(needed) {
                    l2_entry.set_flags(l2_entry.flags() | needed);
                }

                // 2MiB 巨大ページなら L1 テーブルはない
                if l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                    continue;
                }

                // L1 テーブルの各エントリにも WRITABLE を追加
                let l1_table: &mut PageTable =
                    unsafe { &mut *(l2_entry.addr().as_u64() as *mut PageTable) };

                for l1_entry in l1_table.iter_mut() {
                    if l1_entry.is_unused() {
                        continue;
                    }
                    // L1 エントリにも WRITABLE と USER_ACCESSIBLE を追加
                    let needed = PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
                    if !l1_entry.flags().contains(needed) {
                        l1_entry.set_flags(l1_entry.flags() | needed);
                    }
                }
            }
        }
    }

    // CR0.WP を元に戻す
    unsafe {
        Cr0::write(cr0);
    }

    // TLB をフラッシュして変更を反映させる。
    // CR3 に同じ値を書き戻すことで全 TLB エントリが無効化される。
    let (frame, flags) = Cr3::read();
    unsafe {
        Cr3::write(frame, flags);
    }
}

// =================================================================
// グローバルページテーブル
// =================================================================

lazy_static! {
    /// グローバルな OffsetPageTable インスタンス。
    /// init() で初期化される。None なら未初期化。
    ///
    /// OffsetPageTable は仮想→物理アドレス変換やマッピング操作を提供する。
    /// ロック順序: PAGE_TABLE → FRAME_ALLOCATOR（デッドロック防止）
    static ref PAGE_TABLE: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);
}

// =================================================================
// 初期化
// =================================================================

/// ページング管理を初期化する。
///
/// 1. UEFI メモリマップから CONVENTIONAL 領域を収集してフレームアロケータを初期化
/// 2. CR3 から L4 テーブルを取得して OffsetPageTable を作成
///
/// ヒープアロケータ (allocator::init()) の後に呼ぶこと（Vec を使うため）。
pub fn init(memory_map: &MemoryMapOwned) {
    // --- フレームアロケータの初期化 ---
    // UEFI メモリマップから CONVENTIONAL 領域を収集する。
    // 1MiB 以下の低メモリはスキップ（BIOS/レガシーハードウェア領域）。
    let mut regions = alloc::vec::Vec::new();
    for desc in memory_map.entries() {
        if desc.ty == MemoryType::CONVENTIONAL {
            let start = desc.phys_start;
            let page_count = desc.page_count;

            // 1MiB (0x100000) 以下の領域はスキップ。
            // BIOS データ領域、VGA バッファ、レガシー ISA デバイスのメモリ等が
            // 配置されている可能性がある。
            if start < 0x100000 {
                continue;
            }

            regions.push(MemoryRegion { start, page_count });
        }
    }

    let total_frames: u64 = regions.iter().map(|r| r.page_count).sum();
    memory::init(regions);

    // --- ページテーブル領域を書き込み可能 + ユーザーアクセス可能にする ---
    // UEFI はページテーブルが配置された 2MiB ページを読み取り専用にしていることがある。
    // カーネルがページテーブルを変更するためには、これらを書き込み可能にする必要がある。
    // さらに Ring 3（ユーザーモード）からメモリにアクセスできるよう
    // USER_ACCESSIBLE フラグも全エントリに追加する。
    unsafe {
        make_page_tables_user_accessible();
    }

    // --- OffsetPageTable の作成 ---
    // UEFI が設定したアイデンティティマッピングのページテーブルをラップする。
    let page_table = unsafe {
        let l4_table = active_level_4_table();
        // offset = 0: アイデンティティマッピングなので仮想 == 物理
        OffsetPageTable::new(l4_table, VirtAddr::new(0))
    };

    *PAGE_TABLE.lock() = Some(page_table);

    // 初期化完了のログ
    let cr3 = read_cr3();
    crate::kprintln!(
        "Paging initialized (CR3: {:#x}, {} usable frames).",
        cr3.as_u64(),
        total_frames
    );
}

// =================================================================
// アドレス変換
// =================================================================

/// 仮想アドレスを物理アドレスに変換する。
/// マッピングされていないアドレスの場合は None を返す。
///
/// OffsetPageTable の Translate トレイトを使ってページテーブルを走査し、
/// 4 階層のテーブルを辿って最終的な物理アドレスを求める。
pub fn translate_addr(addr: VirtAddr) -> Option<PhysAddr> {
    let pt = PAGE_TABLE.lock();
    let pt = pt.as_ref().expect("paging not initialized");
    pt.translate_addr(addr)
}

// =================================================================
// L4 テーブルの統計情報
// =================================================================

/// L4 ページテーブルの使用中エントリ数を返す。
///
/// L4 テーブルには 512 個のエントリがあり、それぞれが 512GiB の仮想アドレス空間を
/// カバーする。PRESENT フラグが立っているエントリの数を数える。
pub fn l4_used_entries() -> usize {
    let pt = PAGE_TABLE.lock();
    let pt = pt.as_ref().expect("paging not initialized");
    pt.level_4_table()
        .iter()
        .filter(|entry| !entry.is_unused())
        .count()
}

// =================================================================
// デモ: テスト用マッピング
// =================================================================

/// ページングのテスト用デモ。
///
/// 1. 未使用の仮想アドレスに新しいマッピングを作成
/// 2. そのアドレスの仮想→物理変換が正しいことを検証
/// 3. マッピングを解除
///
/// これにより map_to / translate / unmap の一連の操作が正しく動くことを確認する。
pub fn demo_mapping() {
    crate::kprint!("Testing paging... ");

    // --- テスト 1: アイデンティティマッピングの検証 ---
    // CR3 が指すアドレスは必ずマッピングされているはず。
    // アイデンティティマッピングなら仮想 == 物理 になる。
    {
        let pt = PAGE_TABLE.lock();
        let pt = pt.as_ref().expect("paging not initialized");
        let cr3_addr = read_cr3();
        let cr3_virt = VirtAddr::new(cr3_addr.as_u64());
        let translated = pt.translate_addr(cr3_virt);
        assert_eq!(
            translated,
            Some(cr3_addr),
            "identity mapping: virt should equal phys for CR3 address"
        );
    }

    // --- テスト 2: マッピング作成・検証・解除 ---
    // 未使用の仮想アドレスに新しいマッピングを作り、
    // 仮想→物理変換が正しいことを確認してから解除する。
    {
        let test_virt = VirtAddr::new(0x0000_4000_0000_0000);
        let test_page: Page<Size4KiB> = Page::containing_address(test_virt);

        // ロック順序を守る: PAGE_TABLE → FRAME_ALLOCATOR
        let mut pt_guard = PAGE_TABLE.lock();
        let pt = pt_guard.as_mut().expect("paging not initialized");
        let mut fa = FRAME_ALLOCATOR.lock();

        // フレームアロケータから物理フレームを1つ確保し、
        // テスト用仮想アドレスにマッピングする。
        let frame = fa
            .allocate_frame()
            .expect("failed to allocate frame for demo");
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;

        // マッピングを作成。map_to は中間テーブル (L3, L2, L1) 用にも
        // フレームを割り当てる。
        unsafe {
            match pt.map_to(test_page, frame, flags, &mut *fa) {
                Ok(flush) => flush.flush(), // TLB をフラッシュしてマッピングを有効化
                Err(MapToError::FrameAllocationFailed) => {
                    panic!("frame allocation failed during demo mapping");
                }
                Err(e) => {
                    panic!("unexpected error during demo mapping: {:?}", e);
                }
            }
        }

        // 変換が成功することを確認
        let translated = pt
            .translate_addr(test_virt)
            .expect("translation should succeed after mapping");
        assert_eq!(
            translated,
            frame.start_address(),
            "translated address should match mapped frame"
        );

        // マッピングを解除して、変換できなくなることを確認
        let (unmapped_frame, flush) = pt.unmap(test_page).expect("unmap should succeed");
        flush.flush();
        assert_eq!(
            unmapped_frame.start_address(),
            frame.start_address(),
            "unmapped frame should match original"
        );
        assert!(
            pt.translate_addr(test_virt).is_none(),
            "test address should not be mapped after unmap"
        );
    }

    crate::kprintln!("OK!");
}
