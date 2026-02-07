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
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr3Flags};
use x86_64::structures::paging::mapper::MapToError;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
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

/// ページテーブルを書き込み可能にする。
///
/// UEFI (OVMF) はページテーブルを 2MiB 巨大ページの読み取り専用領域に配置する。
/// カーネルがページテーブルを変更するには、この保護を解除する必要がある。
///
/// 手順:
///   1. CR0.WP (Write Protect) ビットを一時的にクリア
///      → ring 0 で読み取り専用ページに書き込めるようになる
///   2. L4 → L3 → L2 → L1 テーブルを辿り、各エントリに WRITABLE を追加する
///   3. CR0.WP ビットを元に戻す
///   4. TLB をフラッシュして変更を反映
///
/// USER_ACCESSIBLE は設定しない。Ring 3 からアクセスが必要なページには
/// set_user_accessible() で個別に設定する。
///
/// # Safety
/// - CR0.WP を一時的に無効化するため、この間は全メモリが書き込み可能になる
/// - 割り込みが無効化された状態で呼ぶべき（初期化時なので問題ない）
unsafe fn make_page_tables_writable() {
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

        // L4 エントリに WRITABLE を追加
        let needed = PageTableFlags::WRITABLE;
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

            // L3 エントリに WRITABLE を追加
            let needed = PageTableFlags::WRITABLE;
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

                // L2 エントリに WRITABLE を追加
                let needed = PageTableFlags::WRITABLE;
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
                    // L1 エントリにも WRITABLE を追加
                    let needed = PageTableFlags::WRITABLE;
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

    // ヒープ領域をフレームアロケータから除外する。
    if let Some((start, size)) = crate::allocator::heap_region_for_reserve() {
        memory::reserve_range(start, size);
        crate::kprintln!(
            "Reserved heap frames: {:#x} - {:#x}",
            start,
            start + size
        );
    }

    // --- ページテーブル領域を書き込み可能にする ---
    // UEFI はページテーブルが配置された 2MiB ページを読み取り専用にしていることがある。
    // カーネルがページテーブルを変更するためには、これらを書き込み可能にする必要がある。
    // USER_ACCESSIBLE は設定しない（Ring 3 に必要なページだけ個別に設定する）。
    unsafe {
        make_page_tables_writable();
    }

    // --- OffsetPageTable の作成 ---
    // UEFI が設定したアイデンティティマッピングのページテーブルをラップする。
    let page_table = unsafe {
        let l4_table = active_level_4_table();
        // offset = 0: アイデンティティマッピングなので仮想 == 物理
        OffsetPageTable::new(l4_table, VirtAddr::new(0))
    };

    *PAGE_TABLE.lock() = Some(page_table);

    // カーネルの CR3 を保存する。
    // プロセスページテーブルの作成・破棄時にカーネルのテーブルと比較するために使う。
    save_kernel_cr3();

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

// =================================================================
// 2MiB 巨大ページの分割
// =================================================================

/// 指定した仮想アドレスを含む L2 エントリが 2MiB 巨大ページ (Huge Page) の場合、
/// 512 個の 4KiB ページテーブルエントリに分割する。
///
/// UEFI/OVMF は L2 レベルで 2MiB 単位の巨大ページを使うことが多い。
/// 4KiB 単位で USER_ACCESSIBLE を個別制御するには、巨大ページを分割する必要がある。
///
/// 処理:
///   1. L4 → L3 → L2 テーブルを辿り、対象の L2 エントリを見つける
///   2. L2 エントリに HUGE_PAGE フラグが立っていなければ何もしない（既に 4KiB ページ）
///   3. HUGE_PAGE の場合:
///      a. フレームアロケータから 1 フレーム (4KiB) を確保 → 新しい L1 テーブル用
///      b. 元の 2MiB 巨大ページの物理アドレスとフラグを取得
///      c. 新 L1 テーブルの 512 エントリに、連続する 4KiB 物理フレームを設定
///         （フラグは元の巨大ページと同じ、ただし HUGE_PAGE は除く）
///      d. L2 エントリを新 L1 テーブルへのポインタに書き換え（HUGE_PAGE フラグ除去）
///      e. TLB フラッシュ
///
/// # Safety
/// - CR0.WP を一時的に無効化する
/// - フレームアロケータから 1 フレーム消費する
#[allow(dead_code)]
pub fn split_huge_page_if_needed(virt_addr: VirtAddr) {
    // 仮想アドレスから L4/L3/L2 のインデックスを求める。
    // x86_64 の仮想アドレスのビット構造:
    //   [47:39] = L4 インデックス
    //   [38:30] = L3 インデックス
    //   [29:21] = L2 インデックス
    //   [20:12] = L1 インデックス
    //   [11:0]  = ページ内オフセット
    let addr = virt_addr.as_u64();
    let l4_idx = ((addr >> 39) & 0x1FF) as usize;
    let l3_idx = ((addr >> 30) & 0x1FF) as usize;
    let l2_idx = ((addr >> 21) & 0x1FF) as usize;

    // CR0.WP を一時的にクリアして書き込みを許可
    let cr0 = Cr0::read();
    unsafe {
        Cr0::write(cr0 & !Cr0Flags::WRITE_PROTECT);
    }

    let l4_table = unsafe { active_level_4_table() };

    // L4 エントリを確認
    let l4_entry = &l4_table[l4_idx];
    if l4_entry.is_unused() || l4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        unsafe { Cr0::write(cr0); }
        return;
    }

    // L3 テーブルに進む
    let l3_table: &mut PageTable =
        unsafe { &mut *(l4_entry.addr().as_u64() as *mut PageTable) };
    let l3_entry = &l3_table[l3_idx];
    if l3_entry.is_unused() || l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        unsafe { Cr0::write(cr0); }
        return;
    }

    // L2 テーブルに進む
    let l2_table: &mut PageTable =
        unsafe { &mut *(l3_entry.addr().as_u64() as *mut PageTable) };
    let l2_entry = &mut l2_table[l2_idx];

    // 巨大ページでなければ分割不要
    if l2_entry.is_unused() || !l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        unsafe { Cr0::write(cr0); }
        return;
    }

    // 2MiB 巨大ページを 512 個の 4KiB ページに分割する
    let huge_phys = l2_entry.addr().as_u64(); // 巨大ページの物理ベースアドレス
    let huge_flags = l2_entry.flags() & !PageTableFlags::HUGE_PAGE; // HUGE_PAGE ビットを除いたフラグ

    // 新しい L1 テーブル用のフレームを確保
    let new_frame = {
        let mut fa = FRAME_ALLOCATOR.lock();
        fa.allocate_frame().expect("split_huge_page: フレーム確保に失敗")
    };

    // 新しい L1 テーブルをゼロ初期化
    let new_l1_table: &mut PageTable =
        unsafe { &mut *(new_frame.start_address().as_u64() as *mut PageTable) };
    // まず全エントリをゼロクリア
    for entry in new_l1_table.iter_mut() {
        entry.set_unused();
    }

    // 512 個の 4KiB エントリを元の巨大ページの連続する物理フレームで埋める
    for i in 0..512u64 {
        let phys = PhysAddr::new(huge_phys + i * 4096);
        let frame = PhysFrame::<Size4KiB>::containing_address(phys);
        // 元の巨大ページのフラグを引き継ぐ（HUGE_PAGE は除去済み）
        new_l1_table[i as usize].set_addr(frame.start_address(), huge_flags);
    }

    // L2 エントリを新 L1 テーブルへのポインタに書き換え
    // PRESENT | WRITABLE は最低限必要。元のフラグから HUGE_PAGE を除いたものを使う。
    let l2_flags = huge_flags & !PageTableFlags::HUGE_PAGE;
    l2_entry.set_addr(new_frame.start_address(), l2_flags);

    // CR0.WP を元に戻す
    unsafe {
        Cr0::write(cr0);
    }

    // TLB をフラッシュして変更を反映
    let (frame, flags) = Cr3::read();
    unsafe {
        Cr3::write(frame, flags);
    }
}

// =================================================================
// USER_ACCESSIBLE の範囲設定/解除
// =================================================================

