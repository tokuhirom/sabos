// slab_allocator.rs — サイズクラス別スラブアロケータ
//
// カーネルヒープのメモリ管理を行うスラブアロケータ。
// 従来の linked_list_allocator (O(n)) を置き換え、alloc/dealloc を O(1) に高速化する。
//
// ## 設計概要
//
// ヒープ領域を 7 つの固定サイズクラス（32B〜2048B）と、
// それ以上の大オブジェクト用領域に分割して管理する。
//
// 各スラブはフリーリスト + バンプポインタのハイブリッド方式:
// - 解放されたスロットはフリーリスト（intrusive linked list）で管理
// - 未使用領域はバンプポインタで切り出す
// - どちらも O(1) で動作する
//
// 大オブジェクト（> 2048B）は first-fit + バンプのハイブリッド方式で管理。

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use spin::Mutex;

// =============================================================================
// サイズクラスの定義
// =============================================================================

/// スラブのサイズクラス（バイト単位）
const SLAB_SIZES: [usize; 7] = [32, 64, 128, 256, 512, 1024, 2048];

/// 各サイズクラスに配分するヒープの「ユニット数」（16分割のうちいくつ割り当てるか）
/// 32B: 1, 64B: 1, 128B: 1, 256B: 1, 512B: 1, 1024B: 1, 2048B: 1 = 合計 7 ユニット
/// 残りの 9 ユニットは大オブジェクト用
///
/// 小さいサイズクラスのユニット数を抑え、大オブジェクトに多く配分する。
/// カーネル内では ELF ファイル読み込み（数百 KiB〜1 MiB）や virtqueue バッファなど
/// 大きなアロケーションが頻繁に発生するため。
const SLAB_UNITS: [usize; 7] = [1, 1, 1, 1, 1, 1, 1];

/// ヒープ全体を何分割するか
///
/// 各サイズクラスに 1 ユニットずつ（計 7）、残りは大オブジェクト用。
/// 32 分割にすることで各スラブのサイズを小さくし、大オブジェクトに多く配分する。
const TOTAL_UNITS: usize = 32;

// =============================================================================
// FreeNode — 解放済みスロットの intrusive linked list ノード
// =============================================================================

/// 解放済みスロットの先頭 8 バイトに埋め込むリンクリストノード。
/// スロットサイズが最低 32B なので、ポインタ（8B）は必ず収まる。
///
/// intrusive linked list とは、別途ノード用のメモリを確保するのではなく、
/// 解放済みメモリ自体にリンク情報を書き込む手法。
/// メモリオーバーヘッドがゼロで、アロケータの bootstrap 問題も回避できる。
#[repr(C)]
struct FreeNode {
    next: *mut FreeNode,
}

// =============================================================================
// Slab — 固定サイズクラスのスラブ
// =============================================================================

/// 1 つのサイズクラスを管理するスラブ。
/// 連続メモリ領域を固定サイズのスロットに分割し、フリーリスト + バンプポインタで管理する。
struct Slab {
    /// スロットのサイズ（バイト）。32, 64, 128, ... のいずれか
    slot_size: usize,
    /// この領域の先頭アドレス
    region_start: usize,
    /// この領域の末尾アドレス（排他的）
    region_end: usize,
    /// 解放済みスロットのフリーリスト（先頭ノード）
    /// alloc 時はここから pop、dealloc 時はここに push する
    free_list: *mut FreeNode,
    /// 次の未初期化スロットのアドレス（バンプポインタ）
    /// フリーリストが空の場合、ここから新しいスロットを切り出す
    next_uninit: usize,
    /// 確保中のスロット数（デバッグ・統計用）
    allocated_count: usize,
}

