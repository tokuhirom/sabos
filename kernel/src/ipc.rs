// ipc.rs — タスク間通信 (IPC)
//
// シンプルなメッセージキュー方式の IPC を提供する。
// 1 タスクにつき 1 つの受信キューを持ち、
// send() で宛先キューにメッセージを追加、recv() で取り出す。
//
// ## Sleep/Wake 方式
//
// recv() はポーリングではなく、futex.rs と同じパターンで
// set_current_sleeping + wake_task を使って待機する。
// これにより CPU サイクルの浪費を防ぎ、レイテンシも改善する。
//
// ## キャンセル機構
//
// cancel_recv() で recv 待ちのタスクを Cancelled エラーで起床させる。
// IPC_CANCELLED セットにフラグを立てて wake_task を呼ぶ。
//
// ## Capability 委譲
//
// send_with_handle / recv_with_handle で IPC メッセージにハンドル（Capability）を
// 付けて送受信できる。マイクロカーネルの基盤。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use core::any::TypeId;
use lazy_static::lazy_static;
use spin::Mutex;

use crate::handle::{self, Handle};
use crate::scheduler;
use crate::user_ptr::SyscallError;

/// IPC メッセージ
#[derive(Debug, Clone)]
pub struct IpcMessage {
    pub sender: u64,
    pub data: Vec<u8>,
}

/// ハンドル付き IPC メッセージ
///
/// IPC 経由で Capability（Handle）を委譲するための構造体。
/// マイクロカーネル化の基盤として、ファイルハンドル等をプロセス間で受け渡す。
#[derive(Debug)]
pub struct IpcMessageWithHandle {
    pub sender: u64,
    pub data: Vec<u8>,
    pub handle: Handle,
}

/// 型安全 IPC 用のメッセージ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypedIpcMessage<T: Copy> {
    pub sender: u64,
    pub data: T,
}

/// 型安全 IPC 用の受信キュー
///
/// 1 タスクにつき 1 種類の型だけを受け付ける。
/// 異なる型を送ろうとした場合はエラーにする。
struct TypedIpcQueue {
    type_id: TypeId,
    queue: VecDeque<Box<dyn core::any::Any + Send>>,
}

lazy_static! {
    /// 宛先タスクID -> 受信キュー
    static ref IPC_QUEUES: Mutex<BTreeMap<u64, VecDeque<IpcMessage>>> = Mutex::new(BTreeMap::new());
    /// 宛先タスクID -> 型安全 IPC 受信キュー
    static ref TYPED_IPC_QUEUES: Mutex<BTreeMap<u64, TypedIpcQueue>> = Mutex::new(BTreeMap::new());
    /// 宛先タスクID -> ハンドル付き IPC 受信キュー
    static ref IPC_HANDLE_QUEUES: Mutex<BTreeMap<u64, VecDeque<IpcMessageWithHandle>>> = Mutex::new(BTreeMap::new());
    /// recv 待ちタスクの集合（Sleep/Wake 方式で使用）
    static ref IPC_WAITERS: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());
    /// キャンセルされたタスクの集合
    static ref IPC_CANCELLED: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());
}

// =================================================================
// 基本 IPC (バイト列メッセージ)
// =================================================================

/// メッセージを送信する
///
/// メッセージを dest タスクの受信キューに追加する。
/// dest が recv 待ち（Sleeping）の場合は wake_task で起床させる。
pub fn send(sender: u64, dest: u64, data: Vec<u8>) -> Result<(), SyscallError> {
    if !scheduler::task_exists(dest) {
        return Err(SyscallError::InvalidArgument);
    }

    // メッセージをキューに追加
    {
        let mut queues = IPC_QUEUES.lock();
        let q = queues.entry(dest).or_insert_with(VecDeque::new);
        q.push_back(IpcMessage { sender, data });
    }

    // dest が IPC recv 待ちなら起床させる
    wake_if_waiting(dest);

    Ok(())
}

