// memory.rs — 物理フレームアロケータ（ビットマップ方式）
//
// OS がページテーブルを操作するには、新しいページテーブル用の物理フレーム（4KiB）を
// 確保できる仕組みが必要。ここではビットマップアロケータを実装する。
//
// ビットマップアロケータは各フレームに 1 ビットを割り当てて、
// 0 = 空き、1 = 使用中 として管理する。
// バンプ方式と違ってフレームの解放（再利用）ができる。
//
// ビットマップは Vec<u64> で保持し、1 つの u64 で 64 フレーム分を管理する。
// 全フレーム数が N の場合、ビットマップのサイズは ceil(N / 64) 個の u64。
//
// UEFI のメモリマップから CONVENTIONAL 領域（OS が自由に使える RAM）を
// 収集し、フラットなインデックスに変換してビットマップで管理する。
// 1MiB 以下の低メモリ領域はレガシーハードウェアが使う可能性があるためスキップする。

use alloc::vec;
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

/// ビットマップ方式の物理フレームアロケータ。
///
/// 仕組み:
///   1. CONVENTIONAL 領域のリストを保持
///   2. 全領域のフレームをフラットなインデックスで管理
///      例: region0 が 100 フレーム、region1 が 200 フレームなら
///          index 0〜99 = region0、index 100〜299 = region1
///   3. ビットマップ (Vec<u64>) の各ビットが 1 フレームに対応
///      0 = 空き、1 = 使用中
///   4. allocate_frame(): 最初の空きビット (0) を探してセット (1) にする
///   5. deallocate_frame(): 指定フレームのビットをクリア (0) にする
///
/// バンプ方式と違ってフレームの解放・再利用が可能。
pub struct BitmapFrameAllocator {
    /// 使用可能な物理メモリ領域のリスト
    regions: Vec<MemoryRegion>,
    /// ビットマップ。1 ビット = 1 フレーム (0 = 空き, 1 = 使用中)。
    /// bitmap[i] の j ビット目が、フラットインデックス (i * 64 + j) に対応する。
    bitmap: Vec<u64>,
    /// 全フレーム数
    total_frames: u64,
    /// 現在使用中のフレーム数
    allocated_count: u64,
    /// 無効な解放の回数（デバッグ用）
    invalid_dealloc_count: u64,
    /// 次に探索を開始するビットマップのインデックス（検索高速化のヒント）
    /// 直前の割り当て位置の近くから探すことで、毎回先頭から探す無駄を減らす。
    next_search_hint: usize,
}

impl BitmapFrameAllocator {
    /// 空のアロケータを作成する。init() で領域を設定するまで何も割り当てられない。
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
            bitmap: Vec::new(),
            total_frames: 0,
            allocated_count: 0,
            invalid_dealloc_count: 0,
            next_search_hint: 0,
        }
    }

    /// UEFI メモリマップから取得した CONVENTIONAL 領域のリストで初期化する。
    /// ビットマップを作成し、全ビットを 0（空き）にする。
    pub fn init(&mut self, regions: Vec<MemoryRegion>) {
        // 全フレーム数を計算
        let total: u64 = regions.iter().map(|r| r.page_count).sum();

        // ビットマップのサイズ = ceil(total / 64)
        // 64 フレームごとに 1 つの u64 を使う
        let bitmap_size = ((total + 63) / 64) as usize;

        self.regions = regions;
        self.bitmap = vec![0u64; bitmap_size]; // 全ビット 0 = 全フレーム空き
        self.total_frames = total;
        self.allocated_count = 0;
        self.invalid_dealloc_count = 0;
        self.next_search_hint = 0;
    }

    /// 使用可能な全フレーム数を返す。
    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// 現在使用中のフレーム数を返す。
    pub fn allocated_count(&self) -> u64 {
        self.allocated_count
    }

    /// 無効な解放の回数を返す（デバッグ用）
    pub fn invalid_dealloc_count(&self) -> u64 {
        self.invalid_dealloc_count
    }

    /// 現在の空きフレーム数を返す。
    pub fn free_frames(&self) -> u64 {
        self.total_frames - self.allocated_count
    }

    /// フラットインデックスを物理アドレスに変換する。
    ///
    /// 各リージョンのフレーム数を順番に足していき、
    /// index がどのリージョンに属するか求めて物理アドレスを計算する。
    fn index_to_phys(&self, index: u64) -> Option<PhysAddr> {
        let mut offset = index;
        for region in &self.regions {
            if offset < region.page_count {
                // このリージョン内のフレーム
                return Some(PhysAddr::new(region.start + offset * 4096));
            }
            offset -= region.page_count;
        }
        // インデックスが範囲外
        None
    }

    /// 物理アドレスをフラットインデックスに変換する。
    ///
    /// 物理アドレスがどのリージョンに属するかを探し、
    /// そのリージョン内でのオフセット + それ以前のリージョンの累計フレーム数 = インデックス。
    fn phys_to_index(&self, addr: PhysAddr) -> Option<u64> {
        let addr = addr.as_u64();
        let mut base_index: u64 = 0;
        for region in &self.regions {
            let region_end = region.start + region.page_count * 4096;
            if addr >= region.start && addr < region_end {
                // このリージョンに属するフレーム
                let offset_in_region = (addr - region.start) / 4096;
                return Some(base_index + offset_in_region);
            }
            base_index += region.page_count;
        }
        // どのリージョンにも属さない
        None
    }

    /// ビットマップの指定インデックスのビットが立っているか確認する。
    fn is_allocated(&self, index: u64) -> bool {
        let word = (index / 64) as usize;
        let bit = (index % 64) as u32;
        if word >= self.bitmap.len() {
            return true; // 範囲外は使用中扱い
        }
        (self.bitmap[word] >> bit) & 1 == 1
    }

    /// ビットマップの指定インデックスのビットをクリアする（空きにする）。
    fn set_free(&mut self, index: u64) {
        let word = (index / 64) as usize;
        let bit = (index % 64) as u32;
        if word < self.bitmap.len() {
            self.bitmap[word] &= !(1u64 << bit);
        }
    }

    /// 物理フレームを解放する。
    ///
    /// 指定されたフレームの物理アドレスに対応するビットマップのビットをクリアして、
    /// そのフレームを再利用可能にする。
    ///
    /// # Safety
    /// 解放するフレームが実際に割り当て済みであること。
    /// 解放後にそのフレームを参照しているマッピングがないこと。
    pub unsafe fn deallocate_frame(&mut self, frame: PhysFrame<Size4KiB>) {
        let addr = frame.start_address();
        if let Some(index) = self.phys_to_index(addr) {
            if self.is_allocated(index) {
                self.set_free(index);
                self.allocated_count -= 1;
                // ヒントを更新: 解放したフレームの近くから次の検索を始める
                let word = (index / 64) as usize;
                if word < self.next_search_hint {
                    self.next_search_hint = word;
                }
            } else {
                // 二重解放や未割り当て解放を検出（panic せずカウント）
                self.invalid_dealloc_count += 1;
                crate::serial_println!(
                    "[memory] WARNING: invalid dealloc (phys={:#x})",
                    addr.as_u64()
                );
            }
        } else {
            self.invalid_dealloc_count += 1;
            crate::serial_println!(
                "[memory] WARNING: invalid dealloc (phys={:#x})",
                addr.as_u64()
            );
        }
    }
}