impl Slab {
    /// 新しいスラブを作成する。
    ///
    /// # 引数
    /// - `slot_size`: スロットのサイズ（バイト）
    /// - `region_start`: 領域の先頭アドレス
    /// - `region_size`: 領域のサイズ（バイト）
    fn new(slot_size: usize, region_start: usize, region_size: usize) -> Self {
        Slab {
            slot_size,
            region_start,
            region_end: region_start + region_size,
            free_list: ptr::null_mut(),
            next_uninit: region_start,
            allocated_count: 0,
        }
    }

    /// スロットを 1 つ確保する（O(1)）。
    ///
    /// 1. フリーリストが非空なら先頭を pop して返す
    /// 2. フリーリストが空ならバンプポインタから切り出す
    /// 3. どちらもなければ null（メモリ不足）
    fn alloc(&mut self) -> *mut u8 {
        // フリーリストから pop（O(1)）
        if !self.free_list.is_null() {
            let node = self.free_list;
            unsafe {
                self.free_list = (*node).next;
            }
            self.allocated_count += 1;
            return node as *mut u8;
        }

        // バンプポインタから切り出し（O(1)）
        if self.next_uninit + self.slot_size <= self.region_end {
            let ptr = self.next_uninit as *mut u8;
            self.next_uninit += self.slot_size;
            self.allocated_count += 1;
            return ptr;
        }

        // メモリ不足
        ptr::null_mut()
    }

    /// スロットを解放する（O(1)）。
    ///
    /// 解放されたスロットの先頭に FreeNode を書き込み、フリーリストに push する。
    ///
    /// # Safety
    /// - `ptr` はこのスラブから確保されたポインタであること
    /// - 二重解放しないこと
    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        let node = ptr as *mut FreeNode;
        unsafe {
            (*node).next = self.free_list;
        }
        self.free_list = node;
        self.allocated_count -= 1;
    }

    /// このスラブの管理領域にポインタが含まれるか判定する。
    fn contains(&self, ptr: *mut u8) -> bool {
        let addr = ptr as usize;
        addr >= self.region_start && addr < self.region_end
    }
}

// =============================================================================
// LargeFreeBlock — 大オブジェクト用のフリーブロック
// =============================================================================

/// 大オブジェクト（> 2048B）用のフリーブロック。
/// ブロックの先頭にサイズと次のブロックへのポインタを格納する。
#[repr(C)]
struct LargeFreeBlock {
    /// このフリーブロックのサイズ（ヘッダ含む）
    size: usize,
    /// 次のフリーブロックへのポインタ
    next: *mut LargeFreeBlock,
}

/// LargeFreeBlock のヘッダサイズ（size + next = 16 バイト）
const LARGE_HEADER_SIZE: usize = core::mem::size_of::<LargeFreeBlock>();

// =============================================================================
// LargeAllocator — 大オブジェクト用アロケータ
// =============================================================================

/// 2048B を超えるアロケーションを管理するアロケータ。
/// フリーリスト（first-fit）+ バンプポインタのハイブリッド方式。
struct LargeAllocator {
    /// 領域の先頭アドレス
    region_start: usize,
    /// 領域の末尾アドレス（排他的）
    region_end: usize,
    /// フリーブロックのリスト
    free_list: *mut LargeFreeBlock,
    /// バンプポインタ（未使用領域の先頭）
    next_uninit: usize,
}

impl LargeAllocator {
    /// 新しい LargeAllocator を作成する。
    fn new(region_start: usize, region_size: usize) -> Self {
        LargeAllocator {
            region_start,
            region_end: region_start + region_size,
            free_list: ptr::null_mut(),
            next_uninit: region_start,
        }
    }