/// メッセージを受信する（Sleep/Wake 方式）
///
/// timeout_ms = 0 の場合は無期限で待つ。
/// キャンセルされた場合は Cancelled エラーを返す。
///
/// ## 実装パターン（futex.rs と同じ）
/// 1. try_recv で即チェック → あればすぐ返す
/// 2. IPC_WAITERS に自分を登録
/// 3. set_current_sleeping で Sleeping に遷移
/// 4. ダブルチェック: もう一度 try_recv → あれば自分を起こして返す
/// 5. yield_now でスケジューラに制御を渡す
/// 6. 起床後: IPC_WAITERS から削除、キャンセルチェック、try_recv
pub fn recv(task_id: u64, timeout_ms: u64) -> Result<IpcMessage, SyscallError> {
    // 1. 即座にチェック
    if let Some(msg) = try_recv(task_id) {
        return Ok(msg);
    }

    // タイムアウト計算
    let wake_at = calc_wake_at(timeout_ms);

    // 2. IPC_WAITERS に登録
    {
        let mut waiters = IPC_WAITERS.lock();
        waiters.insert(task_id);
    }

    // 3. Sleeping に遷移
    scheduler::set_current_sleeping(wake_at);

    // 4. ダブルチェック: Sleeping にした直後、send() が来ていないか確認
    //    （set_current_sleeping と yield_now の間に send が来た場合の対策）
    if let Some(msg) = try_recv(task_id) {
        // メッセージが来ていた → 自分を起こして返す
        scheduler::wake_task(task_id);
        let mut waiters = IPC_WAITERS.lock();
        waiters.remove(&task_id);
        return Ok(msg);
    }

    // 5. スケジューラに制御を渡す
    scheduler::yield_now();

    // 6. 起床後の処理
    {
        let mut waiters = IPC_WAITERS.lock();
        waiters.remove(&task_id);
    }

    // キャンセルチェック
    {
        let mut cancelled = IPC_CANCELLED.lock();
        if cancelled.remove(&task_id) {
            return Err(SyscallError::Cancelled);
        }
    }

    // try_recv
    if let Some(msg) = try_recv(task_id) {
        return Ok(msg);
    }

    // タイムアウトで起床した場合
    Err(SyscallError::Timeout)
}

/// 1 回だけ受信を試みる
pub fn try_recv(task_id: u64) -> Option<IpcMessage> {
    let mut queues = IPC_QUEUES.lock();
    let q = queues.get_mut(&task_id)?;
    q.pop_front()
}

/// recv 待ちをキャンセルする
///
/// target_task_id が recv 待ち（Sleeping）の場合、Cancelled エラーで起床させる。
/// キャンセルフラグを IPC_CANCELLED に設定してから wake_task を呼ぶ。
pub fn cancel_recv(target_task_id: u64) -> Result<(), SyscallError> {
    if !scheduler::task_exists(target_task_id) {
        return Err(SyscallError::InvalidArgument);
    }

    // キャンセルフラグを立てる
    {
        let mut cancelled = IPC_CANCELLED.lock();
        cancelled.insert(target_task_id);
    }

    // 起床させる（Sleeping 状態なら Ready に戻る）
    scheduler::wake_task(target_task_id);

    Ok(())
}

// =================================================================
// ハンドル付き IPC (Capability 委譲)
// =================================================================

/// ハンドル付きメッセージを送信する
///
/// ハンドルを duplicate してから dest タスクの受信キューに追加する。
/// 元のハンドルは呼び出し元が引き続き使える。
pub fn send_with_handle(sender: u64, dest: u64, data: Vec<u8>, src_handle: &Handle) -> Result<(), SyscallError> {
    if !scheduler::task_exists(dest) {
        return Err(SyscallError::InvalidArgument);
    }

    // ハンドルを duplicate（新しい token、pos=0 でコピーを作成）
    let dup_handle = handle::duplicate_handle(src_handle)?;

    // メッセージをキューに追加
    {
        let mut queues = IPC_HANDLE_QUEUES.lock();
        let q = queues.entry(dest).or_insert_with(VecDeque::new);
        q.push_back(IpcMessageWithHandle {
            sender,
            data,
            handle: dup_handle,
        });
    }

    // dest が IPC recv 待ちなら起床させる
    wake_if_waiting(dest);

    Ok(())
}

