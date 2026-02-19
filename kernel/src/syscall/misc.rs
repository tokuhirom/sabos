// syscall/misc.rs — その他のシステムコール
//
// SYS_SELFTEST, SYS_HALT, SYS_MMAP/MUNMAP, SYS_GETRANDOM,
// SYS_SOUND_PLAY, SYS_THREAD_CREATE/EXIT/JOIN, SYS_FUTEX

use crate::user_ptr::SyscallError;
use super::user_slice_from_args;

// =================================================================
// テスト/デバッグ関連システムコール
// =================================================================

/// SYS_SELFTEST: カーネル selftest を実行する
///
/// 引数:
///   arg1: auto_exit フラグ（0 = 通常実行、1 = 完了後に ISA debug exit で QEMU を終了）
/// 戻り値: 0（成功）
pub(crate) fn sys_selftest(auto_exit: u64) -> Result<u64, SyscallError> {
    // selftest 中にタイマー割り込みやタスク切り替えが動くように有効化
    x86_64::instructions::interrupts::enable();
    crate::shell::run_selftest(auto_exit != 0);
    Ok(0)
}

// =================================================================
// システム制御関連システムコール
// =================================================================

/// SYS_HALT: システム停止
///
/// システムを停止する。この関数は戻らない。
/// 割り込みを無効化し、HLT 命令で CPU を停止する。
pub(crate) fn sys_halt() -> Result<u64, SyscallError> {
    crate::kprintln!("System halted.");
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}

// =================================================================
// SYS_GETRANDOM: ランダムバイト生成
// =================================================================

/// SYS_GETRANDOM: RDRAND 命令でランダムバイトを生成
///
/// x86_64 の RDRAND 命令を使って暗号学的に安全なランダムバイトを生成する。
/// RDRAND はハードウェア乱数生成器 (DRNG) を使うため、ソフトウェア PRNG より安全。
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ（書き込むバイト数）
///
/// 戻り値: 書き込んだバイト数
pub(crate) fn sys_getrandom(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();
    let len = buf.len();

    // 8 バイトずつ RDRAND で生成し、バッファに書き込む
    let mut offset = 0;
    while offset < len {
        let random_value: u64 = rdrand64()?;
        let bytes = random_value.to_le_bytes();
        let remaining = len - offset;
        let to_copy = remaining.min(8);
        buf[offset..offset + to_copy].copy_from_slice(&bytes[..to_copy]);
        offset += to_copy;
    }

    Ok(len as u64)
}

/// RDRAND 命令で 64 ビットのランダム値を取得する。
///
/// RDRAND が失敗する場合（エントロピー枯渇など）は最大 10 回リトライする。
/// それでも失敗した場合はエラーを返す。
fn rdrand64() -> Result<u64, SyscallError> {
    for _ in 0..10 {
        let mut value: u64;
        let success: u8;
        unsafe {
            core::arch::asm!(
                "rdrand {val}",
                "setc {ok}",
                val = out(reg) value,
                ok = out(reg_byte) success,
            );
        }
        if success != 0 {
            return Ok(value);
        }
    }
    // RDRAND が 10 回連続で失敗した場合（通常は起こらない）
    Err(SyscallError::NotSupported)
}

// SYS_MMAP / SYS_MUNMAP: 匿名ページの動的マッピング/解除
//
// ユーザー空間から動的にメモリを確保するためのシステムコール。
// POSIX の mmap(MAP_ANONYMOUS) に相当するが、ファイルマッピングは未対応。
// std の GlobalAlloc や、ユーザー空間のヒープ拡張に使う。

/// MMAP_PROT_READ: 読み取り可能
const MMAP_PROT_READ: u64 = 0x1;
/// MMAP_PROT_WRITE: 書き込み可能
const MMAP_PROT_WRITE: u64 = 0x2;
/// MMAP_FLAG_ANONYMOUS: 匿名マッピング（ファイルに紐付かない）
const MMAP_FLAG_ANONYMOUS: u64 = 0x1;

