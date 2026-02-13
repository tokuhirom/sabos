// allocator_fat32d.rs — fat32d 用の大きめアロケータ
//
// fat32d はファイルの読み込みキャッシュを保持するため、
// 通常のユーザープロセスより大きなヒープを必要とする。
// READ_FILE_CHUNK で大容量 ELF バイナリ（最大 2-3 MiB）を
// メモリに読み込んでチャンク返送するため、十分なヒープが必要。
//
// linked_list_allocator (LockedHeap) を使用する。
// フリーリスト方式で alloc/dealloc の両方に対応する。

use linked_list_allocator::LockedHeap;
use core::alloc::Layout;
use core::arch::asm;

/// ヒープサイズ（8 MiB）
///
/// fat32d は ELF ファイル（debug ビルドで最大 2.4 MiB）を
/// 丸ごと読み込んでキャッシュするため、十分なヒープが必要。
/// FAT テーブル 2 つ（各 ~512 KiB）+ ファイルキャッシュ + 作業用バッファ。
/// linked_list_allocator は dealloc 対応なので、
/// ファイルキャッシュの差し替え時にメモリを再利用できる。
const HEAP_SIZE: usize = 8 * 1024 * 1024;

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
///
/// メモリ確保に失敗したらエラーメッセージを出力してプロセスを終了する。
///
/// NOTE: allocator モジュールは各バイナリから mod allocator; で取り込まれるため
/// crate::syscall を直接参照できない。そのためインラインアセンブリで
/// 直接システムコールを発行する。
#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    let msg = b"[OOM] alloc failed in fat32d process\n";
    unsafe {
        asm!(
            "int 0x80",
            in("rax") 1u64,         // SYS_WRITE
            in("rdi") msg.as_ptr(), // buf_ptr
            in("rsi") msg.len(),    // buf_len
            lateout("rax") _,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }

    let _ = layout;

    // プロセスを終了（SYS_EXIT = 60）
    unsafe {
        asm!(
            "int 0x80",
            in("rax") 60u64, // SYS_EXIT
            lateout("rax") _,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    loop {}
}