/// ハンドル付きメッセージを受信する（Sleep/Wake 方式、キャンセルで中断）
///
/// タイムアウトなし。cancel_recv() でキャンセルされるまで待つ。
pub fn recv_with_handle(task_id: u64) -> Result<IpcMessageWithHandle, SyscallError> {
    // 1. 即座にチェック
    if let Some(msg) = try_recv_with_handle(task_id) {
        return Ok(msg);
    }

    // 無期限待ち
    let wake_at = u64::MAX;

    // 2. IPC_WAITERS に登録
    {
        let mut waiters = IPC_WAITERS.lock();
        waiters.insert(task_id);
    }

    // 3. Sleeping に遷移
    scheduler::set_current_sleeping(wake_at);

    // 4. ダブルチェック
    if let Some(msg) = try_recv_with_handle(task_id) {
        scheduler::wake_task(task_id);
        let mut waiters = IPC_WAITERS.lock();
        waiters.remove(&task_id);
        return Ok(msg);
    }

    // 5. スケジューラに制御を渡す
    scheduler::yield_now();

    // 6. 起床後の処理
    {
        let mut waiters = IPC_WAITERS.lock();
        waiters.remove(&task_id);
    }

    // キャンセルチェック
    {
        let mut cancelled = IPC_CANCELLED.lock();
        if cancelled.remove(&task_id) {
            return Err(SyscallError::Cancelled);
        }
    }

    // try_recv
    if let Some(msg) = try_recv_with_handle(task_id) {
        return Ok(msg);
    }

    // ここに来ることは通常ないが、安全のために Timeout を返す
    Err(SyscallError::Timeout)
}

/// 1 回だけハンドル付き受信を試みる
pub fn try_recv_with_handle(task_id: u64) -> Option<IpcMessageWithHandle> {
    let mut queues = IPC_HANDLE_QUEUES.lock();
    let q = queues.get_mut(&task_id)?;
    q.pop_front()
}

// =================================================================
// タスク終了時のクリーンアップ
// =================================================================

/// タスク終了時に IPC キューをクリーンアップする。
///
/// タスクが終了すると、そのタスク宛の未読メッセージは誰も読まないので
/// メモリリークになる。この関数でキューを丸ごと削除して解放する。
/// 未読のハンドル付きメッセージのハンドルも close する。
pub fn cleanup_task(task_id: u64) {
    // 通常メッセージキュー
    {
        let mut queues = IPC_QUEUES.lock();
        queues.remove(&task_id);
    }
    // 型安全 IPC キュー
    {
        let mut queues = TYPED_IPC_QUEUES.lock();
        queues.remove(&task_id);
    }
    // ハンドル付きメッセージキュー（未読ハンドルを close）
    {
        let mut queues = IPC_HANDLE_QUEUES.lock();
        if let Some(mut q) = queues.remove(&task_id) {
            for msg in q.drain(..) {
                let _ = handle::close(&msg.handle);
            }
        }
    }
    // waiters/cancelled からも除去
    {
        let mut waiters = IPC_WAITERS.lock();
        waiters.remove(&task_id);
    }
    {
        let mut cancelled = IPC_CANCELLED.lock();
        cancelled.remove(&task_id);
    }
}

// =================================================================
// 内部ヘルパー
// =================================================================

/// dest が IPC_WAITERS に登録されている場合、wake_task で起床させる
fn wake_if_waiting(dest: u64) {
    let is_waiting = {
        let waiters = IPC_WAITERS.lock();
        waiters.contains(&dest)
    };
    if is_waiting {
        scheduler::wake_task(dest);
    }
}

/// タイムアウトから wake_at（タイマーティック）を計算する
///
/// timeout_ms = 0 → 無期限待ち（u64::MAX）
/// timeout_ms > 0 → 現在 + ticks
fn calc_wake_at(timeout_ms: u64) -> u64 {
    if timeout_ms == 0 {
        u64::MAX
    } else {
        let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        // PIT は約 18.2 Hz (1 tick ≈ 55ms)
        // ticks = ms * 182 / 10000
        now + (timeout_ms * 182 / 10000).max(1)
    }
}

