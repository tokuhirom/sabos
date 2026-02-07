// memory.rs — 物理フレームアロケータ（バディ方式）
//
// バディアロケータは物理メモリを2のべき乗サイズのブロックで管理する。
// ブロックを分割（split）して小さな割り当てに対応し、
// 解放時に隣接ブロック（バディ）と合体（coalesce）して断片化を防ぐ。
//
// 以前のビットマップアロケータは1フレーム単位でしか割り当てできなかったが、
// バディアロケータは連続した物理フレームの割り当てが可能。
// DMA バッファや大きなページテーブルの確保に必要。
//
// 仕組み:
//   - order 0 = 1フレーム (4 KiB), order k = 2^k フレーム
//   - 各 order ごとにフリーリスト（空きブロックのリスト）を持つ
//   - 割り当て: 要求 order 以上のフリーリストからブロックを取り出し、
//     必要に応じて分割して残りを低い order のフリーリストに戻す
//   - 解放: ブロックをフリーリストに戻し、バディ（隣接する同サイズブロック）
//     が空いていれば合体して上の order に昇格。再帰的に繰り返す
//
// 物理メモリは複数の非連続な CONVENTIONAL 領域から構成されるため、
// 各リージョンごとに独立したバディアロケータを持つ設計にしている。
// バディの合体は同一リージョン内でのみ行われる。
//
// UEFI のメモリマップから CONVENTIONAL 領域（OS が自由に使える RAM）を
// 収集し、リージョンごとにバディアロケータを初期化する。
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

/// バディアロケータの最大 order。
/// order 0 = 1 フレーム (4 KiB)、order 10 = 1024 フレーム (4 MiB)。
/// MAX_ORDER = 10 なら最大ブロックは 2^10 * 4 KiB = 4 MiB。
const MAX_ORDER: usize = 10;

// =================================================================
// RegionBuddyAllocator — 1つの物理メモリ領域に対するバディアロケータ
// =================================================================

/// 1つの物理メモリ領域に対するバディアロケータ。
///
/// 物理メモリは複数の非連続な CONVENTIONAL 領域から成るため、
/// リージョンごとに独立したバディアロケータを持つ。
/// バディの合体は同一リージョン内でのみ行う（異なるリージョンにまたがれない）。
///
/// データ構造:
///   - free_lists[k]: order k (= 2^k フレーム) の空きブロックの物理アドレスリスト
///   - bitmap: 1ビット/フレーム (0 = 空き, 1 = 使用中)
///     統計、二重解放検出、reserve_range 後のフリーリスト再構築に使用
struct RegionBuddyAllocator {
    /// 物理開始アドレス（4KiB アライン済み）
    start: u64,
    /// この領域の 4KiB ページ数
    page_count: u64,
    /// order ごとのフリーリスト。
    /// free_lists[k] は order k（= 2^k フレーム）の空きブロックの物理アドレスリスト。
    /// Linux カーネルでは空きページ自体にリンクリストを埋め込むが、
    /// SABOS では分かりやすさを重視して Vec を使う。
    free_lists: [Vec<u64>; MAX_ORDER + 1],
    /// ビットマップ。1ビット = 1フレーム (0 = 空き, 1 = 使用中)。
    /// フリーリストとは独立にフレームの割り当て状態を追跡する。
    /// 用途:
    ///   - 統計情報（allocated_count の計算）
    ///   - 二重解放の検出
    ///   - reserve_range() 後のフリーリスト再構築
    bitmap: Vec<u64>,
}

impl RegionBuddyAllocator {
    /// 新しいリージョンバディアロケータを作成する。
    /// 全フレームを空きとして初期化し、フリーリストを構築する。
    fn new(start: u64, page_count: u64) -> Self {
        let bitmap_size = ((page_count + 63) / 64) as usize;
        let mut alloc = Self {
            start,
            page_count,
            // 各 order のフリーリストを空の Vec で初期化
            free_lists: core::array::from_fn(|_| Vec::new()),
            // 全ビット 0 = 全フレーム空き
            bitmap: vec![0u64; bitmap_size],
        };
        // ビットマップ（全て空き）からフリーリストを構築
        alloc.build_free_lists();
        alloc
    }

    // =================================================================
    // フリーリスト構築
    // =================================================================