/// 指定した仮想アドレス範囲のページに USER_ACCESSIBLE フラグを追加する。
///
/// Ring 3（ユーザーモード）からアクセスが必要なメモリ領域に対して呼ぶ。
/// 対象範囲の各 4KiB ページについて:
///   1. 巨大ページがあれば split_huge_page_if_needed() で分割
///   2. L4/L3/L2 の上位エントリにも USER_ACCESSIBLE を追加
///      （全階層に設定が必要。1箇所でも欠けると Ring 3 からアクセス不可）
///   3. L1 エントリに USER_ACCESSIBLE を追加
///
/// start は 4KiB アラインに切り下げられる。
#[allow(dead_code)]
pub fn set_user_accessible(start: VirtAddr, size: usize) {
    if size == 0 {
        return;
    }

    // 開始アドレスを 4KiB 境界に切り下げ
    let start_addr = start.as_u64() & !0xFFF;
    // 終了アドレス（切り上げ）
    let end_addr = (start.as_u64() + size as u64 + 0xFFF) & !0xFFF;

    // CR0.WP を一時的にクリア
    let cr0 = Cr0::read();
    unsafe {
        Cr0::write(cr0 & !Cr0Flags::WRITE_PROTECT);
    }

    let mut addr = start_addr;
    while addr < end_addr {
        let virt = VirtAddr::new(addr);

        // 巨大ページがあれば 4KiB に分割する
        // （CR0.WP はこの中で一時的に操作されるが、既にクリア済みなので問題ない）
        // 注意: split_huge_page_if_needed は内部で CR0.WP を操作するので、
        // ここでは一旦 WP を復帰してから呼び、再度クリアする
        unsafe { Cr0::write(cr0); }
        split_huge_page_if_needed(virt);
        unsafe { Cr0::write(cr0 & !Cr0Flags::WRITE_PROTECT); }

        // 仮想アドレスから各階層のインデックスを求める
        let l4_idx = ((addr >> 39) & 0x1FF) as usize;
        let l3_idx = ((addr >> 30) & 0x1FF) as usize;
        let l2_idx = ((addr >> 21) & 0x1FF) as usize;
        let l1_idx = ((addr >> 12) & 0x1FF) as usize;

        let l4_table = unsafe { active_level_4_table() };

        // L4 エントリに USER_ACCESSIBLE を追加
        let l4_entry = &mut l4_table[l4_idx];
        if l4_entry.is_unused() {
            addr += 4096;
            continue;
        }
        l4_entry.set_flags(l4_entry.flags() | PageTableFlags::USER_ACCESSIBLE);

        if l4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            addr += 4096;
            continue;
        }

        // L3 エントリに USER_ACCESSIBLE を追加
        let l3_table: &mut PageTable =
            unsafe { &mut *(l4_entry.addr().as_u64() as *mut PageTable) };
        let l3_entry = &mut l3_table[l3_idx];
        if l3_entry.is_unused() {
            addr += 4096;
            continue;
        }
        l3_entry.set_flags(l3_entry.flags() | PageTableFlags::USER_ACCESSIBLE);

        if l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            addr += 4096;
            continue;
        }

        // L2 エントリに USER_ACCESSIBLE を追加
        let l2_table: &mut PageTable =
            unsafe { &mut *(l3_entry.addr().as_u64() as *mut PageTable) };
        let l2_entry = &mut l2_table[l2_idx];
        if l2_entry.is_unused() {
            addr += 4096;
            continue;
        }
        l2_entry.set_flags(l2_entry.flags() | PageTableFlags::USER_ACCESSIBLE);

        if l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            // 巨大ページがまだ残っている場合（分割に失敗した場合）はスキップ
            addr += 4096;
            continue;
        }

        // L1 エントリに USER_ACCESSIBLE を追加
        let l1_table: &mut PageTable =
            unsafe { &mut *(l2_entry.addr().as_u64() as *mut PageTable) };
        let l1_entry = &mut l1_table[l1_idx];
        if !l1_entry.is_unused() {
            l1_entry.set_flags(l1_entry.flags() | PageTableFlags::USER_ACCESSIBLE);
        }

        addr += 4096;
    }

    // CR0.WP を元に戻す
    unsafe {
        Cr0::write(cr0);
    }

    // TLB をフラッシュして変更を反映
    let (frame, flags) = Cr3::read();
    unsafe {
        Cr3::write(frame, flags);
    }
}

/// 指定した仮想アドレス範囲のページから USER_ACCESSIBLE フラグを除去する。
///
/// Ring 3 からのアクセスが不要になったメモリ領域に対して呼ぶ。
/// L1 エントリのみから USER_ACCESSIBLE を除去する。
///
/// L4/L3/L2 の上位エントリからは除去しない。
/// 上位エントリの USER_ACCESSIBLE を外すと、その配下の全ページが
/// Ring 3 からアクセス不可になり、他のユーザーページにも影響するため。
///
/// start は 4KiB アラインに切り下げられる。
#[allow(dead_code)]
pub fn clear_user_accessible(start: VirtAddr, size: usize) {
    if size == 0 {
        return;
    }

    // 開始アドレスを 4KiB 境界に切り下げ
    let start_addr = start.as_u64() & !0xFFF;
    // 終了アドレス（切り上げ）
    let end_addr = (start.as_u64() + size as u64 + 0xFFF) & !0xFFF;

    // CR0.WP を一時的にクリア
    let cr0 = Cr0::read();
    unsafe {
        Cr0::write(cr0 & !Cr0Flags::WRITE_PROTECT);
    }

    let mut addr = start_addr;
    while addr < end_addr {
        // 仮想アドレスから各階層のインデックスを求める
        let l4_idx = ((addr >> 39) & 0x1FF) as usize;
        let l3_idx = ((addr >> 30) & 0x1FF) as usize;
        let l2_idx = ((addr >> 21) & 0x1FF) as usize;
        let l1_idx = ((addr >> 12) & 0x1FF) as usize;

        let l4_table = unsafe { active_level_4_table() };

        // L4 → L3 → L2 → L1 を辿る（上位エントリの USER_ACCESSIBLE は触らない）
        let l4_entry = &l4_table[l4_idx];
        if l4_entry.is_unused() || l4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            addr += 4096;
            continue;
        }

        let l3_table: &mut PageTable =
            unsafe { &mut *(l4_entry.addr().as_u64() as *mut PageTable) };
        let l3_entry = &l3_table[l3_idx];
        if l3_entry.is_unused() || l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            addr += 4096;
            continue;
        }

        let l2_table: &mut PageTable =
            unsafe { &mut *(l3_entry.addr().as_u64() as *mut PageTable) };
        let l2_entry = &l2_table[l2_idx];
        if l2_entry.is_unused() || l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            addr += 4096;
            continue;
        }

        // L1 エントリから USER_ACCESSIBLE を除去
        let l1_table: &mut PageTable =
            unsafe { &mut *(l2_entry.addr().as_u64() as *mut PageTable) };
        let l1_entry = &mut l1_table[l1_idx];
        if !l1_entry.is_unused() {
            l1_entry.set_flags(l1_entry.flags() & !PageTableFlags::USER_ACCESSIBLE);
        }

        addr += 4096;
    }

    // CR0.WP を元に戻す
    unsafe {
        Cr0::write(cr0);
    }

    // TLB をフラッシュして変更を反映
    let (frame, flags) = Cr3::read();
    unsafe {
        Cr3::write(frame, flags);
    }
}

// =================================================================
// プロセスごとのページテーブル管理
// =================================================================
//
// プロセス分離のために、プロセスごとに専用の L4 ページテーブルを持たせる。
// カーネル空間のマッピングは全プロセスで共有し（L4 エントリのコピー）、
// ユーザー空間のページだけプロセス固有の権限（USER_ACCESSIBLE）を設定する。
//
// 中間テーブル (L3/L2/L1) はカーネルと共有しているものを「分岐コピー」して
// プロセス専用の権限設定を可能にする。CR3 レジスタを切り替えることで
// アドレス空間を丸ごと切り替える。

/// カーネルの L4 ページテーブルの物理アドレスを保存するグローバル変数。
/// paging::init() で設定される。プロセスページテーブルの作成・破棄時に
/// カーネルのテーブルと比較するために使う。
static KERNEL_CR3: spin::Once<PhysAddr> = spin::Once::new();

