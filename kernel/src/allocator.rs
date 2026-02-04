// allocator.rs — カーネルヒープアロケータ
//
// Rust で Vec, Box, String などの動的メモリ確保を使うには
// グローバルアロケータ (#[global_allocator]) が必要。
// no_std 環境にはデフォルトのアロケータがないので、自分で用意する。
//
// ここでは linked_list_allocator crate を使う。
// フリーリスト方式のアロケータで、解放されたメモリブロックを
// リンクリストで管理する。シンプルだが実用的。
//
// ヒープ領域は UEFI メモリマップの CONVENTIONAL 領域から確保する。
// もし確保に失敗した場合は、BSS の固定領域にフォールバックする。

use linked_list_allocator::LockedHeap;
use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned, MemoryType};

/// ヒープのサイズ（1 MiB）。
/// 当面はこれで十分。足りなくなったら増やすか、
/// メモリマップベースの動的確保に移行する。
const HEAP_SIZE_FALLBACK: usize = 4 * 1024 * 1024; // 4 MiB
const HEAP_SIZE_DEFAULT: u64 = 16 * 1024 * 1024; // 16 MiB
const HEAP_SIZE_MIN: u64 = 4 * 1024 * 1024; // 4 MiB

/// ヒープ用の静的メモリ領域。
/// BSS セクションに配置されるので、バイナリサイズには影響しない。
/// アラインメントを 16 バイトにしておく（x86_64 の SSE 要件）。
#[repr(align(16))]
struct HeapMemory {
    _data: [u8; HEAP_SIZE_FALLBACK],
}

static mut HEAP_MEMORY: HeapMemory = HeapMemory { _data: [0; HEAP_SIZE_FALLBACK] };

static mut HEAP_START: u64 = 0;
static mut HEAP_SIZE: u64 = 0;
static mut HEAP_FROM_CONVENTIONAL: bool = false;

/// グローバルアロケータ。
/// #[global_allocator] で指定すると、alloc crate（Vec, Box, String 等）が
/// このアロケータを使ってメモリを確保/解放する。
/// LockedHeap は内部で spin lock を使うので、割り込みハンドラからの
/// 同時アクセスにも（一応）安全。
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// ヒープアロケータを初期化する。
/// GDT/IDT の初期化後、alloc を使う前に呼ぶこと。
pub fn init(memory_map: &MemoryMapOwned) {
    if let Some((start, size)) = select_heap_region(memory_map) {
        unsafe {
            HEAP_START = start;
            HEAP_SIZE = size;
            HEAP_FROM_CONVENTIONAL = true;
            ALLOCATOR.lock().init(start as *mut u8, size as usize);
        }
        crate::kprintln!(
            "Heap region: {:#x} - {:#x} ({} MiB)",
            start,
            start + size,
            size / 1024 / 1024
        );
        return;
    }

    // フォールバック: BSS の固定領域
    let heap_start = &raw const HEAP_MEMORY as *const u8 as u64;
    unsafe {
        HEAP_START = heap_start;
        HEAP_SIZE = HEAP_SIZE_FALLBACK as u64;
        HEAP_FROM_CONVENTIONAL = false;
        ALLOCATOR
            .lock()
            .init(heap_start as *mut u8, HEAP_SIZE_FALLBACK);
    }
    crate::kprintln!(
        "Heap fallback: {:#x} - {:#x} ({} MiB)",
        heap_start,
        heap_start + HEAP_SIZE_FALLBACK as u64,
        HEAP_SIZE_FALLBACK / 1024 / 1024
    );
}

/// ヒープ領域を予約対象として返す。
/// CONVENTIONAL から切り出した場合のみ Some を返す。
pub fn heap_region_for_reserve() -> Option<(u64, u64)> {
    unsafe {
        if HEAP_FROM_CONVENTIONAL {
            Some((HEAP_START, HEAP_SIZE))
        } else {
            None
        }
    }
}

/// ヒープ開始アドレス（デバッグ用）
pub fn heap_start() -> u64 {
    unsafe { HEAP_START }
}

/// ヒープサイズ（バイト）
pub fn heap_size() -> u64 {
    unsafe { HEAP_SIZE }
}

/// ヒープが CONVENTIONAL 由来かどうか
pub fn heap_from_conventional() -> bool {
    unsafe { HEAP_FROM_CONVENTIONAL }
}

/// UEFI メモリマップからヒープ領域を選ぶ。
fn select_heap_region(memory_map: &MemoryMapOwned) -> Option<(u64, u64)> {
    let mut best_start = 0u64;
    let mut best_size = 0u64;

    for desc in memory_map.entries() {
        if desc.ty != MemoryType::CONVENTIONAL {
            continue;
        }

        let region_start = desc.phys_start;
        if region_start < 0x100000 {
            continue;
        }

        let region_size = desc.page_count * 4096;
        let candidate = core::cmp::min(HEAP_SIZE_DEFAULT, region_size / 2);
        if candidate < HEAP_SIZE_MIN {
            continue;
        }

        if candidate > best_size {
            best_size = candidate;
            best_start = region_start + region_size - candidate;
        }
    }

    if best_size == 0 {
        return None;
    }

    // 4KiB アライン
    let start = best_start & !0xfff;
    let size = best_size & !0xfff;
    Some((start, size))
}
