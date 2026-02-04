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
// ヒープ領域として BSS セクションに静的配列を確保する。
// BSS はゼロ初期化される領域で、バイナリのファイルサイズは増えない。
// 将来的にはメモリマップから大きな領域を確保する形に移行できるが、
// まずは 1MiB の固定サイズで始める。

use linked_list_allocator::LockedHeap;

/// ヒープのサイズ（1 MiB）。
/// 当面はこれで十分。足りなくなったら増やすか、
/// メモリマップベースの動的確保に移行する。
const HEAP_SIZE: usize = 4 * 1024 * 1024; // 4 MiB

/// ヒープ用の静的メモリ領域。
/// BSS セクションに配置されるので、バイナリサイズには影響しない。
/// アラインメントを 16 バイトにしておく（x86_64 の SSE 要件）。
#[repr(align(16))]
struct HeapMemory {
    _data: [u8; HEAP_SIZE],
}

static mut HEAP_MEMORY: HeapMemory = HeapMemory { _data: [0; HEAP_SIZE] };

/// グローバルアロケータ。
/// #[global_allocator] で指定すると、alloc crate（Vec, Box, String 等）が
/// このアロケータを使ってメモリを確保/解放する。
/// LockedHeap は内部で spin lock を使うので、割り込みハンドラからの
/// 同時アクセスにも（一応）安全。
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// ヒープアロケータを初期化する。
/// GDT/IDT の初期化後、alloc を使う前に呼ぶこと。
pub fn init() {
    // HEAP_MEMORY の先頭アドレスとサイズを渡してアロケータを初期化する。
    // &raw const を使うのは Rust 2024 edition で static mut への
    // 共有参照が禁止されたため。
    let heap_start = &raw const HEAP_MEMORY as *const u8 as usize;
    unsafe {
        ALLOCATOR.lock().init(heap_start as *mut u8, HEAP_SIZE);
    }
}