/// カーネルの CR3 アドレスを記録する。paging::init() の最後に呼ぶ。
fn save_kernel_cr3() {
    let cr3 = read_cr3();
    KERNEL_CR3.call_once(|| cr3);
}

/// カーネルの CR3（L4 ページテーブルの物理アドレス）を返す。
pub fn kernel_cr3() -> PhysAddr {
    *KERNEL_CR3.get().expect("kernel CR3 not saved yet")
}

/// プロセス用のページテーブルを作成する。
///
/// 新しい L4 テーブルを確保し、カーネルの L4 エントリをすべてコピーする。
/// これにより、プロセスのページテーブルでもカーネル空間のマッピングが
/// そのまま使える（L3 以下のテーブルはポインタで共有される）。
///
/// 返り値はプロセス固有の L4 ページテーブルが配置された物理フレーム。
/// CR3 にこのフレームのアドレスを書き込むとアドレス空間が切り替わる。
pub fn create_process_page_table() -> PhysFrame<Size4KiB> {
    // 1. フレームアロケータから 1 フレーム確保 → 新 L4 テーブル
    let new_l4_frame = {
        let mut fa = FRAME_ALLOCATOR.lock();
        fa.allocate_frame()
            .expect("create_process_page_table: フレーム確保に失敗")
    };

    // 2. 新 L4 テーブルをゼロクリア
    let new_l4: &mut PageTable = unsafe {
        &mut *(new_l4_frame.start_address().as_u64() as *mut PageTable)
    };
    for entry in new_l4.iter_mut() {
        entry.set_unused();
    }

    // 3. カーネルの L4 テーブルの全エントリを新 L4 にコピー
    //    L4 エントリは L3 テーブルの物理アドレスを指すポインタなので、
    //    コピーするだけで L3 以下の木構造全体を共有できる。
    let kernel_l4: &PageTable = unsafe {
        &*(kernel_cr3().as_u64() as *const PageTable)
    };
    for i in 0..512 {
        if !kernel_l4[i].is_unused() {
            // エントリの内容（物理アドレス + フラグ）をそのままコピー
            new_l4[i].set_addr(kernel_l4[i].addr(), kernel_l4[i].flags());
        }
    }

    new_l4_frame
}

/// プロセスのページテーブルで指定範囲に USER_ACCESSIBLE を設定する。
///
/// カーネルと共有している中間テーブル (L3/L2/L1) は自動的に分岐コピーする。
/// 分岐コピーとは: カーネルのテーブルと同じアドレスを指しているエントリを見つけたら、
/// 新しいフレームを確保してテーブルの内容をコピーし、エントリを新フレームに差し替える。
/// これにより、カーネルのページテーブルに影響を与えずにプロセス固有の権限を設定できる。
///
/// 2MiB 巨大ページがある場合は split_huge_page_for_process() で 4KiB に分割する。
pub fn set_user_accessible_in_process(
    process_l4_frame: PhysFrame<Size4KiB>,
    start: VirtAddr,
    size: usize,
) {
    if size == 0 {
        return;
    }

    // 開始アドレスを 4KiB 境界に切り下げ
    let start_addr = start.as_u64() & !0xFFF;
    // 終了アドレス（切り上げ）
    let end_addr = (start.as_u64() + size as u64 + 0xFFF) & !0xFFF;

    // カーネルの L4 テーブルへの参照
    let kernel_l4: &PageTable = unsafe {
        &*(kernel_cr3().as_u64() as *const PageTable)
    };

    // プロセスの L4 テーブルへの可変参照
    let process_l4: &mut PageTable = unsafe {
        &mut *(process_l4_frame.start_address().as_u64() as *mut PageTable)
    };

    let mut addr = start_addr;
    while addr < end_addr {
        // 仮想アドレスから各階層のインデックスを求める
        let l4_idx = ((addr >> 39) & 0x1FF) as usize;
        let l3_idx = ((addr >> 30) & 0x1FF) as usize;
        let l2_idx = ((addr >> 21) & 0x1FF) as usize;
        let l1_idx = ((addr >> 12) & 0x1FF) as usize;

        // --- L4 エントリの処理 ---
        let l4_entry = &mut process_l4[l4_idx];
        if l4_entry.is_unused() {
            addr += 4096;
            continue;
        }
        // L4 エントリに USER_ACCESSIBLE を追加
        l4_entry.set_flags(l4_entry.flags() | PageTableFlags::USER_ACCESSIBLE);

        if l4_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            addr += 4096;
            continue;
        }

        // --- L3 テーブルの分岐コピー ---
        // プロセス L4 がカーネル L4 と同じ L3 を指しているなら、L3 をコピーする
        let kernel_l3_addr = if !kernel_l4[l4_idx].is_unused() {
            Some(kernel_l4[l4_idx].addr())
        } else {
            None
        };

        if let Some(k_l3_addr) = kernel_l3_addr {
            if l4_entry.addr() == k_l3_addr {
                // カーネルと同じ L3 を指している → 分岐コピー
                let new_l3_frame = {
                    let mut fa = FRAME_ALLOCATOR.lock();
                    fa.allocate_frame()
                        .expect("set_user_accessible_in_process: L3 フレーム確保に失敗")
                };
                // カーネル L3 の内容をコピー
                let kernel_l3: &PageTable = unsafe {
                    &*(k_l3_addr.as_u64() as *const PageTable)
                };
                let new_l3: &mut PageTable = unsafe {
                    &mut *(new_l3_frame.start_address().as_u64() as *mut PageTable)
                };
                for i in 0..512 {
                    if !kernel_l3[i].is_unused() {
                        new_l3[i].set_addr(kernel_l3[i].addr(), kernel_l3[i].flags());
                    } else {
                        new_l3[i].set_unused();
                    }
                }
                // L4 エントリを新 L3 に差し替え
                let l4_flags = l4_entry.flags();
                l4_entry.set_addr(new_l3_frame.start_address(), l4_flags);
            }
        }

        let l3_table: &mut PageTable = unsafe {
            &mut *(l4_entry.addr().as_u64() as *mut PageTable)
        };

        // --- L3 エントリの処理 ---
        let l3_entry = &mut l3_table[l3_idx];
        if l3_entry.is_unused() {
            addr += 4096;
            continue;
        }
        // L3 エントリに USER_ACCESSIBLE を追加
        l3_entry.set_flags(l3_entry.flags() | PageTableFlags::USER_ACCESSIBLE);

        if l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            addr += 4096;
            continue;
        }

        // --- L2 テーブルの分岐コピー ---
        // カーネルの L3 から対応する L2 アドレスを取得
        let kernel_l2_addr = if let Some(k_l3_addr) = kernel_l3_addr {
            let kernel_l3: &PageTable = unsafe {
                &*(k_l3_addr.as_u64() as *const PageTable)
            };
            if !kernel_l3[l3_idx].is_unused() {
                Some(kernel_l3[l3_idx].addr())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(k_l2_addr) = kernel_l2_addr {
            if l3_entry.addr() == k_l2_addr {
                // カーネルと同じ L2 を指している → 分岐コピー
                let new_l2_frame = {
                    let mut fa = FRAME_ALLOCATOR.lock();
                    fa.allocate_frame()
                        .expect("set_user_accessible_in_process: L2 フレーム確保に失敗")
                };
                let kernel_l2: &PageTable = unsafe {
                    &*(k_l2_addr.as_u64() as *const PageTable)
                };
                let new_l2: &mut PageTable = unsafe {
                    &mut *(new_l2_frame.start_address().as_u64() as *mut PageTable)
                };
                for i in 0..512 {
                    if !kernel_l2[i].is_unused() {
                        new_l2[i].set_addr(kernel_l2[i].addr(), kernel_l2[i].flags());
                    } else {
                        new_l2[i].set_unused();
                    }
                }
                let l3_flags = l3_entry.flags();
                l3_entry.set_addr(new_l2_frame.start_address(), l3_flags);
            }
        }

        let l2_table: &mut PageTable = unsafe {
            &mut *(l3_entry.addr().as_u64() as *mut PageTable)
        };

        // --- L2 エントリの処理 ---
        let l2_entry = &mut l2_table[l2_idx];
        if l2_entry.is_unused() {
            addr += 4096;
            continue;
        }
        // L2 エントリに USER_ACCESSIBLE を追加
        l2_entry.set_flags(l2_entry.flags() | PageTableFlags::USER_ACCESSIBLE);

        // 2MiB 巨大ページの場合、4KiB に分割する
        if l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            split_huge_page_for_process(l2_entry);
            // 分割後にもう一度 USER_ACCESSIBLE を設定（下の L1 処理に進む）
            if l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                // 分割に失敗した場合はスキップ
                addr += 4096;
                continue;
            }
        }

        // --- L1 テーブルの分岐コピー ---
        // カーネルの L2 から対応する L1 アドレスを取得
        let kernel_l1_addr = if let Some(k_l2_addr) = kernel_l2_addr {
            let kernel_l2: &PageTable = unsafe {
                &*(k_l2_addr.as_u64() as *const PageTable)
            };
            if !kernel_l2[l2_idx].is_unused()
                && !kernel_l2[l2_idx].flags().contains(PageTableFlags::HUGE_PAGE)
            {
                Some(kernel_l2[l2_idx].addr())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(k_l1_addr) = kernel_l1_addr {
            if l2_entry.addr() == k_l1_addr {
                // カーネルと同じ L1 を指している → 分岐コピー
                let new_l1_frame = {
                    let mut fa = FRAME_ALLOCATOR.lock();
                    fa.allocate_frame()
                        .expect("set_user_accessible_in_process: L1 フレーム確保に失敗")
                };
                let kernel_l1: &PageTable = unsafe {
                    &*(k_l1_addr.as_u64() as *const PageTable)
                };
                let new_l1: &mut PageTable = unsafe {
                    &mut *(new_l1_frame.start_address().as_u64() as *mut PageTable)
                };
                for i in 0..512 {
                    if !kernel_l1[i].is_unused() {
                        new_l1[i].set_addr(kernel_l1[i].addr(), kernel_l1[i].flags());
                    } else {
                        new_l1[i].set_unused();
                    }
                }
                let l2_flags = l2_entry.flags();
                l2_entry.set_addr(new_l1_frame.start_address(), l2_flags);
            }
        }

        let l1_table: &mut PageTable = unsafe {
            &mut *(l2_entry.addr().as_u64() as *mut PageTable)
        };

        // --- L1 エントリの処理 ---
        let l1_entry = &mut l1_table[l1_idx];
        if !l1_entry.is_unused() {
            l1_entry.set_flags(l1_entry.flags() | PageTableFlags::USER_ACCESSIBLE);
        }

        addr += 4096;
    }
}