    /// メモリを確保する。
    ///
    /// 1. フリーリストを first-fit で走査して適切なブロックを探す
    /// 2. 見つからなければバンプポインタから切り出す
    ///
    /// # 引数
    /// - `size`: 確保するサイズ（バイト）
    /// - `align`: アライメント要件
    fn alloc(&mut self, size: usize, align: usize) -> *mut u8 {
        // ブロックのサイズ: ヘッダ + ペイロード + アライメントパディング
        // ヘッダにサイズを記録し、ペイロード部分のポインタを返す
        let total_size = LARGE_HEADER_SIZE + size;

        // フリーリストから first-fit で探す
        let mut prev: *mut LargeFreeBlock = ptr::null_mut();
        let mut current = self.free_list;

        while !current.is_null() {
            unsafe {
                let block_addr = current as usize;
                let payload_addr = block_addr + LARGE_HEADER_SIZE;
                // ペイロードのアライメント調整
                let aligned_payload = align_up(payload_addr, align);
                let padding = aligned_payload - payload_addr;
                let needed = total_size + padding;

                if (*current).size >= needed {
                    // このブロックを使う — リストから外す
                    if prev.is_null() {
                        self.free_list = (*current).next;
                    } else {
                        (*prev).next = (*current).next;
                    }

                    // アライメント調整: ヘッダをペイロードの直前に配置する
                    let header_addr = aligned_payload - LARGE_HEADER_SIZE;
                    let header = header_addr as *mut LargeFreeBlock;
                    (*header).size = (*current).size;
                    (*header).next = ptr::null_mut();

                    return aligned_payload as *mut u8;
                }

                prev = current;
                current = (*current).next;
            }
        }

        // フリーリストに適切なブロックがなければバンプポインタから切り出す
        // まずアライメント調整
        let payload_start = align_up(self.next_uninit + LARGE_HEADER_SIZE, align);
        let header_addr = payload_start - LARGE_HEADER_SIZE;
        let end = payload_start + size;

        if end > self.region_end {
            return ptr::null_mut(); // メモリ不足
        }

        // ヘッダを書き込む
        unsafe {
            let header = header_addr as *mut LargeFreeBlock;
            (*header).size = end - header_addr;
            (*header).next = ptr::null_mut();
        }

        self.next_uninit = end;
        payload_start as *mut u8
    }

    /// メモリを解放する。
    ///
    /// ペイロードポインタからヘッダを逆算し、フリーリストにアドレス順で挿入する。
    /// 隣接するフリーブロックがあれば結合（coalescing）して断片化を軽減する。
    /// さらに、バンプポインタに隣接するフリーブロックがあればバンプポインタを巻き戻す。
    ///
    /// # Safety
    /// - `ptr` はこのアロケータから確保されたポインタであること
    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        let header_addr = (ptr as usize) - LARGE_HEADER_SIZE;
        let freed = header_addr as *mut LargeFreeBlock;
        let freed_addr = freed as usize;

        // アドレス順にフリーリストに挿入し、隣接ブロックを結合する
        // フリーリストはアドレスの昇順で維持する
        let mut prev: *mut LargeFreeBlock = ptr::null_mut();
        let mut current = self.free_list;

        // 挿入位置を探す（freed_addr より後のブロックの前に挿入）
        while !current.is_null() && (current as usize) < freed_addr {
            prev = current;
            unsafe { current = (*current).next; }
        }

        // freed を prev と current の間に挿入
        unsafe {
            (*freed).next = current;
        }

        if prev.is_null() {
            self.free_list = freed;
        } else {
            unsafe { (*prev).next = freed; }
        }

        // 後方結合: freed と current が隣接しているか
        if !current.is_null() {
            let freed_end = freed_addr + unsafe { (*freed).size };
            if freed_end == current as usize {
                unsafe {
                    (*freed).size += (*current).size;
                    (*freed).next = (*current).next;
                }
            }
        }

        // 前方結合: prev と freed が隣接しているか
        if !prev.is_null() {
            let prev_end = (prev as usize) + unsafe { (*prev).size };
            if prev_end == freed_addr {
                unsafe {
                    (*prev).size += (*freed).size;
                    (*prev).next = (*freed).next;
                }
            }
        }

