// memory.rs — 物理フレームアロケータ
//
// OS がページテーブルを操作するには、新しいページテーブル用の物理フレーム（4KiB）を
// 確保できる仕組みが必要。ここではバンプアロケータ（解放なし）を実装する。
//
// バンプアロケータは「次に返すフレームのインデックス」を進めるだけの
// 最もシンプルなアロケータ。解放ができないが、OS 初期段階には十分。
// 将来的にはビットマップアロケータ等に置き換えられる。
//
// UEFI のメモリマップから CONVENTIONAL 領域（OS が自由に使える RAM）を
// 収集し、そこからフレームを順番に払い出す。
// 1MiB 以下の低メモリ領域はレガシーハードウェアが使う可能性があるためスキップする。

use alloc::vec::Vec;
use lazy_static::lazy_static;
use spin::Mutex;
use x86_64::structures::paging::{FrameAllocator, PhysFrame, Size4KiB};
use x86_64::PhysAddr;

/// UEFI メモリマップの CONVENTIONAL 領域を表す構造体。
/// UEFI の MemoryDescriptor から必要な情報だけを抽出して保持する。
/// start は物理アドレス、page_count は 4KiB ページ数。
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    /// 領域の開始物理アドレス（4KiB アライン済み）
    pub start: u64,
    /// 領域に含まれる 4KiB ページの数
    pub page_count: u64,
}

/// バンプ方式の物理フレームアロケータ。
///
/// 仕組み:
///   1. CONVENTIONAL 領域のリストを保持
///   2. (region_index, frame_offset) で「次に払い出すフレーム」を追跡
///   3. allocate_frame() が呼ばれるたびにオフセットを +1 する
///   4. 現在のリージョンを使い切ったら次のリージョンに進む
///
/// 解放は一切しない（バンプ方式の特徴）。
/// フレームを返却したい場合はビットマップアロケータ等に置き換える必要がある。
pub struct BumpFrameAllocator {
    /// 使用可能な物理メモリ領域のリスト
    regions: Vec<MemoryRegion>,
    /// 現在払い出し中のリージョンのインデックス
    region_index: usize,
    /// 現在のリージョン内で次に払い出すフレームのオフセット
    frame_offset: u64,
    /// これまでに割り当てたフレームの総数（統計用）
    allocated_count: u64,
}

impl BumpFrameAllocator {
    /// 空のアロケータを作成する。init() で領域を設定するまで何も割り当てられない。
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
            region_index: 0,
            frame_offset: 0,
            allocated_count: 0,
        }
    }

    /// UEFI メモリマップから取得した CONVENTIONAL 領域のリストで初期化する。
    pub fn init(&mut self, regions: Vec<MemoryRegion>) {
        self.regions = regions;
        self.region_index = 0;
        self.frame_offset = 0;
        self.allocated_count = 0;
    }

    /// 使用可能な全フレーム数を返す。
    /// すべてのリージョンの page_count を合計した値。
    pub fn total_frames(&self) -> u64 {
        self.regions.iter().map(|r| r.page_count).sum()
    }

    /// これまでに割り当てたフレームの数を返す。
    pub fn allocated_count(&self) -> u64 {
        self.allocated_count
    }
}

/// FrameAllocator トレイトの実装。
/// x86_64 crate のページテーブル操作（map_to 等）がこのトレイトを要求する。
///
/// # Safety
/// このアロケータは同じフレームを二度返さないことを保証する（バンプ方式なので）。
unsafe impl FrameAllocator<Size4KiB> for BumpFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        // 現在のリージョンを使い切っていたら次のリージョンに進む
        while self.region_index < self.regions.len() {
            let region = &self.regions[self.region_index];

            if self.frame_offset < region.page_count {
                // このリージョン内にまだフレームがある
                let phys_addr = region.start + self.frame_offset * 4096;
                self.frame_offset += 1;
                self.allocated_count += 1;
                return Some(PhysFrame::containing_address(PhysAddr::new(phys_addr)));
            }

            // このリージョンを使い切った → 次のリージョンへ
            self.region_index += 1;
            self.frame_offset = 0;
        }

        // すべてのリージョンを使い切った
        None
    }
}

lazy_static! {
    /// グローバルフレームアロケータ。
    /// ページテーブル操作時にフレームを確保するために使う。
    /// ロック順序: PAGE_TABLE → FRAME_ALLOCATOR（デッドロック防止のため必ず守ること）
    pub static ref FRAME_ALLOCATOR: Mutex<BumpFrameAllocator> =
        Mutex::new(BumpFrameAllocator::new());
}

/// フレームアロケータを初期化する。
/// UEFI メモリマップの CONVENTIONAL 領域を収集してアロケータに渡す。
/// 1MiB 以下の低メモリ領域はスキップする（レガシーハードウェアが使う可能性があるため）。
pub fn init(regions: Vec<MemoryRegion>) {
    FRAME_ALLOCATOR.lock().init(regions);
}