    /// ビットマップの状態に基づいてフリーリストを（再）構築する。
    ///
    /// ビットマップの空きフレームを走査し、アライメント制約を守りながら
    /// 最大サイズのバディブロックにまとめてフリーリストに登録する。
    ///
    /// 例: 7 ページのリージョン（全て空き）→
    ///   - offset 0: order 2 (4ページ、4ページアライン)
    ///   - offset 4: order 1 (2ページ、2ページアライン)
    ///   - offset 6: order 0 (1ページ)
    ///
    /// init 時と reserve_range() 後に呼ばれる。
    /// 通常の allocate/deallocate ではフリーリストを差分更新するので、
    /// この関数は呼ばれない。
    fn build_free_lists(&mut self) {
        // 既存のフリーリストをクリア
        for list in &mut self.free_lists {
            list.clear();
        }

        // ビットマップを走査して空きブロックをフリーリストに登録
        let mut offset: u64 = 0;
        while offset < self.page_count {
            if self.is_frame_allocated(offset) {
                offset += 1;
                continue;
            }

            // この位置から始まる最大のバディブロックの order を求める
            let order = self.max_free_order_at(offset);
            let block_phys = self.start + offset * 4096;
            self.free_lists[order].push(block_phys);
            offset += 1u64 << order;
        }

        // フリーリストを反転して、pop() が最小アドレスのブロックを返すようにする。
        // build_free_lists はオフセット昇順でブロックを push するので、
        // 反転しないと pop() は最大アドレス（末尾）を返す。
        // 低アドレスから優先的に割り当てることで、以前のビットマップアロケータと
        // 同様の割り当てパターンになり、既存コードとの互換性を保つ。
        for list in &mut self.free_lists {
            list.reverse();
        }
    }

    /// 指定オフセットから始まる最大の空きバディブロックの order を求める。
    ///
    /// 3つの条件を同時に満たす最大の order を返す:
    ///   1. アライメント: offset が 2^order ページにアライン
    ///   2. サイズ: offset + 2^order <= page_count（リージョン内に収まる）
    ///   3. 全フレーム空き: ブロック内の全フレームが未割り当て
    ///
    /// 効率化: order を1つ上げるとき、追加で必要な上半分（2^order フレーム）
    /// だけをチェックする。下半分は前の order で確認済み。
    fn max_free_order_at(&self, offset: u64) -> usize {
        let mut order = 0;
        while order < MAX_ORDER {
            let next_order = order + 1;
            let next_size = 1u64 << next_order;

            // アライメントチェック: offset が 2^next_order にアラインされているか
            if offset % next_size != 0 {
                break;
            }

            // 境界チェック: リージョン内に収まるか
            if offset + next_size > self.page_count {
                break;
            }

            // 上半分の全フレームが空きか確認
            // （下半分は order で確認済みなので再チェック不要）
            let half = 1u64 << order;
            if !self.is_range_free(offset + half, half) {
                break;
            }

            order = next_order;
        }
        order
    }

    // =================================================================
    // アドレス判定
    // =================================================================

