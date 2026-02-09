// ipc.rs — タスク間通信 (IPC)
//
// シンプルなメッセージキュー方式の IPC を提供する。
// 1 タスクにつき 1 つの受信キューを持ち、
// send() で宛先キューにメッセージを追加、recv() で取り出す。
//
// いずれも UserSlice で検証済みのバッファを使う前提。
// ここではカーネル内のデータ構造のみ扱う。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::any::TypeId;
use lazy_static::lazy_static;
use spin::Mutex;

use crate::scheduler;
use crate::user_ptr::SyscallError;

/// IPC メッセージ
#[derive(Debug, Clone)]
pub struct IpcMessage {
    pub sender: u64,
    pub data: Vec<u8>,
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
}

/// メッセージを送信する
pub fn send(sender: u64, dest: u64, data: Vec<u8>) -> Result<(), SyscallError> {
    if !scheduler::task_exists(dest) {
        return Err(SyscallError::InvalidArgument);
    }

    let mut queues = IPC_QUEUES.lock();
    let q = queues.entry(dest).or_insert_with(VecDeque::new);
    q.push_back(IpcMessage { sender, data });
    Ok(())
}

/// メッセージを受信する
///
/// timeout_ms = 0 の場合は無期限で待つ。
pub fn recv(task_id: u64, timeout_ms: u64) -> Result<IpcMessage, SyscallError> {
    let start_tick = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    let deadline_tick = if timeout_ms == 0 {
        None
    } else {
        // 1 tick ≈ 54.925ms
        let ticks = (timeout_ms * 182 / 10000).max(1);
        Some(start_tick + ticks)
    };

    loop {
        if let Some(msg) = try_recv(task_id) {
            return Ok(msg);
        }

        if let Some(deadline) = deadline_tick {
            let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
            if now >= deadline {
                return Err(SyscallError::Timeout);
            }
        }

        // 他のタスクに譲る（yield_now で即座に Ready に戻り、次のスケジューリングラウンドで再チェック）
        scheduler::yield_now();
    }
}

/// 1 回だけ受信を試みる
pub fn try_recv(task_id: u64) -> Option<IpcMessage> {
    let mut queues = IPC_QUEUES.lock();
    let q = queues.get_mut(&task_id)?;
    q.pop_front()
}

/// タスク終了時に IPC キューをクリーンアップする。
///
/// タスクが終了すると、そのタスク宛の未読メッセージは誰も読まないので
/// メモリリークになる。この関数でキューを丸ごと削除して解放する。
pub fn cleanup_task(task_id: u64) {
    {
        let mut queues = IPC_QUEUES.lock();
        queues.remove(&task_id);
    }
    {
        let mut queues = TYPED_IPC_QUEUES.lock();
        queues.remove(&task_id);
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
    Ok(())
}

/// 型安全 IPC: メッセージを受信する
///
/// timeout_ms = 0 の場合は無期限で待つ。
pub fn recv_typed<T: Copy + Send + 'static>(task_id: u64, timeout_ms: u64) -> Result<TypedIpcMessage<T>, SyscallError> {
    let start_tick = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    let deadline_tick = if timeout_ms == 0 {
        None
    } else {
        // 1 tick ≈ 54.925ms
        let ticks = (timeout_ms * 182 / 10000).max(1);
        Some(start_tick + ticks)
    };

    loop {
        match try_recv_typed_once::<T>(task_id) {
            Ok(Some(msg)) => return Ok(msg),
            Ok(None) => {}
            Err(e) => return Err(e),
        }

        if let Some(deadline) = deadline_tick {
            let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
            if now >= deadline {
                return Err(SyscallError::Timeout);
            }
        }

        // 他のタスクに譲る（yield_now で即座に Ready に戻り、次のスケジューリングラウンドで再チェック）
        scheduler::yield_now();
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