/// プロセスのページテーブル内で 2MiB 巨大ページを 512 個の 4KiB ページに分割する。
///
/// split_huge_page_if_needed() と同様だが、プロセスのページテーブル内の
/// L2 エントリに対して直接操作する。CR0.WP の操作は不要（プロセスのテーブルは
/// 書き込み可能なフレーム上にあるため）。
fn split_huge_page_for_process(l2_entry: &mut x86_64::structures::paging::page_table::PageTableEntry) {
    if !l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return;
    }

    let huge_phys = l2_entry.addr().as_u64();
    let huge_flags = l2_entry.flags() & !PageTableFlags::HUGE_PAGE;

    // 新しい L1 テーブル用のフレームを確保
    let new_frame = {
        let mut fa = FRAME_ALLOCATOR.lock();
        fa.allocate_frame()
            .expect("split_huge_page_for_process: フレーム確保に失敗")
    };

    // 新しい L1 テーブルを初期化
    let new_l1_table: &mut PageTable = unsafe {
        &mut *(new_frame.start_address().as_u64() as *mut PageTable)
    };
    for entry in new_l1_table.iter_mut() {
        entry.set_unused();
    }

    // 512 個の 4KiB エントリを元の巨大ページの連続する物理フレームで埋める
    for i in 0..512u64 {
        let phys = PhysAddr::new(huge_phys + i * 4096);
        let frame = PhysFrame::<Size4KiB>::containing_address(phys);
        new_l1_table[i as usize].set_addr(frame.start_address(), huge_flags);
    }

    // L2 エントリを新 L1 テーブルへのポインタに書き換え
    let l2_flags = huge_flags & !PageTableFlags::HUGE_PAGE;
    l2_entry.set_addr(new_frame.start_address(), l2_flags);
}

/// L3 エントリの 1GiB 巨大ページを 512 個の 2MiB 巨大ページ（L2 テーブル）に分割する。
///
/// UEFI/OVMF は物理メモリの範囲を 1GiB 単位の巨大ページでマッピングすることがある。
/// mmap で 1GiB ページの一部に新しいマッピングを設定するには、
/// まず 1GiB → 512 x 2MiB に分割し、次に必要な 2MiB → 512 x 4KiB に分割する。
fn split_1gib_huge_page_for_process(l3_entry: &mut x86_64::structures::paging::page_table::PageTableEntry) {
    if !l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return;
    }

    let huge_phys = l3_entry.addr().as_u64();
    // 元のフラグから HUGE_PAGE を外したものを L2 の各エントリに使う
    // ただし L2 → 2MiB ページなので HUGE_PAGE を付けたまま各エントリに設定
    let base_flags = l3_entry.flags() & !PageTableFlags::HUGE_PAGE;

    // 新しい L2 テーブル用のフレームを確保
    let new_frame = {
        let mut fa = FRAME_ALLOCATOR.lock();
        fa.allocate_frame()
            .expect("split_1gib_huge_page_for_process: フレーム確保に失敗")
    };

    // 新しい L2 テーブルを初期化
    let new_l2_table: &mut PageTable = unsafe {
        &mut *(new_frame.start_address().as_u64() as *mut PageTable)
    };
    for entry in new_l2_table.iter_mut() {
        entry.set_unused();
    }

    // 512 個の 2MiB 巨大ページエントリで埋める
    for i in 0..512u64 {
        let phys = PhysAddr::new(huge_phys + i * (2 * 1024 * 1024)); // 2MiB 刻み
        let frame = PhysFrame::<Size4KiB>::containing_address(phys);
        // L2 エントリとして 2MiB 巨大ページを設定
        new_l2_table[i as usize].set_addr(frame.start_address(), base_flags | PageTableFlags::HUGE_PAGE);
    }

    // L3 エントリを新 L2 テーブルへのポインタに書き換え
    // HUGE_PAGE を外し、通常のテーブルポインタにする
    let l3_flags = base_flags
        | PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE;
    l3_entry.set_addr(new_frame.start_address(), l3_flags);
}

