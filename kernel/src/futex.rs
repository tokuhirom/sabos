// futex.rs — Fast Userspace Mutex
//
// ユーザー空間の同期プリミティブ（Mutex/Condvar）の基盤となる futex を実装する。
// futex (Fast Userspace Mutex) はカーネル支援のスリープ/ウェイク機構。
// 競合がなければカーネルに入らず、競合時のみ syscall でスリープ/ウェイクする。
//
// ## 仕組み
//
// ユーザー空間にある AtomicU32 の値を「ロック状態」として使う。
// - 競合なし: ユーザー空間で CAS (Compare-And-Swap) するだけでロック取得
// - 競合あり: FUTEX_WAIT でカーネルに入り、スリープして待つ
// - ロック解放: FUTEX_WAKE でカーネルに入り、待機中のスレッドを起こす
//
// ## キーの設計
//
// キーは (CR3 物理アドレス, ユーザー空間仮想アドレス) のペア。
// CR3 を含めることで、異なるプロセスの同じ仮想アドレスを区別できる。
// 同一プロセス内のスレッドは同じ CR3 を共有するので、正しく同期できる。

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use spin::Mutex;
use crate::user_ptr::SyscallError;

/// Futex 操作コード
pub const FUTEX_WAIT: u64 = 0;  // 値が一致したらスリープ
pub const FUTEX_WAKE: u64 = 1;  // 待機中のタスクを起床

/// Futex テーブル。
/// キーは (CR3 の物理アドレス, ユーザー空間仮想アドレス) のペア。
/// CR3 を含めることで、異なるプロセスの同じ仮想アドレスを区別する。
/// 値はそのアドレスで待機中のタスク ID のリスト。
static FUTEX_TABLE: Mutex<BTreeMap<(u64, u64), Vec<u64>>> =
    Mutex::new(BTreeMap::new());

/// 現在のタスクのアドレス空間 ID を取得する。
/// Futex のキーとして使用し、プロセスごとにアドレス空間を区別する。
///
/// ユーザープロセス（スレッド含む）の場合はタスクの CR3 フレームの物理アドレスを返す。
/// カーネルタスクの場合はカーネル共通の CR3 を返す。
/// CR3 レジスタの実際の値ではなく、タスクに登録された値を使うことで、
/// 同じアドレス空間を共有するタスク間でキーが一致することを保証する。
fn current_address_space_id() -> u64 {
    crate::scheduler::current_task_cr3()
}

/// FUTEX_WAIT: アドレスの値が expected と一致したらスリープ
///
/// 1. addr の値を読む（ユーザー空間ポインタ）
/// 2. 値が expected と不一致なら即座に返す（他スレッドが既に変更済み）
/// 3. 一致したら現在のタスクを FUTEX_TABLE に登録
/// 4. タスクを Sleeping 状態にして yield
/// 5. 起床後（futex_wake or タイムアウト）、FUTEX_TABLE から自分を削除して返す
///
/// # 引数
/// - `addr`: ユーザー空間の AtomicU32 のアドレス
/// - `expected`: 期待する値（この値と一致したらスリープ）
/// - `timeout_ms`: タイムアウト (ms)。0 なら無期限待ち。
///
/// # 戻り値
/// - Ok(0): 正常に起床した
/// - Err(Other): 値が expected と一致しなかった（スリープしなかった）
pub fn futex_wait(addr: u64, expected: u32, timeout_ms: u64) -> Result<u64, SyscallError> {
    // ユーザー空間から値を読み取り
    // SAFETY: syscall ハンドラが addr のユーザー空間チェックを行う前提
    let current_val = unsafe { *(addr as *const u32) };
    if current_val != expected {
        // 値が既に変わっている（他のスレッドがロックを解放した等）
        // スリープせずに即座にリターンし、呼び出し元に再試行させる
        return Err(SyscallError::Other);
    }

    let task_id = crate::scheduler::current_task_id();
    let cr3 = current_address_space_id();

    // FUTEX_TABLE に自分のタスク ID を登録
    {
        let mut table = FUTEX_TABLE.lock();
        table.entry((cr3, addr)).or_insert_with(Vec::new).push(task_id);
    }

    // タスクを Sleeping 状態にする
    // timeout_ms == 0 なら無期限待ち（u64::MAX は実質無限）
    let wake_at = if timeout_ms == 0 {
        u64::MAX // 無期限待ち（futex_wake で明示的に起こされるまで）
    } else {
        // PIT は約 18.2 Hz (1 tick ≈ 55ms)
        // ticks = ms * 182 / 10000
        let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        now + (timeout_ms * 182 / 10000).max(1)
    };
    crate::scheduler::set_current_sleeping(wake_at);
    crate::scheduler::yield_now();

    // ここに到達 = 起床した（futex_wake で起こされた or タイムアウト）
    // FUTEX_TABLE から自分を削除（futex_wake で既に削除されている場合もある）
    {
        let mut table = FUTEX_TABLE.lock();
        if let Some(waiters) = table.get_mut(&(cr3, addr)) {
            waiters.retain(|&id| id != task_id);
            if waiters.is_empty() {
                table.remove(&(cr3, addr));
            }
        }
    }

    Ok(0)
}

/// FUTEX_WAKE: アドレスで待機中のタスクを最大 count 個起床させる
///
/// Mutex のアンロック時や Condvar の notify 時に呼ばれる。
/// 待機者リストから最大 count 個のタスクを取り出し、Ready 状態に戻す。
///
/// # 引数
/// - `addr`: ユーザー空間の AtomicU32 のアドレス
/// - `count`: 起床させるタスクの最大数（u32::MAX = 全員起床）
///
/// # 戻り値
/// - Ok(n): 実際に起床したタスクの数
pub fn futex_wake(addr: u64, count: u32) -> Result<u64, SyscallError> {
    let cr3 = current_address_space_id();
    let mut woken = 0u64;

    // 待機者リストからタスクを取り出す
    let to_wake = {
        let mut table = FUTEX_TABLE.lock();
        if let Some(waiters) = table.get_mut(&(cr3, addr)) {
            let wake_count = (count as usize).min(waiters.len());
            let to_wake: Vec<u64> = waiters.drain(..wake_count).collect();
            if waiters.is_empty() {
                table.remove(&(cr3, addr));
            }
            to_wake
        } else {
            Vec::new()
        }
    };
    // ロック解放後にスケジューラ操作（デッドロック防止）

    for task_id in to_wake {
        crate::scheduler::wake_task(task_id);
        woken += 1;
    }

    Ok(woken)
}
