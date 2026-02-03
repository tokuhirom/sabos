// ipc.rs — タスク間通信 (IPC)
//
// シンプルなメッセージキュー方式の IPC を提供する。
// 1 タスクにつき 1 つの受信キューを持ち、
// send() で宛先キューにメッセージを追加、recv() で取り出す。
//
// いずれも UserSlice で検証済みのバッファを使う前提。
// ここではカーネル内のデータ構造のみ扱う。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
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

lazy_static! {
    /// 宛先タスクID -> 受信キュー
    static ref IPC_QUEUES: Mutex<BTreeMap<u64, VecDeque<IpcMessage>>> = Mutex::new(BTreeMap::new());
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
        if let Some(msg) = try_recv_once(task_id) {
            return Ok(msg);
        }

        if let Some(deadline) = deadline_tick {
            let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
            if now >= deadline {
                return Err(SyscallError::Timeout);
            }
        }

        // 他のタスクに譲る
        scheduler::sleep_ticks(1);
    }
}

/// 1 回だけ受信を試みる
fn try_recv_once(task_id: u64) -> Option<IpcMessage> {
    let mut queues = IPC_QUEUES.lock();
    let q = queues.get_mut(&task_id)?;
    q.pop_front()
}