/// プロセスのページテーブルを破棄し、プロセス固有に作成したフレームを解放する。
///
/// カーネルと共有しているテーブル（L3/L2/L1）は解放しない。
/// プロセスの L4 エントリがカーネルの L4 エントリと異なるアドレスを指している場合、
/// そのテーブルはプロセス固有の分岐コピーなので解放対象。
/// 同様に、分岐コピーされた L3/L2 の中で、カーネルのものと異なるフレームも解放する。
pub fn destroy_process_page_table(process_l4_frame: PhysFrame<Size4KiB>) {
    let kernel_l4: &PageTable = unsafe {
        &*(kernel_cr3().as_u64() as *const PageTable)
    };
    let process_l4: &PageTable = unsafe {
        &*(process_l4_frame.start_address().as_u64() as *const PageTable)
    };

    for l4_idx in 0..512 {
        if process_l4[l4_idx].is_unused() {
            continue;
        }
        if process_l4[l4_idx].flags().contains(PageTableFlags::HUGE_PAGE) {
            continue;
        }

        let proc_l3_addr = process_l4[l4_idx].addr();
        let kernel_l3_addr = if !kernel_l4[l4_idx].is_unused() {
            kernel_l4[l4_idx].addr()
        } else {
            // カーネルにないエントリがプロセスにある（通常ないが念のため）
            PhysAddr::new(0)
        };

        // プロセス固有の L3 テーブルでなければスキップ（カーネルと共有）
        if proc_l3_addr == kernel_l3_addr {
            continue;
        }

        // プロセス固有の L3 テーブルの中身を走査して、さらに分岐コピーされた L2/L1 を解放
        let proc_l3: &PageTable = unsafe {
            &*(proc_l3_addr.as_u64() as *const PageTable)
        };
        let kernel_l3: &PageTable = if !kernel_l4[l4_idx].is_unused()
            && !kernel_l4[l4_idx].flags().contains(PageTableFlags::HUGE_PAGE)
        {
            unsafe { &*(kernel_l3_addr.as_u64() as *const PageTable) }
        } else {
            // カーネルに L3 がない場合 → L3 テーブルだけ解放
            let frame = PhysFrame::<Size4KiB>::containing_address(proc_l3_addr);
            let mut fa = FRAME_ALLOCATOR.lock();
            unsafe { fa.deallocate_frame(frame); }
            continue;
        };

        for l3_idx in 0..512 {
            if proc_l3[l3_idx].is_unused() {
                continue;
            }
            if proc_l3[l3_idx].flags().contains(PageTableFlags::HUGE_PAGE) {
                continue;
            }

            let proc_l2_addr = proc_l3[l3_idx].addr();
            let kernel_l2_addr = if !kernel_l3[l3_idx].is_unused() {
                kernel_l3[l3_idx].addr()
            } else {
                PhysAddr::new(0)
            };

            // カーネルと共有している L2 ならスキップ
            if proc_l2_addr == kernel_l2_addr {
                continue;
            }

            // プロセス固有の L2 テーブルの中身を走査して、分岐コピーされた L1 を解放
            let proc_l2: &PageTable = unsafe {
                &*(proc_l2_addr.as_u64() as *const PageTable)
            };
            let kernel_l2: &PageTable = if !kernel_l3[l3_idx].is_unused()
                && !kernel_l3[l3_idx].flags().contains(PageTableFlags::HUGE_PAGE)
            {
                unsafe { &*(kernel_l2_addr.as_u64() as *const PageTable) }
            } else {
                // カーネルに L2 がない → L2 テーブルだけ解放
                let frame = PhysFrame::<Size4KiB>::containing_address(proc_l2_addr);
                let mut fa = FRAME_ALLOCATOR.lock();
                unsafe { fa.deallocate_frame(frame); }
                continue;
            };

            for l2_idx in 0..512 {
                if proc_l2[l2_idx].is_unused() {
                    continue;
                }
                if proc_l2[l2_idx].flags().contains(PageTableFlags::HUGE_PAGE) {
                    continue;
                }

                let proc_l1_addr = proc_l2[l2_idx].addr();
                let kernel_l1_addr = if !kernel_l2[l2_idx].is_unused()
                    && !kernel_l2[l2_idx].flags().contains(PageTableFlags::HUGE_PAGE)
                {
                    kernel_l2[l2_idx].addr()
                } else {
                    PhysAddr::new(0)
                };

                // カーネルと共有している L1 ならスキップ
                if proc_l1_addr == kernel_l1_addr {
                    continue;
                }

                // プロセス固有の L1 テーブルを解放
                let frame = PhysFrame::<Size4KiB>::containing_address(proc_l1_addr);
                let mut fa = FRAME_ALLOCATOR.lock();
                unsafe { fa.deallocate_frame(frame); }
            }

            // プロセス固有の L2 テーブルを解放
            let frame = PhysFrame::<Size4KiB>::containing_address(proc_l2_addr);
            let mut fa = FRAME_ALLOCATOR.lock();
            unsafe { fa.deallocate_frame(frame); }
        }

        // プロセス固有の L3 テーブルを解放
        let frame = PhysFrame::<Size4KiB>::containing_address(proc_l3_addr);
        let mut fa = FRAME_ALLOCATOR.lock();
        unsafe { fa.deallocate_frame(frame); }
    }

    // 最後に L4 テーブル自体を解放
    {
        let mut fa = FRAME_ALLOCATOR.lock();
        unsafe { fa.deallocate_frame(process_l4_frame); }
    }
}

/// プロセスのページテーブルで仮想アドレスを物理アドレスに変換する（デバッグ用）。
#[allow(dead_code)]
///
/// カーネルの CR3 を切り替えずに、プロセスのページテーブルを手動で辿って
/// 仮想→物理の変換を行う。
pub fn translate_in_process(
    process_l4_frame: PhysFrame<Size4KiB>,
    virt: VirtAddr,
) -> Option<PhysAddr> {
    let addr = virt.as_u64();
    let l4_idx = ((addr >> 39) & 0x1FF) as usize;
    let l3_idx = ((addr >> 30) & 0x1FF) as usize;
    let l2_idx = ((addr >> 21) & 0x1FF) as usize;
    let l1_idx = ((addr >> 12) & 0x1FF) as usize;
    let page_offset = addr & 0xFFF;

    let l4: &PageTable = unsafe {
        &*(process_l4_frame.start_address().as_u64() as *const PageTable)
    };
    let l4_entry = &l4[l4_idx];
    if l4_entry.is_unused() { return None; }

    let l3: &PageTable = unsafe { &*(l4_entry.addr().as_u64() as *const PageTable) };
    let l3_entry = &l3[l3_idx];
    if l3_entry.is_unused() { return None; }
    if l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        // 1GiB ページ
        return Some(PhysAddr::new(l3_entry.addr().as_u64() + (addr & 0x3FFFFFFF)));
    }

    let l2: &PageTable = unsafe { &*(l3_entry.addr().as_u64() as *const PageTable) };
    let l2_entry = &l2[l2_idx];
    if l2_entry.is_unused() { return None; }
    if l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        // 2MiB ページ
        return Some(PhysAddr::new(l2_entry.addr().as_u64() + (addr & 0x1FFFFF)));
    }

    let l1: &PageTable = unsafe { &*(l2_entry.addr().as_u64() as *const PageTable) };
    let l1_entry = &l1[l1_idx];
    if l1_entry.is_unused() { return None; }

    Some(PhysAddr::new(l1_entry.addr().as_u64() + page_offset))
}

/// CR3 レジスタをプロセスのページテーブルに切り替える。
///
/// カーネルマッピングは共有されているので、切り替え後もカーネルコードは
/// 正常に動作する。TLB は CR3 書き込み時に自動フラッシュされる。
///
/// # Safety
/// - process_l4_frame が有効なページテーブルを指していること
/// - カーネルマッピングが含まれていること
pub unsafe fn switch_to_process_page_table(process_l4_frame: PhysFrame<Size4KiB>) {
    unsafe {
        Cr3::write(process_l4_frame, Cr3Flags::empty());
    }
}

/// CR3 レジスタをカーネルのページテーブルに復帰する。
///
/// # Safety
/// - カーネルの CR3 が save_kernel_cr3() で保存済みであること
pub unsafe fn switch_to_kernel_page_table() {
    let kernel_l4 = PhysFrame::containing_address(kernel_cr3());
    unsafe {
        Cr3::write(kernel_l4, Cr3Flags::empty());
    }
}

// =================================================================
// ELF ローダー用: 新規物理フレームの確保とマッピング
// =================================================================
//
// ELF バイナリのセグメントをプロセスのアドレス空間にロードするには、
// 新しい物理フレームを確保してプロセスのページテーブルにマッピングする必要がある。
//
// 仮想アドレス 0x400000 はカーネルのアイデンティティマッピングに含まれている
// ことがある。その場合、プロセスのページテーブルはカーネルと中間テーブル
// (L3/L2/L1) を共有しているため、直接 L1 エントリを書き換えると
// カーネルのマッピングを壊してしまう。
//
// 解決策: set_user_accessible_in_process() と同じ「分岐コピー」パターンを使う。
// カーネルと共有している中間テーブルを検出し、新しいフレームにコピーしてから
// プロセス固有の変更（新しいデータフレームのマッピング）を行う。