/// FrameAllocator トレイトの実装。
/// x86_64 crate のページテーブル操作（map_to 等）がこのトレイトを要求する。
///
/// # Safety
/// このアロケータは同じフレームを二度返さないことを保証する
/// （ビットマップで使用中フラグを管理しているため）。
unsafe impl FrameAllocator<Size4KiB> for BitmapFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let bitmap_len = self.bitmap.len();
        if bitmap_len == 0 {
            return None;
        }

        // next_search_hint から始めて、空きビットを探す。
        // 一周しても見つからなければ None を返す。
        //
        // 各 u64 が 0xFFFF_FFFF_FFFF_FFFF（全ビット 1）なら全フレーム使用中なので
        // スキップできる。これにより大量の使用中フレームを一気に飛ばせる。
        for i in 0..bitmap_len {
            let word_index = (self.next_search_hint + i) % bitmap_len;
            let word = self.bitmap[word_index];

            // 全ビットが 1 なら、この 64 フレームはすべて使用中 → スキップ
            if word == u64::MAX {
                continue;
            }

            // 空きビット（0）を探す。
            // !word で反転して trailing_zeros で最初の 1（= 元の最初の 0）の位置を求める。
            let bit = (!word).trailing_zeros();

            // ビットマップの範囲内か確認
            let flat_index = word_index as u64 * 64 + bit as u64;
            if flat_index >= self.total_frames {
                // ビットマップの最後の word で、total_frames を超えた部分
                continue;
            }

            // ビットをセットして使用中にする
            self.bitmap[word_index] |= 1u64 << bit;
            self.allocated_count += 1;

            // ヒントを更新: 次回はこの word から探す
            self.next_search_hint = word_index;

            // フラットインデックスを物理アドレスに変換
            if let Some(phys_addr) = self.index_to_phys(flat_index) {
                return Some(PhysFrame::containing_address(phys_addr));
            }
        }

        // すべてのフレームが使用中
        None
    }
}

lazy_static! {
    /// グローバルフレームアロケータ。
    /// ページテーブル操作時にフレームを確保するために使う。
    /// ロック順序: PAGE_TABLE → FRAME_ALLOCATOR（デッドロック防止のため必ず守ること）
    pub static ref FRAME_ALLOCATOR: Mutex<BitmapFrameAllocator> =
        Mutex::new(BitmapFrameAllocator::new());
}

/// フレームアロケータを初期化する。
/// UEFI メモリマップの CONVENTIONAL 領域を収集してアロケータに渡す。
/// 1MiB 以下の低メモリ領域はスキップする（レガシーハードウェアが使う可能性があるため）。
pub fn init(regions: Vec<MemoryRegion>) {
    FRAME_ALLOCATOR.lock().init(regions);
}