        // バンプポインタ巻き戻し:
        // フリーリストの末尾ブロックがバンプポインタに隣接している場合、
        // そのブロックを削除してバンプポインタを巻き戻す。
        // これにより、解放された末尾の空間を再利用できる。
        self.try_shrink_bump();
    }

    /// フリーリスト末尾がバンプポインタに隣接していれば、
    /// バンプポインタを巻き戻してフリーリストから削除する。
    fn try_shrink_bump(&mut self) {
        // フリーリストを走査して、バンプポインタに隣接するブロックを探す
        let mut prev: *mut LargeFreeBlock = ptr::null_mut();
        let mut current = self.free_list;

        while !current.is_null() {
            let block_end = (current as usize) + unsafe { (*current).size };
            if block_end == self.next_uninit {
                // このブロックがバンプポインタに隣接 → 巻き戻し
                self.next_uninit = current as usize;
                if prev.is_null() {
                    self.free_list = unsafe { (*current).next };
                } else {
                    unsafe { (*prev).next = (*current).next; }
                }
                // 巻き戻し後、さらに別のブロックが隣接している可能性があるので再帰
                self.try_shrink_bump();
                return;
            }
            prev = current;
            current = unsafe { (*current).next };
        }
    }

    /// この領域にポインタが含まれるか判定する。
    fn contains(&self, ptr: *mut u8) -> bool {
        let addr = ptr as usize;
        addr >= self.region_start && addr < self.region_end
    }

    /// バンプポインタ直前のブロックを in-place で拡張する。
    ///
    /// Vec の倍々成長（realloc）で、古いバッファが末尾にある場合に
    /// コピーなしで領域を拡張できる。断片化を大幅に軽減する。
    ///
    /// # 戻り値
    /// - `true`: 拡張成功（ptr はそのまま使える）
    /// - `false`: 拡張失敗（ptr は末尾ブロックではないか、領域不足）
    fn try_grow_in_place(&mut self, ptr: *mut u8, old_size: usize, new_size: usize) -> bool {
        let header_addr = (ptr as usize) - LARGE_HEADER_SIZE;
        let block_end = header_addr + LARGE_HEADER_SIZE + old_size;

        // このブロックがバンプポインタの直前にあるか確認
        if block_end != self.next_uninit {
            return false;
        }

        // 拡張に必要な追加サイズ
        let additional = new_size - old_size;
        let new_end = self.next_uninit + additional;

        if new_end > self.region_end {
            return false; // 領域不足
        }

        // バンプポインタを進め、ヘッダのサイズを更新する
        self.next_uninit = new_end;
        unsafe {
            let header = header_addr as *mut LargeFreeBlock;
            (*header).size = LARGE_HEADER_SIZE + new_size;
        }
        true
    }
}

// =============================================================================
// SlabAllocator — スラブアロケータ本体
// =============================================================================

/// 7 つのスラブ + 大オブジェクト用アロケータを統合した
/// カーネルヒープアロケータ。
pub struct SlabAllocator {
    /// 固定サイズクラスのスラブ（32B, 64B, 128B, 256B, 512B, 1024B, 2048B）
    slabs: [Slab; 7],
    /// 大オブジェクト用アロケータ（> 2048B）
    large: LargeAllocator,
}