/// プロセスのページテーブルに新しい物理フレームをマッピングする。
///
/// 指定した仮想アドレス範囲に対して:
///   1. 必要なページ数分の物理フレームを確保
///   2. プロセスの L4 → L3 → L2 → L1 テーブルを辿り（カーネルと共有なら分岐コピー）
///   3. L1 エントリに確保したフレームをマッピング
///   4. 全階層に PRESENT | WRITABLE | USER_ACCESSIBLE を設定
///
/// カーネルのアイデンティティマッピングに含まれるアドレス範囲でも、
/// 分岐コピーにより安全にプロセス固有のマッピングを作成できる。
///
/// # 引数
/// - `process_l4_frame`: プロセスの L4 ページテーブルフレーム
/// - `virt_start`: マッピング先の仮想アドレス（4KiB アラインに切り下げ）
/// - `size`: マッピングするサイズ（バイト）
/// - `previously_allocated`: 前回の呼び出しで既に確保済みのフレームリスト。
///   同じページに複数のセグメントが配置される場合、このフレームは再利用する。
///
/// # 戻り値
/// 確保した物理フレームのリスト。先頭が virt_start に対応し、以降は連続ページ。
/// ELF セグメントのフラグ（p_flags）からページテーブルフラグに変換する（W^X 適用）
///
/// W^X (Write XOR Execute) のルール:
/// - 実行可能セグメント（PF_X）→ WRITABLE なし・NO_EXECUTE なし
/// - 書き込み可能セグメント（PF_W）→ WRITABLE あり・NO_EXECUTE あり
/// - 読み取り専用セグメント → WRITABLE なし・NO_EXECUTE あり
///
/// ここでいう NX ビット（No-Execute）は、x86_64 のページテーブルエントリの
/// 最上位ビットで、セットすると「このページのコードは実行できない」という意味になる。
/// W^X と組み合わせることで、書き込み可能なページからコードを実行する攻撃を防ぐ。
pub fn elf_flags_to_page_flags(elf_flags: u32) -> PageTableFlags {
    const PF_X: u32 = 1; // 実行可能
    const PF_W: u32 = 2; // 書き込み可能

    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;

    // 書き込み可能セグメントなら WRITABLE を設定
    if (elf_flags & PF_W) != 0 {
        flags |= PageTableFlags::WRITABLE;
    }

    // 実行不可のセグメントなら NO_EXECUTE（NX ビット）を設定
    if (elf_flags & PF_X) == 0 {
        flags |= PageTableFlags::NO_EXECUTE;
    }

    flags
}

/// 同一ページを共有する複数セグメントのページフラグをマージする。
///
/// W^X の理想は「書き込み可能なページは実行不可、実行可能なページは書き込み不可」だが、
/// リンカがセグメントを 4KiB 境界にアラインしない場合、同一ページに異なる権限の
/// セグメントが含まれることがある。この場合、より広い権限（union）を適用する。
/// - WRITABLE: どちらか一方でも W なら書き込み可能にする
/// - NO_EXECUTE: 両方とも NX なら NX を維持、一方でも実行可能なら NX を外す
fn merge_page_flags(existing: PageTableFlags, new: PageTableFlags) -> PageTableFlags {
    let mut merged = existing;

    // WRITABLE: 新しいセグメントが書き込み可能なら追加
    if new.contains(PageTableFlags::WRITABLE) {
        merged |= PageTableFlags::WRITABLE;
    }

    // NO_EXECUTE: 新しいセグメントが実行可能（NX なし）なら NX を外す
    if !new.contains(PageTableFlags::NO_EXECUTE) {
        merged.remove(PageTableFlags::NO_EXECUTE);
    }

    merged
}

pub fn map_user_pages_in_process(
    process_l4_frame: PhysFrame<Size4KiB>,
    virt_start: VirtAddr,
    size: usize,
    previously_allocated: &[PhysFrame<Size4KiB>],
    elf_flags: u32,
) -> alloc::vec::Vec<PhysFrame<Size4KiB>> {
    if size == 0 {
        return alloc::vec::Vec::new();
    }

    // 開始アドレスを 4KiB 境界に切り下げ
    let start_addr = virt_start.as_u64() & !0xFFF;
    // 終了アドレス（切り上げ）
    let end_addr = (virt_start.as_u64() + size as u64 + 0xFFF) & !0xFFF;
    let page_count = ((end_addr - start_addr) / 4096) as usize;

    let mut allocated_frames = alloc::vec::Vec::with_capacity(page_count);

    // L4/L3/L2 の中間テーブルは常に WRITABLE + USER_ACCESSIBLE にする。
    // 中間テーブルは「通過するためのエントリ」なので制限しない。
    // W^X の制限は L1 エントリ（リーフ）にだけ適用する。
    let intermediate_flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE;

    // L1 エントリ用のフラグ（W^X 適用済み）
    let leaf_flags = elf_flags_to_page_flags(elf_flags);

    // カーネルの L4 テーブル（分岐コピーの判定に使う）
    let kernel_l4: &PageTable = unsafe {
        &*(kernel_cr3().as_u64() as *const PageTable)
    };

    let process_l4: &mut PageTable = unsafe {
        &mut *(process_l4_frame.start_address().as_u64() as *mut PageTable)
    };

    let mut addr = start_addr;
    for _ in 0..page_count {
        let l4_idx = ((addr >> 39) & 0x1FF) as usize;
        let l3_idx = ((addr >> 30) & 0x1FF) as usize;
        let l2_idx = ((addr >> 21) & 0x1FF) as usize;
        let l1_idx = ((addr >> 12) & 0x1FF) as usize;

        // === L4 → L3 ===
        let l4_entry = &mut process_l4[l4_idx];
        if l4_entry.is_unused() {
            // L4 エントリが空 → 新しい L3 テーブルを作成
            let new_l3_frame = alloc_zeroed_frame();
            l4_entry.set_addr(new_l3_frame.start_address(), intermediate_flags);
        } else {
            // カーネルと同じ L3 を指していたら分岐コピー
            if !kernel_l4[l4_idx].is_unused()
                && l4_entry.addr() == kernel_l4[l4_idx].addr()
            {
                let new_l3_frame = fork_page_table(l4_entry.addr());
                l4_entry.set_addr(new_l3_frame.start_address(), l4_entry.flags() | intermediate_flags);
            } else {
                l4_entry.set_flags(l4_entry.flags() | intermediate_flags);
            }
        }

        let l3_table: &mut PageTable = unsafe {
            &mut *(l4_entry.addr().as_u64() as *mut PageTable)
        };

        // === L3 → L2 ===
        let l3_entry = &mut l3_table[l3_idx];
        if l3_entry.is_unused() {
            let new_l2_frame = alloc_zeroed_frame();
            l3_entry.set_addr(new_l2_frame.start_address(), intermediate_flags);
        } else {
            // カーネルの L3 テーブルから対応する L2 アドレスを取得して分岐判定
            let kernel_l2_addr = get_kernel_subtable_addr(kernel_l4, l4_idx, l3_idx, None);
            if let Some(k_addr) = kernel_l2_addr {
                if l3_entry.addr() == k_addr {
                    // 2MiB 巨大ページの場合は分割してから分岐
                    if l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                        // 1GiB ページの分割は未対応（通常使われない）
                        panic!("map_user_pages_in_process: 1GiB 巨大ページの分割は未対応");
                    }
                    let new_l2_frame = fork_page_table(l3_entry.addr());
                    l3_entry.set_addr(new_l2_frame.start_address(), l3_entry.flags() | intermediate_flags);
                } else {
                    l3_entry.set_flags(l3_entry.flags() | intermediate_flags);
                }
            } else {
                l3_entry.set_flags(l3_entry.flags() | intermediate_flags);
            }
        }

        if l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            // 1GiB 巨大ページは通常使われない。念のためスキップ。
            addr += 4096;
            continue;
        }

        let l2_table: &mut PageTable = unsafe {
            &mut *(l3_entry.addr().as_u64() as *mut PageTable)
        };

        // === L2 → L1 ===
        let l2_entry = &mut l2_table[l2_idx];
        if l2_entry.is_unused() {
            let new_l1_frame = alloc_zeroed_frame();
            l2_entry.set_addr(new_l1_frame.start_address(), intermediate_flags);
        } else {
            // 2MiB 巨大ページの場合は 4KiB に分割する
            if l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                split_huge_page_for_process(l2_entry);
            }

            // カーネルの L2 テーブルから対応する L1 アドレスを取得して分岐判定
            let kernel_l1_addr = get_kernel_subtable_addr(kernel_l4, l4_idx, l3_idx, Some(l2_idx));
            if let Some(k_addr) = kernel_l1_addr {
                if l2_entry.addr() == k_addr {
                    let new_l1_frame = fork_page_table(l2_entry.addr());
                    l2_entry.set_addr(new_l1_frame.start_address(), l2_entry.flags() | intermediate_flags);
                } else {
                    l2_entry.set_flags(l2_entry.flags() | intermediate_flags);
                }
            } else {
                l2_entry.set_flags(l2_entry.flags() | intermediate_flags);
            }
        }

        let l1_table: &mut PageTable = unsafe {
            &mut *(l2_entry.addr().as_u64() as *mut PageTable)
        };

        // === L1 エントリにデータ用フレームをマッピング ===
        let l1_entry = &mut l1_table[l1_idx];

        // このプロセス用に既にマッピング済みのフレームがあるかチェック。
        // 同じページに複数の LOAD セグメントがまたがる場合、2回目以降は
        // 既存フレームを返す（データの上書きを避けるため）。
        // ただし、カーネルのアイデンティティマッピングのフレーム（分岐コピー後や
        // huge page split 後に残っている元のマッピング）は新しいフレームで置き換える。
        if !l1_entry.is_unused() {
            // 既にプロセス専用のフレームが設定されているか確認する。
            // 物理アドレスが allocated_frames に含まれ、かつ L1 エントリに
            // USER_ACCESSIBLE が設定されている場合のみ、前のセグメントで
            // 設定したものと判定する。
            //
            // USER_ACCESSIBLE のチェックが必要な理由:
            // 2MiB huge page を split すると、L1 エントリにアイデンティティ
            // マッピング（物理アドレス == 仮想アドレス）が設定される。
            // これらのエントリには USER_ACCESSIBLE がない。
            // もしバディアロケータが別のページのデータ用に、この
            // アイデンティティマッピングと同じ物理アドレスのフレームを
            // 返していた場合、物理アドレスだけで判定すると誤って
            // 「自分のフレーム」と判定してしまう。
            // USER_ACCESSIBLE も確認することで、split 由来の
            // カーネルマッピングと自分が設定したマッピングを区別する。
            let existing_phys = l1_entry.addr();
            let existing_has_user = l1_entry.flags().contains(PageTableFlags::USER_ACCESSIBLE);
            let is_our_frame = existing_has_user
                && (allocated_frames.iter().any(|f: &PhysFrame<Size4KiB>| f.start_address() == existing_phys)
                    || previously_allocated.iter().any(|f| f.start_address() == existing_phys));
            if is_our_frame {
                // 前のセグメントで確保済み → フレームは再利用するが、
                // フラグはマージする（W^X: 複数セグメントが同一ページを共有する場合、
                // より広い権限を適用する必要がある。例: rodata(R) と data(RW) が
                // 同じ 4KiB ページにまたがる場合、そのページは RW にする）
                let existing_flags = l1_entry.flags();
                let merged_flags = merge_page_flags(existing_flags, leaf_flags);
                if merged_flags != existing_flags {
                    l1_entry.set_flags(merged_flags);
                }
                let existing_frame = PhysFrame::<Size4KiB>::containing_address(existing_phys);
                allocated_frames.push(existing_frame);
                addr += 4096;
                continue;
            }
            // カーネルのマッピングが残っている → 新しいフレームで上書き
        }

        // 新しいフレームを確保してマッピング
        let data_frame = {
            let mut fa = FRAME_ALLOCATOR.lock();
            fa.allocate_frame()
                .expect("map_user_pages_in_process: データ用フレーム確保に失敗")
        };

        // フレームの内容をゼロクリア（BSS 用、データコピー前の初期状態）
        unsafe {
            let ptr = data_frame.start_address().as_u64() as *mut u8;
            core::ptr::write_bytes(ptr, 0, 4096);
        }

        l1_entry.set_addr(data_frame.start_address(), leaf_flags);
        allocated_frames.push(data_frame);

        addr += 4096;
    }

    allocated_frames
}