/// mmap 用の仮想アドレスの下限。
/// ELF の LOAD セグメント、ユーザースタック (0x2000000)、
/// およびカーネルのアイデンティティマッピング（物理 RAM 範囲）と
/// 重ならないように、十分に高いアドレスから割り当てる。
///
/// UEFI は物理メモリを 1GiB ヒュージページで identity mapping するため、
/// L4[0] の範囲（0x0 ～ 0x7F_FFFF_FFFF = 512GiB）には identity mapping の
/// ページテーブルエントリが存在する可能性がある。
/// そのため mmap 領域は L4[2] の範囲（1TiB 以降）に配置して衝突を回避する。
const MMAP_VADDR_BASE: u64 = 0x100_0000_0000; // 1 TiB
/// mmap 領域の上限。
const MMAP_VADDR_LIMIT: u64 = 0x200_0000_0000; // 2 TiB

/// SYS_MMAP: ユーザー空間に匿名ページをマッピングする。
///
/// 引数:
/// - arg1 (addr_hint): マッピング先仮想アドレスのヒント（0 ならカーネルが決定）
/// - arg2 (len): マッピングサイズ（バイト、4KiB にアラインされる）
/// - arg3 (prot): プロテクションフラグ（PROT_READ | PROT_WRITE）
/// - arg4 (flags): マッピングフラグ（MAP_ANONYMOUS のみ対応）
///
/// 戻り値: マッピングされた仮想アドレス
pub(crate) fn sys_mmap(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let addr_hint = arg1;
    let len = arg2;
    let prot = arg3;
    let flags = arg4;

    // len が 0 はエラー
    if len == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    // 現状は匿名マッピングのみ対応
    if (flags & MMAP_FLAG_ANONYMOUS) == 0 {
        return Err(SyscallError::NotSupported);
    }

    // prot の検証（最低限 READ は必要）
    if (prot & MMAP_PROT_READ) == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    let writable = (prot & MMAP_PROT_WRITE) != 0;

    // ページ数を計算（切り上げ）
    let num_pages = ((len + 4095) / 4096) as usize;

    // 現在のプロセスの L4 ページテーブルフレームを取得
    let l4_frame = crate::scheduler::current_task_page_table_frame()
        .ok_or(SyscallError::NotSupported)?; // カーネルタスクでは mmap 不可

    // マッピング先の仮想アドレスを決定する
    let virt_addr = if addr_hint != 0 {
        // ユーザーが指定したアドレスを使う（4KiB アラインに切り上げ）
        let aligned = (addr_hint + 4095) & !4095;
        if aligned < MMAP_VADDR_BASE || aligned + (num_pages as u64 * 4096) > MMAP_VADDR_LIMIT {
            return Err(SyscallError::InvalidAddress);
        }
        aligned
    } else {
        // カーネルが空き領域を探す
        find_free_mmap_region(l4_frame, num_pages)?
    };

    // ページをマッピング
    let allocated = crate::paging::map_anonymous_pages_in_process(
        l4_frame,
        x86_64::VirtAddr::new(virt_addr),
        num_pages,
        writable,
    );

    // 確保したフレームをプロセスの allocated_frames に追加
    // （プロセス終了時に自動で解放される）
    crate::scheduler::add_mmap_frames_to_current(&allocated);

    Ok(virt_addr)
}

/// SYS_MUNMAP: ユーザー空間のページマッピングを解除する。
///
/// 引数:
/// - arg1 (addr): マッピング解除する仮想アドレス（4KiB アライン必須）
/// - arg2 (len): 解除するサイズ（バイト）
///
/// 戻り値: 0（成功）
pub(crate) fn sys_munmap(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let addr = arg1;
    let len = arg2;

    // アドレスが 4KiB アラインされているか確認
    if (addr & 0xFFF) != 0 {
        return Err(SyscallError::MisalignedPointer);
    }

    if len == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    // mmap 領域の範囲チェック
    if addr < MMAP_VADDR_BASE || addr + len > MMAP_VADDR_LIMIT {
        return Err(SyscallError::InvalidAddress);
    }

    let num_pages = ((len + 4095) / 4096) as usize;

    let l4_frame = crate::scheduler::current_task_page_table_frame()
        .ok_or(SyscallError::NotSupported)?;

    // ページのマッピングを解除し、物理フレームを解放
    let freed = crate::paging::unmap_pages_in_process(
        l4_frame,
        x86_64::VirtAddr::new(addr),
        num_pages,
    );

    // プロセスの allocated_frames から削除
    crate::scheduler::remove_mmap_frames_from_current(&freed);

    Ok(0)
}