impl SlabAllocator {
    /// スラブアロケータを初期化する。
    ///
    /// ヒープ領域を 16 等分（ユニット）し、各サイズクラスと大オブジェクトに配分する。
    ///
    /// | クラス | ユニット数 | 16MiB 時のサイズ |
    /// |--------|-----------|----------------|
    /// | 32B    | 1         | 512 KiB        |
    /// | 64B    | 1         | 512 KiB        |
    /// | 128B   | 1         | 512 KiB        |
    /// | 256B   | 1         | 512 KiB        |
    /// | 512B   | 1         | 512 KiB        |
    /// | 1024B  | 1         | 512 KiB        |
    /// | 2048B  | 1         | 512 KiB        |
    /// | Large  | 25        | 12.5 MiB       |
    fn new(heap_start: usize, heap_size: usize) -> Self {
        let unit_size = heap_size / TOTAL_UNITS;
        let mut offset = heap_start;

        // 各スラブの領域を切り出す
        // Rust の配列初期化は const な値が必要なので、ダミーで初期化してから上書きする
        let mut slabs = [
            Slab::new(32, 0, 0),
            Slab::new(64, 0, 0),
            Slab::new(128, 0, 0),
            Slab::new(256, 0, 0),
            Slab::new(512, 0, 0),
            Slab::new(1024, 0, 0),
            Slab::new(2048, 0, 0),
        ];

        for i in 0..7 {
            let region_size = unit_size * SLAB_UNITS[i];
            slabs[i] = Slab::new(SLAB_SIZES[i], offset, region_size);
            offset += region_size;
        }

        // 残りを大オブジェクト用に割り当てる
        let large_size = heap_start + heap_size - offset;
        let large = LargeAllocator::new(offset, large_size);

        SlabAllocator { slabs, large }
    }

    /// メモリを確保する。
    ///
    /// Layout のサイズとアライメントからサイズクラスを決定し、適切なスラブに委譲する。
    /// スラブのサイズを超える場合は大オブジェクトアロケータに委譲する。
    fn alloc(&mut self, layout: Layout) -> *mut u8 {
        // effective_size = max(size, align) でサイズクラスを選択
        // 例: size=8, align=64 → 64B スラブを使う
        let effective_size = layout.size().max(layout.align());

        // 適切なサイズクラスを探す
        for slab in &mut self.slabs {
            if effective_size <= slab.slot_size {
                return slab.alloc();
            }
        }

        // どのスラブにも収まらなければ大オブジェクトアロケータに委譲
        self.large.alloc(layout.size(), layout.align())
    }

    /// メモリを解放する。
    ///
    /// ポインタがどのスラブ（または大オブジェクト領域）に属するかを判定し、
    /// 適切なアロケータに委譲する。
    ///
    /// # Safety
    /// - `ptr` はこのアロケータから確保されたポインタであること
    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        // どのスラブに属するか判定（高々 7 回の比較、O(1)）
        for slab in &mut self.slabs {
            if slab.contains(ptr) {
                unsafe { slab.dealloc(ptr) };
                return;
            }
        }

        // 大オブジェクト領域に属するか判定
        if self.large.contains(ptr) {
            unsafe { self.large.dealloc(ptr) };
            return;
        }

        // どこにも属さないポインタ → バグ。panic で知らせる
        panic!(
            "slab_allocator: dealloc of unknown pointer {:#x}",
            ptr as usize
        );
    }

    /// realloc: 可能なら in-place でブロックを拡張する。
    ///
    /// Large 領域のバンプ末尾ブロックを in-place で拡張できれば、
    /// コピーなしで完了する。それ以外は alloc + copy + dealloc。
    unsafe fn realloc(&mut self, ptr: *mut u8, old_layout: Layout, new_size: usize) -> *mut u8 {
        // 大きくなる場合のみ in-place 拡張を試みる
        if new_size > old_layout.size() && self.large.contains(ptr) {
            if self.large.try_grow_in_place(ptr, old_layout.size(), new_size) {
                return ptr;
            }
        }

        // in-place 拡張できなければ、新しい領域を確保してコピー
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, old_layout.align()) };
        let new_ptr = self.alloc(new_layout);
        if !new_ptr.is_null() {
            let copy_size = old_layout.size().min(new_size);
            unsafe {
                ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
                self.dealloc(ptr);
            }
        }
        new_ptr
    }
}

// =============================================================================
// LockedSlabAllocator — spin::Mutex で包んだグローバルアロケータ
// =============================================================================