/// ゼロクリア済みの新しいフレームを確保する（ページテーブル用）。
fn alloc_zeroed_frame() -> PhysFrame<Size4KiB> {
    let frame = {
        let mut fa = FRAME_ALLOCATOR.lock();
        fa.allocate_frame()
            .expect("alloc_zeroed_frame: フレーム確保に失敗")
    };
    let table: &mut PageTable = unsafe {
        &mut *(frame.start_address().as_u64() as *mut PageTable)
    };
    for e in table.iter_mut() {
        e.set_unused();
    }
    frame
}

/// ページテーブルの内容を新しいフレームにコピーする（分岐コピー）。
///
/// 元のテーブルの全 512 エントリを新しいフレームにコピーして返す。
/// 元のテーブルは変更しない。
fn fork_page_table(src_addr: PhysAddr) -> PhysFrame<Size4KiB> {
    let new_frame = {
        let mut fa = FRAME_ALLOCATOR.lock();
        fa.allocate_frame()
            .expect("fork_page_table: フレーム確保に失敗")
    };
    let src: &PageTable = unsafe { &*(src_addr.as_u64() as *const PageTable) };
    let dst: &mut PageTable = unsafe {
        &mut *(new_frame.start_address().as_u64() as *mut PageTable)
    };
    for i in 0..512 {
        if !src[i].is_unused() {
            dst[i].set_addr(src[i].addr(), src[i].flags());
        } else {
            dst[i].set_unused();
        }
    }
    new_frame
}

/// カーネルのページテーブルから、指定インデックスのサブテーブルの物理アドレスを取得する。
///
/// l2_idx が None の場合: L4[l4_idx] → L3[l3_idx] の先のテーブルアドレスを返す。
/// l2_idx が Some の場合: L4[l4_idx] → L3[l3_idx] → L2[l2_idx] の先のアドレスを返す。
fn get_kernel_subtable_addr(
    kernel_l4: &PageTable,
    l4_idx: usize,
    l3_idx: usize,
    l2_idx: Option<usize>,
) -> Option<PhysAddr> {
    if kernel_l4[l4_idx].is_unused() || kernel_l4[l4_idx].flags().contains(PageTableFlags::HUGE_PAGE) {
        return None;
    }
    let kernel_l3: &PageTable = unsafe {
        &*(kernel_l4[l4_idx].addr().as_u64() as *const PageTable)
    };

    if l2_idx.is_none() {
        // L3[l3_idx] のアドレスを返す
        if kernel_l3[l3_idx].is_unused() {
            return None;
        }
        return Some(kernel_l3[l3_idx].addr());
    }

    // L2 まで辿る
    if kernel_l3[l3_idx].is_unused() || kernel_l3[l3_idx].flags().contains(PageTableFlags::HUGE_PAGE) {
        return None;
    }
    let kernel_l2: &PageTable = unsafe {
        &*(kernel_l3[l3_idx].addr().as_u64() as *const PageTable)
    };
    let l2_idx = l2_idx.unwrap();
    if kernel_l2[l2_idx].is_unused() || kernel_l2[l2_idx].flags().contains(PageTableFlags::HUGE_PAGE) {
        return None;
    }
    Some(kernel_l2[l2_idx].addr())
}