// =================================================================
// 型安全 IPC (カーネル内プロトタイプ)
// =================================================================
//
// 1 タスクにつき 1 種類のメッセージ型を割り当てる。
// send/recv の型が一致しない場合はエラーにすることで、
// コンパイル時に型を固定し、実行時にも型の混在を防ぐ。

/// 型安全 IPC: メッセージを送信する
pub fn send_typed<T: Copy + Send + 'static>(sender: u64, dest: u64, data: T) -> Result<(), SyscallError> {
    if !scheduler::task_exists(dest) {
        return Err(SyscallError::InvalidArgument);
    }

    let mut queues = TYPED_IPC_QUEUES.lock();
    let entry = queues.entry(dest).or_insert_with(|| TypedIpcQueue {
        type_id: TypeId::of::<T>(),
        queue: VecDeque::new(),
    });

    if entry.type_id != TypeId::of::<T>() {
        return Err(SyscallError::InvalidArgument);
    }

    entry.queue.push_back(Box::new(TypedIpcMessage { sender, data }));

    // ロックを解放してから wake
    drop(queues);
    wake_if_waiting(dest);

    Ok(())
}

/// 型安全 IPC: メッセージを受信する（Sleep/Wake 方式）
///
/// timeout_ms = 0 の場合は無期限で待つ。
pub fn recv_typed<T: Copy + Send + 'static>(task_id: u64, timeout_ms: u64) -> Result<TypedIpcMessage<T>, SyscallError> {
    // 1. 即座にチェック
    match try_recv_typed_once::<T>(task_id) {
        Ok(Some(msg)) => return Ok(msg),
        Err(e) => return Err(e),
        Ok(None) => {}
    }

    // タイムアウト計算
    let wake_at = calc_wake_at(timeout_ms);

    // 2. IPC_WAITERS に登録
    {
        let mut waiters = IPC_WAITERS.lock();
        waiters.insert(task_id);
    }

    // 3. Sleeping に遷移
    scheduler::set_current_sleeping(wake_at);

    // 4. ダブルチェック
    match try_recv_typed_once::<T>(task_id) {
        Ok(Some(msg)) => {
            scheduler::wake_task(task_id);
            let mut waiters = IPC_WAITERS.lock();
            waiters.remove(&task_id);
            return Ok(msg);
        }
        Err(e) => {
            scheduler::wake_task(task_id);
            let mut waiters = IPC_WAITERS.lock();
            waiters.remove(&task_id);
            return Err(e);
        }
        Ok(None) => {}
    }

    // 5. スケジューラに制御を渡す
    scheduler::yield_now();

    // 6. 起床後の処理
    {
        let mut waiters = IPC_WAITERS.lock();
        waiters.remove(&task_id);
    }

    // キャンセルチェック
    {
        let mut cancelled = IPC_CANCELLED.lock();
        if cancelled.remove(&task_id) {
            return Err(SyscallError::Cancelled);
        }
    }

    // try_recv
    match try_recv_typed_once::<T>(task_id) {
        Ok(Some(msg)) => Ok(msg),
        Ok(None) => Err(SyscallError::Timeout),
        Err(e) => Err(e),
    }
}

/// 1 回だけ型安全 IPC 受信を試みる
fn try_recv_typed_once<T: Copy + Send + 'static>(task_id: u64) -> Result<Option<TypedIpcMessage<T>>, SyscallError> {
    let mut queues = TYPED_IPC_QUEUES.lock();
    let entry = match queues.get_mut(&task_id) {
        Some(q) => q,
        None => return Ok(None),
    };

    if entry.type_id != TypeId::of::<T>() {
        return Err(SyscallError::InvalidArgument);
    }

    let msg = match entry.queue.pop_front() {
        Some(m) => m,
        None => return Ok(None),
    };

    match msg.downcast::<TypedIpcMessage<T>>() {
        Ok(boxed) => Ok(Some(*boxed)),
        Err(_) => Err(SyscallError::Other),
    }
}
