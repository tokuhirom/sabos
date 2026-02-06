// allocator.rs — ユーザー空間のヒープアロケータ
//
// linked_list_allocator (LockedHeap) を使用する。
// フリーリスト方式で alloc/dealloc の両方に対応する。
// カーネル側と同じクレートを使い、パターンを統一する。
//
// Boa (JavaScript エンジン) など GC を持つプログラムでは
// 頻繁に alloc/dealloc が発生するため、バンプアロケータでは不十分。
// linked_list_allocator はフリーリストで解放メモリを再利用する。

use linked_list_allocator::LockedHeap;
use core::alloc::Layout;

/// ヒープサイズ（1 MiB）
/// 通常のユーザープロセス向け。GUI は allocator_gui.rs で別サイズを使う。
/// linked_list_allocator は dealloc 対応なので、メモリを再利用できる。
const HEAP_SIZE: usize = 1024 * 1024;

/// ヒープ用の静的メモリ領域。
/// BSS セクションに配置されるので、ELF バイナリサイズには影響しない。
/// アラインメントを 16 バイトにしておく（x86_64 の SSE 要件）。
#[repr(align(16))]
struct Heap {
    _data: [u8; HEAP_SIZE],
}

static mut HEAP: Heap = Heap {
    _data: [0; HEAP_SIZE],
};

/// グローバルアロケータ。
/// #[global_allocator] で指定すると、alloc crate（Vec, Box, String 等）が
/// このアロケータを使ってメモリを確保/解放する。
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// ヒープアロケータを初期化する。
/// _start() の最初に呼ぶこと（alloc を使う前に）。
///
/// LockedHeap::empty() で作った空のアロケータに、
/// 実際のヒープ領域のアドレスとサイズを教える。
pub fn init() {
    unsafe {
        let start = &raw const HEAP as *mut u8;
        ALLOCATOR.lock().init(start, HEAP_SIZE);
    }
}

/// alloc の OOM ハンドラ
/// メモリ確保に失敗したら無限ループで停止する。
#[alloc_error_handler]
fn alloc_error(_layout: Layout) -> ! {
    loop {}
}
