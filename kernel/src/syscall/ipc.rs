// syscall/ipc.rs — IPC・ブロックデバイス関連システムコール
//
// SYS_IPC_SEND/RECV/RECV_FROM/CANCEL/SEND_HANDLE/RECV_HANDLE,
// SYS_BLOCK_READ/WRITE

use crate::user_ptr::SyscallError;
use super::{user_slice_from_args, user_ptr_from_arg};

/// SYS_BLOCK_READ: ブロックデバイスからセクタを読み取る
///
/// 引数:
///   arg1 — セクタ番号
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ（512 バイト固定）
///   arg4 — デバイスインデックス（0 = disk.img, 1 = hostfs.img, ...）
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_block_read(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg3).map_err(|_| SyscallError::InvalidArgument)?;
    if len != 512 {
        return Err(SyscallError::InvalidArgument);
    }
    let dev_index = arg4 as usize;

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    let mut devs = crate::virtio_blk::VIRTIO_BLKS.lock();
    let drv = devs.get_mut(dev_index).ok_or(SyscallError::Other)?;
    // ユーザー空間のバッファは物理アドレスではないため、
    // DMA 先に直接渡すと壊れる。カーネルバッファに読み取ってから
    // ユーザー空間にコピーする。
    let mut kernel_buf = [0u8; 512];
    drv.read_sector(arg1, &mut kernel_buf).map_err(|_| SyscallError::Other)?;
    buf.copy_from_slice(&kernel_buf);
    Ok(len as u64)
}

/// SYS_BLOCK_WRITE: ブロックデバイスにセクタを書き込む
///
/// 引数:
///   arg1 — セクタ番号
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ（512 バイト固定）
///   arg4 — デバイスインデックス（0 = disk.img, 1 = hostfs.img, ...）
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_block_write(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg3).map_err(|_| SyscallError::InvalidArgument)?;
    if len != 512 {
        return Err(SyscallError::InvalidArgument);
    }
    let dev_index = arg4 as usize;

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_slice();

    let mut devs = crate::virtio_blk::VIRTIO_BLKS.lock();
    let drv = devs.get_mut(dev_index).ok_or(SyscallError::Other)?;
    // DMA 先は物理アドレス前提なので、カーネルバッファにコピーしてから書き込む。
    let mut kernel_buf = [0u8; 512];
    kernel_buf.copy_from_slice(buf);
    drv.write_sector(arg1, &kernel_buf).map_err(|_| SyscallError::Other)?;
    Ok(len as u64)
}

/// SYS_IPC_SEND: メッセージを送信する
///
/// 引数:
///   arg1 — 宛先タスクID
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_ipc_send(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_slice();

    let sender = crate::scheduler::current_task_id();
    crate::ipc::send(sender, arg1, buf.to_vec())?;
    Ok(0)
}

/// SYS_IPC_RECV: メッセージを受信する
///
/// 引数:
///   arg1 — 送信元タスクIDの書き込み先（ユーザー空間）
///   arg2 — 受信バッファのポインタ（ユーザー空間）
///   arg3 — 受信バッファの長さ
///   arg4 — タイムアウト (ms). 0 は非ブロッキング（即座チェック）
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_ipc_recv(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // IPC 受信は待ちに入る可能性があるため、割り込みを有効化してタイマ割り込みを許可する。
    // これをしないと sleep_ticks() が起床できず、待ちが永久に続く。
    x86_64::instructions::interrupts::enable();

    let sender_ptr = user_ptr_from_arg::<u64>(arg1)?;
    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    let task_id = crate::scheduler::current_task_id();
    let msg = crate::ipc::recv(task_id, arg4)?;

    let copy_len = core::cmp::min(buf.len(), msg.data.len());
    buf[..copy_len].copy_from_slice(&msg.data[..copy_len]);
    sender_ptr.write(msg.sender);

    Ok(copy_len as u64)
}

/// SYS_IPC_RECV_FROM: 特定の送信元からのメッセージのみを受信する
///
/// 指定した from_sender からのメッセージだけをキューから取り出す。
/// 他の送信元からのメッセージはキューに残す。
/// netd_request のように、特定のサービスからの応答を待つ場合に使用する。
///
/// 引数:
///   arg1 — フィルタリングする送信元タスクID
///   arg2 — 受信バッファのポインタ（ユーザー空間）
///   arg3 — 受信バッファの長さ
///   arg4 — タイムアウト (ms). 0 は非ブロッキング（即座チェック）
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_ipc_recv_from(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // IPC 受信は待ちに入る可能性があるため、割り込みを有効化
    x86_64::instructions::interrupts::enable();

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    let task_id = crate::scheduler::current_task_id();
    let from_sender = arg1;
    let msg = crate::ipc::recv_from(task_id, from_sender, arg4)?;

    let copy_len = core::cmp::min(buf.len(), msg.data.len());
    buf[..copy_len].copy_from_slice(&msg.data[..copy_len]);

    Ok(copy_len as u64)
}

/// SYS_IPC_CANCEL: IPC recv 待ちをキャンセルする
///
/// 引数:
///   arg1 — キャンセル対象のタスクID
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_ipc_cancel(arg1: u64) -> Result<u64, SyscallError> {
    crate::ipc::cancel_recv(arg1)?;
    Ok(0)
}

/// SYS_IPC_SEND_HANDLE: ハンドル付き IPC メッセージを送信する
///
/// 引数:
///   arg1 — 宛先タスクID
///   arg2 — バッファのポインタ（ユーザー空間）
///   arg3 — バッファの長さ
///   arg4 — ハンドルのポインタ（ユーザー空間、Handle 構造体）
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_ipc_send_handle(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_slice();

    let handle_ptr = user_ptr_from_arg::<crate::handle::Handle>(arg4)?;
    let handle = handle_ptr.read();

    let sender = crate::scheduler::current_task_id();
    crate::ipc::send_with_handle(sender, arg1, buf.to_vec(), &handle)?;
    Ok(0)
}

/// SYS_IPC_RECV_HANDLE: ハンドル付き IPC メッセージを受信する
///
/// 引数:
///   arg1 — 送信元タスクIDの書き込み先（ユーザー空間）
///   arg2 — 受信バッファのポインタ（ユーザー空間）
///   arg3 — 受信バッファの長さ
///   arg4 — ハンドルの書き込み先（ユーザー空間、Handle 構造体）
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_ipc_recv_handle(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // IPC 受信は待ちに入る可能性があるため、割り込みを有効化してタイマ割り込みを許可する
    x86_64::instructions::interrupts::enable();

    let sender_ptr = user_ptr_from_arg::<u64>(arg1)?;
    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();
    let handle_out_ptr = user_ptr_from_arg::<crate::handle::Handle>(arg4)?;

    let task_id = crate::scheduler::current_task_id();
    let msg = crate::ipc::recv_with_handle(task_id)?;

    let copy_len = core::cmp::min(buf.len(), msg.data.len());
    buf[..copy_len].copy_from_slice(&msg.data[..copy_len]);
    sender_ptr.write(msg.sender);
    handle_out_ptr.write(msg.handle);

    Ok(copy_len as u64)
}
