// allocator.rs — ユーザー空間の簡易アロケータ
//
// no_std 環境で Vec や String を使うために、最低限のヒープを用意する。
// ここでは単純なバンプアロケータを使い、free は無視する。
// 学習用・単一プロセス前提の最小実装。

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

/// ヒープサイズ（256 KiB）
const HEAP_SIZE: usize = 256 * 1024;

#[repr(align(16))]
struct Heap {
    _data: [u8; HEAP_SIZE],
}

static mut HEAP: Heap = Heap { _data: [0; HEAP_SIZE] };
static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct BumpAllocator;

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size = layout.size();

        let start = &raw const HEAP as usize;
        let mut current = NEXT.load(Ordering::Relaxed);

        // アラインメント調整
        if current % align != 0 {
            current = (current + align - 1) & !(align - 1);
        }

        let new_end = current.saturating_add(size);
        if new_end > HEAP_SIZE {
            return core::ptr::null_mut();
        }

        NEXT.store(new_end, Ordering::Relaxed);
        (start + current) as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // free は無視（バンプアロケータなので再利用しない）
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator;

#[alloc_error_handler]
fn alloc_error(_layout: Layout) -> ! {
    loop {}
}