/// mmap 領域から空き仮想アドレスを探す。
///
/// MMAP_VADDR_BASE から MMAP_VADDR_LIMIT の間で、num_pages 分の連続した
/// 未マッピング領域を探す。単純な線形探索（first-fit）。
fn find_free_mmap_region(
    process_l4_frame: x86_64::structures::paging::PhysFrame<x86_64::structures::paging::Size4KiB>,
    num_pages: usize,
) -> Result<u64, SyscallError> {
    let required_bytes = num_pages as u64 * 4096;

    let process_l4: &x86_64::structures::paging::page_table::PageTable = unsafe {
        &*(process_l4_frame.start_address().as_u64()
            as *const x86_64::structures::paging::page_table::PageTable)
    };

    // MMAP_VADDR_BASE からページ単位で空きを探す
    let mut candidate = MMAP_VADDR_BASE;

    while candidate + required_bytes <= MMAP_VADDR_LIMIT {
        let mut all_free = true;

        for page_idx in 0..num_pages {
            let addr = candidate + (page_idx as u64) * 4096;
            if is_page_mapped(process_l4, addr) {
                // この位置は使用中 → 次の候補に進む
                candidate = addr + 4096;
                all_free = false;
                break;
            }
        }

        if all_free {
            return Ok(candidate);
        }
    }

    // 空き領域が見つからなかった
    Err(SyscallError::Other)
}

/// 指定した仮想アドレスがプロセスのページテーブルでマッピング済みかチェックする。
fn is_page_mapped(
    l4_table: &x86_64::structures::paging::page_table::PageTable,
    virt_addr: u64,
) -> bool {
    use x86_64::structures::paging::PageTableFlags;

    let l4_idx = ((virt_addr >> 39) & 0x1FF) as usize;
    let l3_idx = ((virt_addr >> 30) & 0x1FF) as usize;
    let l2_idx = ((virt_addr >> 21) & 0x1FF) as usize;
    let l1_idx = ((virt_addr >> 12) & 0x1FF) as usize;

    let l4_entry = &l4_table[l4_idx];
    if l4_entry.is_unused() {
        return false;
    }

    let l3_table: &x86_64::structures::paging::page_table::PageTable = unsafe {
        &*(l4_entry.addr().as_u64()
            as *const x86_64::structures::paging::page_table::PageTable)
    };

    let l3_entry = &l3_table[l3_idx];
    if l3_entry.is_unused() {
        return false;
    }
    if l3_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return true; // 1GiB ページ: マッピング済み
    }

    let l2_table: &x86_64::structures::paging::page_table::PageTable = unsafe {
        &*(l3_entry.addr().as_u64()
            as *const x86_64::structures::paging::page_table::PageTable)
    };

    let l2_entry = &l2_table[l2_idx];
    if l2_entry.is_unused() {
        return false;
    }
    if l2_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return true; // 2MiB ページ: マッピング済み
    }

    let l1_table: &x86_64::structures::paging::page_table::PageTable = unsafe {
        &*(l2_entry.addr().as_u64()
            as *const x86_64::structures::paging::page_table::PageTable)
    };

    let l1_entry = &l1_table[l1_idx];
    !l1_entry.is_unused()
}

// =================================================================
// サウンド関連
// =================================================================