/// SYS_MMAP 用: プロセスのアドレス空間に匿名ページ（ゼロ初期化済み）をマッピングする。
///
/// map_user_pages_in_process() は ELF ロード用に特化しているが、
/// この関数は「空のページを動的に追加する」ためのもの。
///
/// - `process_l4_frame`: プロセスの L4 ページテーブル
/// - `virt_start`: マッピング先仮想アドレス（4KiB アラインされていること）
/// - `num_pages`: 確保するページ数
/// - `writable`: 書き込み可能にするか
///
/// 戻り値: 確保した物理フレームのリスト（プロセス終了時に解放するために使う）
pub fn map_anonymous_pages_in_process(
    process_l4_frame: PhysFrame<Size4KiB>,
    virt_start: VirtAddr,
    num_pages: usize,
    writable: bool,
) -> alloc::vec::Vec<PhysFrame<Size4KiB>> {
    if num_pages == 0 {
        return alloc::vec::Vec::new();
    }

    let mut allocated_frames = alloc::vec::Vec::with_capacity(num_pages);

    // 中間テーブル（L4/L3/L2）は常に WRITABLE + USER_ACCESSIBLE
    let intermediate_flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE;

    // L1 エントリ（リーフ）のフラグ: 書き込み可能 + 実行不可（W^X）
    let mut leaf_flags = PageTableFlags::PRESENT
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    if writable {
        leaf_flags |= PageTableFlags::WRITABLE;
    }

    // カーネルの L4 テーブル（分岐コピーの判定に使う）
    let kernel_l4: &PageTable = unsafe {
        &*(kernel_cr3().as_u64() as *const PageTable)
    };

    let process_l4: &mut PageTable = unsafe {
        &mut *(process_l4_frame.start_address().as_u64() as *mut PageTable)
    };

    let start_addr = virt_start.as_u64();

    for i in 0..num_pages {
        let addr = start_addr + (i as u64) * 4096;
        let l4_idx = ((addr >> 39) & 0x1FF) as usize;
        let l3_idx = ((addr >> 30) & 0x1FF) as usize;
        let l2_idx = ((addr >> 21) & 0x1FF) as usize;
        let l1_idx = ((addr >> 12) & 0x1FF) as usize;

        // === L4 → L3 ===
        let l4_entry = &mut process_l4[l4_idx];
        if l4_entry.is_unused() {
            let new_l3_frame = alloc_zeroed_frame();
            l4_entry.set_addr(new_l3_frame.start_address(), intermediate_flags);
        } else {
            if !kernel_l4[l4_idx].is_unused()
                && l4_entry.addr() == kernel_l4[l4_idx].addr()
            {
                let new_l3_frame = fork_page_table(l4_entry.addr());
                l4_entry.set_addr(new_l3_frame.start_address(), l4_entry.flags() | intermediate_flags);
            } else {
                l4_entry.set_flags(l4_entry.flags() | intermediate_flags);
            }
        }

        let l3_table: &mut PageTable = unsafe {
            &mut *(l4_entry.addr().as_u64() as *mut PageTable)
        };

        // === L3 → L2 ===
        let l3_entry = &mut l3_table[l3_idx];
        if l3_entry.is_unused() {
            let new_l2_frame = alloc_zeroed_frame();
            l3_entry.set_addr(new_l2_frame.start_address(), intermediate_flags);
        } else {
            // 1GiB 巨大ページの場合は 512 x 2MiB に分割してから処理
            if l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                split_1gib_huge_page_for_process(l3_entry);
            }

            let kernel_l2_addr = get_kernel_subtable_addr(kernel_l4, l4_idx, l3_idx, None);
            if let Some(k_addr) = kernel_l2_addr {
                if l3_entry.addr() == k_addr {
                    let new_l2_frame = fork_page_table(l3_entry.addr());
                    l3_entry.set_addr(new_l2_frame.start_address(), l3_entry.flags() | intermediate_flags);
                } else {
                    l3_entry.set_flags(l3_entry.flags() | intermediate_flags);
                }
            } else {
                l3_entry.set_flags(l3_entry.flags() | intermediate_flags);
            }
        }

        let l2_table: &mut PageTable = unsafe {
            &mut *(l3_entry.addr().as_u64() as *mut PageTable)
        };

        // === L2 → L1 ===
        let l2_entry = &mut l2_table[l2_idx];
        if l2_entry.is_unused() {
            let new_l1_frame = alloc_zeroed_frame();
            l2_entry.set_addr(new_l1_frame.start_address(), intermediate_flags);
        } else {
            if l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                split_huge_page_for_process(l2_entry);
            }
            let kernel_l1_addr = get_kernel_subtable_addr(kernel_l4, l4_idx, l3_idx, Some(l2_idx));
            if let Some(k_addr) = kernel_l1_addr {
                if l2_entry.addr() == k_addr {
                    let new_l1_frame = fork_page_table(l2_entry.addr());
                    l2_entry.set_addr(new_l1_frame.start_address(), l2_entry.flags() | intermediate_flags);
                } else {
                    l2_entry.set_flags(l2_entry.flags() | intermediate_flags);
                }
            } else {
                l2_entry.set_flags(l2_entry.flags() | intermediate_flags);
            }
        }

        let l1_table: &mut PageTable = unsafe {
            &mut *(l2_entry.addr().as_u64() as *mut PageTable)
        };

        // === L1 エントリにゼロクリア済みフレームをマッピング ===
        let l1_entry = &mut l1_table[l1_idx];

        // 新しいフレームを確保（ゼロクリア済み）
        let data_frame = {
            let mut fa = FRAME_ALLOCATOR.lock();
            fa.allocate_frame()
                .expect("map_anonymous_pages_in_process: フレーム確保に失敗")
        };

        // ゼロクリア
        unsafe {
            let ptr = data_frame.start_address().as_u64() as *mut u8;
            core::ptr::write_bytes(ptr, 0, 4096);
        }

        // L1 エントリが既に使用中の場合（分岐コピーによるアイデンティティマッピングの残骸など）、
        // 新しいフレームで上書きする。既存のフレームはカーネルのものなので解放しない。
        l1_entry.set_addr(data_frame.start_address(), leaf_flags);
        allocated_frames.push(data_frame);
    }

    // TLB をフラッシュ（新しいマッピングを有効にする）
    unsafe {
        core::arch::asm!("mov rax, cr3; mov cr3, rax", out("rax") _);
    }

    allocated_frames
}

/// SYS_MUNMAP 用: プロセスのアドレス空間からページのマッピングを解除する。
///
/// L1 エントリを unused にし、対応する物理フレームを解放する。
///
/// - `process_l4_frame`: プロセスの L4 ページテーブル
/// - `virt_start`: マッピング解除先の仮想アドレス（4KiB アラインされていること）
/// - `num_pages`: 解除するページ数
///
/// 戻り値: 解放された物理フレームのリスト（allocated_frames から除去するために使う）
pub fn unmap_pages_in_process(
    process_l4_frame: PhysFrame<Size4KiB>,
    virt_start: VirtAddr,
    num_pages: usize,
) -> alloc::vec::Vec<PhysFrame<Size4KiB>> {
    if num_pages == 0 {
        return alloc::vec::Vec::new();
    }

    let mut freed_frames = alloc::vec::Vec::with_capacity(num_pages);

    let process_l4: &mut PageTable = unsafe {
        &mut *(process_l4_frame.start_address().as_u64() as *mut PageTable)
    };

    let start_addr = virt_start.as_u64();

    for i in 0..num_pages {
        let addr = start_addr + (i as u64) * 4096;
        let l4_idx = ((addr >> 39) & 0x1FF) as usize;
        let l3_idx = ((addr >> 30) & 0x1FF) as usize;
        let l2_idx = ((addr >> 21) & 0x1FF) as usize;
        let l1_idx = ((addr >> 12) & 0x1FF) as usize;

        // L4 → L3
        let l4_entry = &process_l4[l4_idx];
        if l4_entry.is_unused() {
            continue;
        }

        let l3_table: &PageTable = unsafe {
            &*(l4_entry.addr().as_u64() as *const PageTable)
        };

        // L3 → L2
        let l3_entry = &l3_table[l3_idx];
        if l3_entry.is_unused() || l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            continue;
        }

        let l2_table: &PageTable = unsafe {
            &*(l3_entry.addr().as_u64() as *const PageTable)
        };

        // L2 → L1
        let l2_entry = &l2_table[l2_idx];
        if l2_entry.is_unused() || l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            continue;
        }

        let l1_table: &mut PageTable = unsafe {
            &mut *(l2_entry.addr().as_u64() as *mut PageTable)
        };

        // L1 エントリのマッピングを解除
        let l1_entry = &mut l1_table[l1_idx];
        if l1_entry.is_unused() {
            continue;
        }

        let frame = PhysFrame::<Size4KiB>::containing_address(l1_entry.addr());
        freed_frames.push(frame);

        // エントリを未使用にする
        l1_entry.set_unused();
    }

    // 物理フレームを解放
    {
        let mut fa = FRAME_ALLOCATOR.lock();
        for frame in &freed_frames {
            unsafe {
                fa.deallocate_frame(*frame);
            }
        }
    }

    // TLB をフラッシュ（マッピング解除を反映）
    unsafe {
        core::arch::asm!("mov rax, cr3; mov cr3, rax", out("rax") _);
    }

    freed_frames
}