/// グローバルアロケータとして使うための Mutex ラッパー。
///
/// `init()` 前は `None`、初期化後は `Some(SlabAllocator)` になる。
/// init 前に alloc が呼ばれた場合は null を返す（OOM ハンドラが panic する）。
pub struct LockedSlabAllocator {
    inner: Mutex<Option<SlabAllocator>>,
}

impl LockedSlabAllocator {
    /// 未初期化状態のアロケータを作成する。
    /// const fn なので static 変数の初期化に使える。
    pub const fn new() -> Self {
        LockedSlabAllocator {
            inner: Mutex::new(None),
        }
    }

    /// アロケータを初期化する。
    ///
    /// # 引数
    /// - `heap_start`: ヒープ領域の先頭アドレス
    /// - `heap_size`: ヒープ領域のサイズ（バイト）
    pub fn init(&self, heap_start: usize, heap_size: usize) {
        let mut inner = self.inner.lock();
        *inner = Some(SlabAllocator::new(heap_start, heap_size));
    }
}

/// GlobalAlloc トレイトの実装。
/// Rust の alloc crate（Vec, Box, String 等）がこのメソッドを呼んでメモリを管理する。
unsafe impl GlobalAlloc for LockedSlabAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut inner = self.inner.lock();
        match inner.as_mut() {
            Some(allocator) => allocator.alloc(layout),
            // 未初期化状態 → null を返す（OOM ハンドラが処理する）
            None => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let mut inner = self.inner.lock();
        match inner.as_mut() {
            Some(allocator) => unsafe { allocator.dealloc(ptr) },
            None => {
                // 未初期化状態で dealloc が呼ばれるのはバグ
                panic!("slab_allocator: dealloc called before init");
            }
        }
    }

    /// realloc のオーバーライド。
    ///
    /// デフォルトの realloc は alloc + copy + dealloc だが、
    /// Large 領域のバンプ末尾ブロックを in-place で拡張できれば
    /// コピーを回避してパフォーマンスと断片化耐性を大幅に改善する。
    /// Vec の倍々成長パターンで特に効果的。
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let mut inner = self.inner.lock();
        match inner.as_mut() {
            Some(allocator) => unsafe { allocator.realloc(ptr, layout, new_size) },
            None => ptr::null_mut(),
        }
    }
}

// Mutex の中身は Send + Sync を保証する必要がある。
// SlabAllocator は生ポインタを含むが、Mutex で排他制御しているので安全。
unsafe impl Send for SlabAllocator {}

// =============================================================================
// ユーティリティ関数
// =============================================================================

/// アドレスを指定アライメントに切り上げる。
///
/// 例: align_up(0x1001, 0x1000) = 0x2000
#[inline]
fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

// =============================================================================
// テスト用公開関数
// =============================================================================