/// SYS_SOUND_PLAY: AC97 ドライバで正弦波ビープ音を再生する。
///
/// # 引数
/// - arg1 (freq_hz): 周波数 (Hz)。1〜20000 の範囲。
/// - arg2 (duration_ms): 持続時間 (ミリ秒)。1〜10000 の範囲。
///
/// # 戻り値
/// - 0: 成功
/// - エラー: InvalidArgument (範囲外), NotSupported (AC97 未検出)
pub(crate) fn sys_sound_play(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let freq_hz = arg1 as u32;
    let duration_ms = arg2 as u32;

    // 引数の範囲チェック
    if freq_hz == 0 || freq_hz > 20000 {
        return Err(SyscallError::InvalidArgument);
    }
    if duration_ms == 0 || duration_ms > 10000 {
        return Err(SyscallError::InvalidArgument);
    }

    // AC97 ドライバを取得して再生
    let mut ac97 = crate::ac97::AC97.lock();
    match ac97.as_mut() {
        Some(driver) => {
            driver.play_tone(freq_hz, duration_ms);
            Ok(0)
        }
        None => Err(SyscallError::NotSupported),
    }
}

/// SYS_THREAD_CREATE: 同一プロセス内で新しいスレッドを作成する
///
/// 引数:
///   arg1 — スレッドのエントリポイント（ユーザー空間アドレス）
///   arg2 — スレッド用ユーザースタックのトップ（mmap で確保済み）
///   arg3 — スレッドに渡す引数（rdi レジスタにセット）
///
/// 戻り値:
///   スレッドのタスク ID
pub(crate) fn sys_thread_create(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let entry_point = arg1;
    let stack_top = arg2;
    let arg = arg3;

    match crate::scheduler::spawn_thread(entry_point, stack_top, arg) {
        Ok(thread_id) => Ok(thread_id),
        Err(_e) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_THREAD_EXIT: 現在のスレッドを終了する
///
/// 引数:
///   arg1 — 終了コード
///
/// スレッドの終了処理。プロセスリーダーの exit とは異なり、
/// アドレス空間（CR3）の破棄は行わない。
pub(crate) fn sys_thread_exit(arg1: u64) -> Result<u64, SyscallError> {
    let exit_code = arg1 as i32;
    crate::scheduler::set_exit_code(exit_code);
    // exit_usermode() でカーネルモードに戻り、
    // thread_exit_handler または user_task_exit_handler に流れる
    crate::usermode::exit_usermode();
}

/// SYS_THREAD_JOIN: スレッドの終了を待つ
///
/// 引数:
///   arg1 — 待つスレッドのタスク ID
///   arg2 — タイムアウト (ms)。0 なら無期限待ち。
///
/// 戻り値:
///   スレッドの終了コード
pub(crate) fn sys_thread_join(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let thread_id = arg1;
    let timeout_ms = arg2;

    match crate::scheduler::wait_for_thread(thread_id, timeout_ms) {
        Ok(exit_code) => Ok(exit_code as u64),
        Err(crate::scheduler::WaitError::NoChild) => Err(SyscallError::InvalidArgument),
        Err(crate::scheduler::WaitError::NotChild) => Err(SyscallError::PermissionDenied),
        Err(crate::scheduler::WaitError::Timeout) => Err(SyscallError::Timeout),
    }
}

/// SYS_FUTEX: Futex 操作（ユーザー空間同期プリミティブの基盤）
///
/// 引数:
///   arg1 — ユーザー空間の AtomicU32 のアドレス
///   arg2 — 操作コード（0: FUTEX_WAIT, 1: FUTEX_WAKE）
///   arg3 — WAIT 時: expected 値 / WAKE 時: 起床させる最大タスク数
///   arg4 — WAIT 時: タイムアウト (ms, 0 = 無期限) / WAKE 時: 未使用
///
/// 戻り値:
///   WAIT: 0（起床した）/ エラー（値が不一致で即リターン）
///   WAKE: 起床したタスクの数
pub(crate) fn sys_futex(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let addr = arg1;
    let op = arg2;
    let val = arg3 as u32;

    match op {
        crate::futex::FUTEX_WAIT => {
            let timeout_ms = arg4;
            crate::futex::futex_wait(addr, val, timeout_ms)
        }
        crate::futex::FUTEX_WAKE => {
            crate::futex::futex_wake(addr, val)
        }
        _ => Err(SyscallError::InvalidArgument),
    }
}