    /// 指定物理アドレスがこのリージョンに含まれるか判定する。
    fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.start + self.page_count * 4096
    }

    // =================================================================
    // ビットマップ操作
    // =================================================================

    /// 指定オフセットのフレームが使用中（ビット 1）かチェック。
    fn is_frame_allocated(&self, page_offset: u64) -> bool {
        let word = (page_offset / 64) as usize;
        let bit = (page_offset % 64) as u32;
        if word >= self.bitmap.len() {
            return true; // 範囲外は使用中扱い
        }
        (self.bitmap[word] >> bit) & 1 == 1
    }

    /// 指定オフセットのフレームを使用中にマーク（ビットを 1 にセット）。
    fn set_frame_allocated(&mut self, page_offset: u64) {
        let word = (page_offset / 64) as usize;
        let bit = (page_offset % 64) as u32;
        if word < self.bitmap.len() {
            self.bitmap[word] |= 1u64 << bit;
        }
    }

    /// 指定オフセットのフレームを空きにマーク（ビットを 0 にクリア）。
    fn set_frame_free(&mut self, page_offset: u64) {
        let word = (page_offset / 64) as usize;
        let bit = (page_offset % 64) as u32;
        if word < self.bitmap.len() {
            self.bitmap[word] &= !(1u64 << bit);
        }
    }

    /// 指定範囲の全フレームが空き（ビット 0）かチェック。
    ///
    /// ワード単位（64フレーム = 1 u64）で一括チェックすることで、
    /// 大きな範囲でも高速に判定する。
    fn is_range_free(&self, start_offset: u64, count: u64) -> bool {
        let end = start_offset + count;
        let mut offset = start_offset;

        // ワードアラインまで個別チェック
        while offset < end && offset % 64 != 0 {
            if self.is_frame_allocated(offset) {
                return false;
            }
            offset += 1;
        }

        // ワード単位で一括チェック（64フレーム分を 1 回の比較で判定）
        while offset + 64 <= end {
            let word_idx = (offset / 64) as usize;
            if word_idx < self.bitmap.len() && self.bitmap[word_idx] != 0 {
                return false;
            }
            offset += 64;
        }

        // 残りを個別チェック
        while offset < end {
            if self.is_frame_allocated(offset) {
                return false;
            }
            offset += 1;
        }

        true
    }

    /// 範囲のフレームをビットマップで使用中にマーク。
    fn mark_range_allocated(&mut self, start_offset: u64, count: u64) {
        for i in 0..count {
            self.set_frame_allocated(start_offset + i);
        }
    }

    /// 範囲のフレームをビットマップで空きにマーク。
    fn mark_range_free(&mut self, start_offset: u64, count: u64) {
        for i in 0..count {
            self.set_frame_free(start_offset + i);
        }
    }

    // =================================================================
    // バディ割り当て・解放
    // =================================================================

    /// 指定 order のブロックを割り当てる。
    ///
    /// アルゴリズム:
    ///   1. 要求 order 以上で空きがある最小の order j を探す
    ///   2. free_lists[j] からブロックを取り出す
    ///   3. j > order なら分割: 上半分を free_lists[j-1] に戻し、下半分をキープ
    ///      これを order まで繰り返す
    ///   4. ビットマップを更新して物理アドレスを返す
    ///
    /// 返り値は割り当てたブロックの物理アドレス。None なら空きなし。
    fn allocate(&mut self, order: usize) -> Option<u64> {
        // 要求 order 以上で空きがある最小の order j を探す
        let mut j = order;
        while j <= MAX_ORDER {
            if !self.free_lists[j].is_empty() {
                break;
            }
            j += 1;
        }

        if j > MAX_ORDER {
            return None; // このリージョンに十分な空きがない
        }

        // フリーリストからブロックを取り出す
        let block_addr = self.free_lists[j].pop().unwrap();

        // 要求 order まで分割する
        // order j のブロックを半分に分割: 下半分をキープ、上半分をフリーリストへ
        // これを order j-1, j-2, ... order まで繰り返す
        while j > order {
            j -= 1;
            // 上半分のバディの物理アドレス = ブロック先頭 + 半分のサイズ
            let buddy_addr = block_addr + ((1u64 << j) * 4096);
            self.free_lists[j].push(buddy_addr);
        }

        // ビットマップを更新（割り当てたフレームを使用中にマーク）
        let page_offset = (block_addr - self.start) / 4096;
        let frame_count = 1u64 << order;
        self.mark_range_allocated(page_offset, frame_count);

        Some(block_addr)
    }

    /// 指定 order のブロックを解放し、バディと合体を試みる。
    ///
    /// アルゴリズム:
    ///   1. ビットマップをクリア（空きにする）
    ///   2. バディを計算: offset XOR 2^order
    ///   3. バディが free_lists[order] にいれば取り出して合体、order+1 で繰り返し
    ///   4. 合体できなくなったら free_lists[current_order] に追加
    ///
    /// 返り値: true = 正常解放、false = 無効な解放（二重解放など）
    fn deallocate(&mut self, addr: u64, order: usize) -> bool {
        let page_offset = (addr - self.start) / 4096;

        // 二重解放チェック（先頭フレームが使用中であることを確認）
        if !self.is_frame_allocated(page_offset) {
            return false;
        }

        // ビットマップをクリア（空きにする）
        let frame_count = 1u64 << order;
        self.mark_range_free(page_offset, frame_count);

        // バディとの合体を試みる
        // current_offset: 現在のブロックのページオフセット
        // current_order: 現在のブロックの order
        let mut current_offset = page_offset;
        let mut current_order = order;

        while current_order < MAX_ORDER {
            let block_size = 1u64 << current_order;

            // バディのオフセットを計算（XOR で求まる）
            // 例: order 0 で offset=4 → バディは 4 XOR 1 = 5
            //     order 1 で offset=4 → バディは 4 XOR 2 = 6
            //     order 2 で offset=0 → バディは 0 XOR 4 = 4
            let buddy_offset = current_offset ^ block_size;

            // バディがリージョン内に収まるか確認
            if buddy_offset + block_size > self.page_count {
                break;
            }

            // バディがフリーリストに存在するか確認し、あれば取り出す
            // （線形探索だが、フリーリストは通常短いので実用上問題ない）
            let buddy_addr = self.start + buddy_offset * 4096;
            if let Some(pos) = self.free_lists[current_order]
                .iter()
                .position(|&a| a == buddy_addr)
            {
                // バディを取り出して合体
                self.free_lists[current_order].swap_remove(pos);
                // 合体後のブロックは小さい方のオフセットから始まる
                current_offset = current_offset.min(buddy_offset);
                current_order += 1;
            } else {
                // バディが空いていない（または別の order で分割されている）
                // → 合体終了
                break;
            }
        }

        // 最終的なブロック（合体済み or 合体なし）をフリーリストに追加
        let block_addr = self.start + current_offset * 4096;
        self.free_lists[current_order].push(block_addr);

        true
    }
}