/// スラブアロケータの内部状態をテストする。
///
/// selftest から呼ばれ、以下を検証する:
/// 1. 小さいアロケーション（各サイズクラス）の確保・解放・再確保
/// 2. アライメント要件のある確保
/// 3. 大オブジェクトの確保・解放
/// 4. 全サイズクラスの混合ストレステスト
pub fn test_slab_allocator() -> bool {
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    // === テスト 1: 小さいアロケーションの連続確保・解放・再確保 ===
    // 32B 以下のアロケーションを大量に確保し、解放後に再確保できるか検証
    {
        let mut boxes: Vec<Box<u64>> = Vec::new();
        for i in 0..100 {
            boxes.push(Box::new(i as u64));
        }
        // 値が正しいか確認
        for (i, b) in boxes.iter().enumerate() {
            if **b != i as u64 {
                crate::serial_println!("[slab_test] small alloc mismatch at {}", i);
                return false;
            }
        }
        // 全解放
        drop(boxes);

        // 再確保（解放されたスロットが再利用されるはず）
        let mut boxes2: Vec<Box<u64>> = Vec::new();
        for i in 0..100 {
            boxes2.push(Box::new(i as u64 + 1000));
        }
        for (i, b) in boxes2.iter().enumerate() {
            if **b != i as u64 + 1000 {
                crate::serial_println!("[slab_test] small realloc mismatch at {}", i);
                return false;
            }
        }
    }

    // === テスト 2: アライメント要件のある確保 ===
    // align=16, align=64, align=128 のアロケーションを確保し、アライメントを検証
    {
        // Layout::from_size_align を使って直接 alloc する
        let layouts = [
            Layout::from_size_align(8, 16).unwrap(),
            Layout::from_size_align(16, 64).unwrap(),
            Layout::from_size_align(32, 128).unwrap(),
            Layout::from_size_align(1, 256).unwrap(),
        ];

        for layout in &layouts {
            let ptr = unsafe { alloc::alloc::alloc(*layout) };
            if ptr.is_null() {
                crate::serial_println!(
                    "[slab_test] aligned alloc failed: size={} align={}",
                    layout.size(),
                    layout.align()
                );
                return false;
            }
            // アライメントチェック
            if (ptr as usize) % layout.align() != 0 {
                crate::serial_println!(
                    "[slab_test] alignment violation: ptr={:#x} align={}",
                    ptr as usize,
                    layout.align()
                );
                unsafe { alloc::alloc::dealloc(ptr, *layout) };
                return false;
            }
            unsafe { alloc::alloc::dealloc(ptr, *layout) };
        }
    }

    // === テスト 3: 大オブジェクトの確保・解放 ===
    // 4096B, 8192B, 16384B のアロケーションを確保し、書き込み・読み出しを検証
    {
        for &size in &[4096usize, 8192, 16384] {
            let mut v = vec![0u8; size];
            // 全バイトに書き込み
            for (i, byte) in v.iter_mut().enumerate() {
                *byte = (i & 0xFF) as u8;
            }
            // 読み返し
            for (i, byte) in v.iter().enumerate() {
                if *byte != (i & 0xFF) as u8 {
                    crate::serial_println!(
                        "[slab_test] large alloc data mismatch at offset {} (size={})",
                        i,
                        size
                    );
                    return false;
                }
            }
        }
    }

    // === テスト 4: 全サイズクラスの混合ストレステスト ===
    // 様々なサイズを交互に確保・解放して断片化耐性を確認
    {
        let sizes = [16, 48, 100, 200, 400, 800, 1500, 3000, 5000];
        let mut allocations: Vec<Vec<u8>> = Vec::new();

        // 各サイズで確保
        for &size in &sizes {
            let v = vec![0xAB_u8; size];
            allocations.push(v);
        }

        // 奇数番目を解放
        for i in (1..allocations.len()).step_by(2) {
            allocations[i] = Vec::new();
        }

        // 別サイズで再確保
        for i in (1..allocations.len()).step_by(2) {
            let new_size = sizes[i] * 2;
            allocations[i] = vec![0xCD_u8; new_size];
        }

        // 偶数番目の中身チェック
        for i in (0..allocations.len()).step_by(2) {
            if allocations[i].is_empty() {
                crate::serial_println!("[slab_test] mixed stress: empty at {}", i);
                return false;
            }
            if allocations[i][0] != 0xAB {
                crate::serial_println!(
                    "[slab_test] mixed stress: data mismatch at {} (expected 0xAB, got {:#x})",
                    i,
                    allocations[i][0]
                );
                return false;
            }
        }

        // 奇数番目の中身チェック
        for i in (1..allocations.len()).step_by(2) {
            if allocations[i].is_empty() {
                crate::serial_println!("[slab_test] mixed stress: empty at {}", i);
                return false;
            }
            if allocations[i][0] != 0xCD {
                crate::serial_println!(
                    "[slab_test] mixed stress: data mismatch at {} (expected 0xCD, got {:#x})",
                    i,
                    allocations[i][0]
                );
                return false;
            }
        }
    }

    true
}