// =================================================================
// BuddyFrameAllocator — 公開インターフェース
// =================================================================

/// バディ方式の物理フレームアロケータ。
///
/// 以前のビットマップアロケータからの改善点:
///   - 連続した物理フレームの割り当てが可能（order 指定）
///   - 解放時にバディと合体して断片化を自動的に軽減
///   - 分割・合体により、メモリ効率が向上
///
/// 各 CONVENTIONAL リージョンごとに独立したバディアロケータを持つ。
/// リージョンは開始アドレス昇順でソートされている。
pub struct BuddyFrameAllocator {
    /// リージョンごとのバディアロケータ（開始アドレス昇順）
    region_allocators: Vec<RegionBuddyAllocator>,
    /// 全フレーム数
    total_frames: u64,
    /// 現在使用中のフレーム数
    allocated_count: u64,
    /// 無効な解放の回数（デバッグ用。panic するので本来 0 であるべき）
    invalid_dealloc_count: u64,
}

impl BuddyFrameAllocator {
    /// 空のアロケータを作成する。init() で領域を設定するまで何も割り当てられない。
    pub fn new() -> Self {
        Self {
            region_allocators: Vec::new(),
            total_frames: 0,
            allocated_count: 0,
            invalid_dealloc_count: 0,
        }
    }

    /// UEFI メモリマップから取得した CONVENTIONAL 領域のリストで初期化する。
    ///
    /// 各リージョンごとにバディアロケータを作成し、
    /// 全メモリを空きとしてフリーリストを構築する。
    /// リージョンは開始アドレス昇順にソートされる。
    pub fn init(&mut self, mut regions: Vec<MemoryRegion>) {
        // リージョンを開始アドレス昇順にソートする。
        // UEFI メモリマップは通常ソート済みだが、保証はないので明示的にソートする。
        // find_region_index() の二分探索に必要。
        regions.sort_by_key(|r| r.start);

        // 全フレーム数を計算
        let total: u64 = regions.iter().map(|r| r.page_count).sum();

        // 各リージョンにバディアロケータを作成
        let region_allocators: Vec<RegionBuddyAllocator> = regions
            .into_iter()
            .map(|r| RegionBuddyAllocator::new(r.start, r.page_count))
            .collect();

        self.region_allocators = region_allocators;
        self.total_frames = total;
        self.allocated_count = 0;
        self.invalid_dealloc_count = 0;
    }

    // =================================================================
    // 統計情報（既存 API 互換）
    // =================================================================

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

    // =================================================================
    // 割り当て・解放
    // =================================================================

    /// 指定 order の連続フレームを割り当てる。
    ///
    /// order 0 = 1フレーム (4 KiB), order k = 2^k フレーム。
    /// 各リージョンを順番に試し、最初に空きが見つかったリージョンから割り当てる。
    ///
    /// 返り値は割り当てたブロックの先頭フレーム。None なら空きなし。
    pub fn allocate_order(&mut self, order: usize) -> Option<PhysFrame<Size4KiB>> {
        for region in &mut self.region_allocators {
            if let Some(addr) = region.allocate(order) {
                self.allocated_count += 1u64 << order;
                return Some(PhysFrame::containing_address(PhysAddr::new(addr)));
            }
        }
        None
    }

    /// 指定 order の連続フレームを解放する。
    ///
    /// # Safety
    /// 解放するフレームが実際に指定 order で割り当て済みであること。
    /// 解放後にそのフレームを参照しているマッピングがないこと。
    pub unsafe fn deallocate_order(&mut self, frame: PhysFrame<Size4KiB>, order: usize) {
        let addr = frame.start_address().as_u64();

        // リージョンを二分探索で特定（開始アドレス昇順ソート済み）
        let pos = self.region_allocators.partition_point(|r| r.start <= addr);
        if pos > 0 {
            let idx = pos - 1;
            if self.region_allocators[idx].contains(addr) {
                if self.region_allocators[idx].deallocate(addr, order) {
                    self.allocated_count -= 1u64 << order;
                    return;
                } else {
                    // 二重解放や未割り当て解放はバグ
                    panic!(
                        "[memory] double-free or invalid dealloc detected (phys={:#x})",
                        addr
                    );
                }
            }
        }

        // どのリージョンにも属さないアドレスの解放はバグ
        panic!(
            "[memory] dealloc of unknown physical address (phys={:#x})",
            addr
        );
    }

    /// 物理フレーム1つを解放する（order 0 の deallocate_order のラッパー）。
    ///
    /// # Safety
    /// 解放するフレームが実際に割り当て済みであること。
    /// 解放後にそのフレームを参照しているマッピングがないこと。
    pub unsafe fn deallocate_frame(&mut self, frame: PhysFrame<Size4KiB>) {
        unsafe { self.deallocate_order(frame, 0); }
    }

    /// 指定範囲のフレームを予約済みにする（ヒープ領域の確保など）。
    ///
    /// ビットマップでフレームを使用中にマークした後、
    /// 影響を受けたリージョンのフリーリストを再構築する。
    /// ブート時に1回だけ呼ばれる想定。
    pub fn reserve_range(&mut self, start: u64, size: u64) {
        if size == 0 {
            return;
        }

        // ページ境界にアライン
        let start_aligned = start & !0xfff;
        let end_aligned = (start + size + 0xfff) & !0xfff;

        // 影響を受けたリージョンを追跡（フリーリスト再構築用）
        let mut affected = vec![false; self.region_allocators.len()];
        let mut addr = start_aligned;

        while addr < end_aligned {
            // リージョンを二分探索で特定
            let pos = self.region_allocators.partition_point(|r| r.start <= addr);
            if pos > 0 {
                let idx = pos - 1;
                let region = &mut self.region_allocators[idx];
                if region.contains(addr) {
                    let page_offset = (addr - region.start) / 4096;
                    if !region.is_frame_allocated(page_offset) {
                        region.set_frame_allocated(page_offset);
                        self.allocated_count += 1;
                    }
                    affected[idx] = true;
                }
            }
            addr += 4096;
        }

        // 影響を受けたリージョンのフリーリストを再構築する。
        // reserve_range によってビットマップが変わったので、
        // フリーリストをビットマップと整合させる必要がある。
        for (idx, was_affected) in affected.iter().enumerate() {
            if *was_affected {
                self.region_allocators[idx].build_free_lists();
            }
        }
    }
}

/// FrameAllocator トレイトの実装。
/// x86_64 crate のページテーブル操作（map_to 等）がこのトレイトを要求する。
///
/// allocate_frame() は order 0（1フレーム = 4 KiB）の割り当てとして実装。
///
/// # Safety
/// このアロケータは同じフレームを二度返さないことを保証する
/// （ビットマップ + フリーリストで使用中フラグを管理しているため）。
unsafe impl FrameAllocator<Size4KiB> for BuddyFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        self.allocate_order(0)
    }
}

lazy_static! {
    /// グローバルフレームアロケータ。
    /// ページテーブル操作時にフレームを確保するために使う。
    /// ロック順序: PAGE_TABLE → FRAME_ALLOCATOR（デッドロック防止のため必ず守ること）
    pub static ref FRAME_ALLOCATOR: Mutex<BuddyFrameAllocator> =
        Mutex::new(BuddyFrameAllocator::new());
}

/// フレームアロケータを初期化する。
/// UEFI メモリマップの CONVENTIONAL 領域を収集してアロケータに渡す。
/// 1MiB 以下の低メモリ領域はスキップする（レガシーハードウェアが使う可能性があるため）。
pub fn init(regions: Vec<MemoryRegion>) {
    FRAME_ALLOCATOR.lock().init(regions);
}

/// 指定範囲のフレームを予約する（ヒープ等の除外）。
pub fn reserve_range(start: u64, size: u64) {
    FRAME_ALLOCATOR.lock().reserve_range(start, size);
}
