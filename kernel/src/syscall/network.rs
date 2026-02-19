// syscall/network.rs — ネットワーク関連システムコール
//
// SYS_NET_SEND/RECV_FRAME, SYS_NET_GET_MAC,
// SYS_NET_DNS_LOOKUP, SYS_NET_TCP_*, SYS_NET_UDP_*, SYS_NET_PING6

use crate::user_ptr::SyscallError;
use super::user_slice_from_args;

/// SYS_NET_SEND_FRAME: Ethernet フレーム送信
///
/// 引数:
///   arg1 — フレームのポインタ（ユーザー空間）
///   arg2 — フレームの長さ
///
/// 戻り値:
///   送信したバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_net_send_frame(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    if len == 0 || len > 1514 {
        return Err(SyscallError::InvalidArgument);
    }

    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_slice();

    let mut drv = crate::virtio_net::VIRTIO_NET.lock();
    let drv = drv.as_mut().ok_or(SyscallError::Other)?;
    drv.send_packet(buf).map_err(|_| SyscallError::Other)?;

    Ok(len as u64)
}

/// SYS_NET_RECV_FRAME: Ethernet フレーム受信
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ
///   arg3 — タイムアウト（ミリ秒）。0 なら即時復帰
///
/// 戻り値:
///   受信したバイト数（成功時）
///   0（タイムアウト時）
///   負の値（エラー時）
pub(crate) fn sys_net_recv_frame(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let buf_len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    let timeout_ms = arg3;

    if buf_len == 0 {
        return Err(SyscallError::InvalidArgument);
    }

    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    x86_64::instructions::interrupts::enable();
    let start_tick = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);

    loop {
        {
            let mut drv = crate::virtio_net::VIRTIO_NET.lock();
            if let Some(frame) = drv.as_mut().and_then(|d| d.receive_packet()) {
                let copy_len = core::cmp::min(frame.len(), buf_len);
                buf[..copy_len].copy_from_slice(&frame[..copy_len]);
                return Ok(copy_len as u64);
            }
        }

        if timeout_ms == 0 {
            return Ok(0);
        }

        let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        let elapsed_ticks = now.saturating_sub(start_tick);
        let elapsed_ms = elapsed_ticks * 55;
        if elapsed_ms >= timeout_ms {
            return Ok(0);
        }

        // QEMU TCG モードでは、ゲスト CPU がビジーループしていると
        // SLIRP のネットワーク I/O が処理されない。
        //
        // ISR ステータスの読み取り（port I/O）で QEMU のイベントループを
        // キックし、SLIRP が受信パケットを処理できるようにする。
        // その後 hlt で CPU を停止して、タイマー割り込みまで待機する。
        {
            let mut drv = crate::virtio_net::VIRTIO_NET.lock();
            if let Some(d) = drv.as_mut() {
                d.read_isr_status();
            }
        }
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

/// SYS_NET_GET_MAC: MAC アドレス取得
///
/// 引数:
///   arg1 — 書き込み先バッファ（ユーザー空間）
///   arg2 — バッファの長さ（6 以上）
///
/// 戻り値:
///   6（成功時）
///   負の値（エラー時）
pub(crate) fn sys_net_get_mac(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    if buf_len < 6 {
        return Err(SyscallError::InvalidArgument);
    }

    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    let drv = crate::virtio_net::VIRTIO_NET.lock();
    let drv = drv.as_ref().ok_or(SyscallError::Other)?;
    buf[..6].copy_from_slice(&drv.mac_address);
    Ok(6)
}

// =================================================================
// カーネル内ネットワークスタック系システムコール (40-44, 150-156)
// =================================================================

/// SYS_NET_DNS_LOOKUP: DNS 名前解決
///
/// 引数:
///   arg1 — ドメイン名バッファ（ユーザー空間）
///   arg2 — ドメイン名の長さ
///   arg3 — 結果 IP アドレス書き込み先（4 バイト）
///
/// 戻り値: 0（成功）、負（エラー）
pub(crate) fn sys_net_dns_lookup(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    // wait_net_condition で待ちに入るため、割り込みを有効化する。
    // syscall は割り込み無効状態で実行されるが、yield_now() / sleep が正しく動作するには
    // タイマー割り込みが必要。
    x86_64::instructions::interrupts::enable();
    let domain_slice = user_slice_from_args(arg1, arg2)?;
    let domain_bytes = domain_slice.as_slice();
    let domain = core::str::from_utf8(domain_bytes).map_err(|_| SyscallError::InvalidArgument)?;

    let ip = crate::netstack::dns_lookup(domain).map_err(|_| SyscallError::Other)?;

    // 結果を書き込み
    let result_slice = user_slice_from_args(arg3, 4)?;
    let result_buf = result_slice.as_mut_slice();
    result_buf[..4].copy_from_slice(&ip);
    Ok(0)
}

/// SYS_NET_TCP_CONNECT: TCP 接続の確立
///
/// 引数:
///   arg1 — IP アドレスバッファ（4 バイト、ユーザー空間）
///   arg2 — ポート番号
///
/// 戻り値: conn_id（成功）、負（エラー）
pub(crate) fn sys_net_tcp_connect(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    // wait_net_condition で待ちに入るため、割り込みを有効化する
    x86_64::instructions::interrupts::enable();
    let ip_slice = user_slice_from_args(arg1, 4)?;
    let ip_bytes = ip_slice.as_slice();
    let mut ip = [0u8; 4];
    ip.copy_from_slice(&ip_bytes[..4]);
    let port = arg2 as u16;

    let conn_id = crate::netstack::tcp_connect(ip, port).map_err(|_| SyscallError::Other)?;
    Ok(conn_id as u64)
}

/// SYS_NET_TCP_SEND: TCP データ送信
///
/// 引数:
///   arg1 — conn_id
///   arg2 — データバッファ（ユーザー空間）
///   arg3 — データの長さ
///
/// 戻り値: 0（成功）、負（エラー）
pub(crate) fn sys_net_tcp_send(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let conn_id = arg1 as u32;
    let data_slice = user_slice_from_args(arg2, arg3)?;
    let data = data_slice.as_slice();

    crate::netstack::tcp_send(conn_id, data).map_err(|_| SyscallError::Other)?;
    Ok(0)
}

/// SYS_NET_TCP_RECV: TCP データ受信
///
/// 引数:
///   arg1 — conn_id
///   arg2 — バッファ（ユーザー空間）
///   arg3 — バッファの長さ
///   arg4 — タイムアウト（ミリ秒）
///
/// 戻り値: 受信バイト数（成功）、0（タイムアウト）、負（エラー）
pub(crate) fn sys_net_tcp_recv(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    // wait_net_condition で待ちに入るため、割り込みを有効化する
    x86_64::instructions::interrupts::enable();
    let conn_id = arg1 as u32;
    let buf_len = usize::try_from(arg3).map_err(|_| SyscallError::InvalidArgument)?;
    let timeout_ms = arg4;

    let buf_slice = user_slice_from_args(arg2, arg3)?;
    let buf = buf_slice.as_mut_slice();

    match crate::netstack::tcp_recv(conn_id, timeout_ms) {
        Ok(data) => {
            let copy_len = core::cmp::min(data.len(), buf_len);
            buf[..copy_len].copy_from_slice(&data[..copy_len]);
            Ok(copy_len as u64)
        }
        Err("timeout") => Ok(0),
        Err("connection closed") => Ok(0),
        Err(_) => Err(SyscallError::Other),
    }
}

/// SYS_NET_TCP_CLOSE: TCP 接続のクローズ
///
/// 引数:
///   arg1 — conn_id
///
/// 戻り値: 0（成功）、負（エラー）
pub(crate) fn sys_net_tcp_close(arg1: u64) -> Result<u64, SyscallError> {
    // wait_net_condition で待ちに入るため、割り込みを有効化する
    x86_64::instructions::interrupts::enable();
    let conn_id = arg1 as u32;
    crate::netstack::tcp_close(conn_id).map_err(|_| SyscallError::Other)?;
    Ok(0)
}

/// SYS_NET_TCP_LISTEN: TCP リッスン開始
///
/// 引数:
///   arg1 — ポート番号
///
/// 戻り値: 0（成功）、負（エラー）
pub(crate) fn sys_net_tcp_listen(arg1: u64) -> Result<u64, SyscallError> {
    let port = arg1 as u16;
    crate::netstack::tcp_listen(port).map_err(|_| SyscallError::Other)?;
    Ok(0)
}

/// SYS_NET_TCP_ACCEPT: TCP 接続の受け入れ
///
/// 引数:
///   arg1 — タイムアウト（ミリ秒）
///   arg2 — リッスンポート
///
/// 戻り値: conn_id（成功）、負（エラー/タイムアウト）
pub(crate) fn sys_net_tcp_accept(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    // wait_net_condition で待ちに入るため、割り込みを有効化する
    x86_64::instructions::interrupts::enable();
    let timeout_ms = arg1;
    let listen_port = arg2 as u16;

    match crate::netstack::tcp_accept(timeout_ms, listen_port) {
        Ok(conn_id) => Ok(conn_id as u64),
        Err("timeout") => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::Other),
    }
}

/// SYS_NET_UDP_BIND: UDP ソケットバインド
///
/// 引数:
///   arg1 — ポート番号（0 = エフェメラルポート自動割り当て）
///
/// 戻り値: socket_id | (local_port << 32)
pub(crate) fn sys_net_udp_bind(arg1: u64) -> Result<u64, SyscallError> {
    let port = arg1 as u16;
    let socket_id = crate::netstack::udp_bind(port).map_err(|_| SyscallError::Other)?;
    let local_port = crate::netstack::udp_local_port(socket_id).map_err(|_| SyscallError::Other)?;
    // socket_id を下位 32 ビット、local_port を上位 32 ビットにパック
    Ok(socket_id as u64 | ((local_port as u64) << 32))
}

/// SYS_NET_UDP_SEND_TO: UDP データ送信
///
/// 引数:
///   arg1 — UdpSendToArgs 構造体ポインタ（ユーザー空間）
///
/// 戻り値: 0（成功）、負（エラー）
pub(crate) fn sys_net_udp_send_to(arg1: u64) -> Result<u64, SyscallError> {
    // UdpSendToArgs を読み取る
    let args_size = core::mem::size_of::<sabos_syscall::UdpSendToArgs>();
    let args_slice = user_slice_from_args(arg1, args_size as u64)?;
    let args_bytes = args_slice.as_slice();

    let socket_id = u32::from_le_bytes([args_bytes[0], args_bytes[1], args_bytes[2], args_bytes[3]]);
    let dst_ip = [args_bytes[4], args_bytes[5], args_bytes[6], args_bytes[7]];
    let dst_port = u16::from_le_bytes([args_bytes[8], args_bytes[9]]);
    // _pad at [10..12]
    // data_ptr at [12..20] (u64 LE) — ただし構造体のアラインメントに注意
    // UdpSendToArgs: socket_id(4) + dst_ip(4) + dst_port(2) + _pad(2) + data_ptr(8) + data_len(8) = 28
    // しかし #[repr(C)] では u64 は 8 バイトアラインなので padding が入る可能性
    // 安全のため、構造体を直接キャストする
    let data_ptr = u64::from_le_bytes([
        args_bytes[16], args_bytes[17], args_bytes[18], args_bytes[19],
        args_bytes[20], args_bytes[21], args_bytes[22], args_bytes[23],
    ]);
    let data_len = u64::from_le_bytes([
        args_bytes[24], args_bytes[25], args_bytes[26], args_bytes[27],
        args_bytes[28], args_bytes[29], args_bytes[30], args_bytes[31],
    ]);

    let data_slice = user_slice_from_args(data_ptr, data_len)?;
    let data = data_slice.as_slice();

    crate::netstack::udp_send_to(socket_id, dst_ip, dst_port, data)
        .map_err(|_| SyscallError::Other)?;
    Ok(0)
}

/// SYS_NET_UDP_RECV_FROM: UDP データ受信
///
/// 引数:
///   arg1 — UdpRecvFromArgs 構造体ポインタ（ユーザー空間）
///
/// 戻り値: 受信バイト数（成功）、負（エラー）
pub(crate) fn sys_net_udp_recv_from(arg1: u64) -> Result<u64, SyscallError> {
    // wait_net_condition で待ちに入るため、割り込みを有効化する
    x86_64::instructions::interrupts::enable();
    let args_size = core::mem::size_of::<sabos_syscall::UdpRecvFromArgs>();
    let args_slice = user_slice_from_args(arg1, args_size as u64)?;
    let args_bytes = args_slice.as_slice();

    let socket_id = u32::from_le_bytes([args_bytes[0], args_bytes[1], args_bytes[2], args_bytes[3]]);
    // _pad at [4..8]
    let buf_ptr = u64::from_le_bytes([
        args_bytes[8], args_bytes[9], args_bytes[10], args_bytes[11],
        args_bytes[12], args_bytes[13], args_bytes[14], args_bytes[15],
    ]);
    let buf_len = u64::from_le_bytes([
        args_bytes[16], args_bytes[17], args_bytes[18], args_bytes[19],
        args_bytes[20], args_bytes[21], args_bytes[22], args_bytes[23],
    ]);
    let timeout_ms = u64::from_le_bytes([
        args_bytes[24], args_bytes[25], args_bytes[26], args_bytes[27],
        args_bytes[28], args_bytes[29], args_bytes[30], args_bytes[31],
    ]);
    let src_info_ptr = u64::from_le_bytes([
        args_bytes[32], args_bytes[33], args_bytes[34], args_bytes[35],
        args_bytes[36], args_bytes[37], args_bytes[38], args_bytes[39],
    ]);

    match crate::netstack::udp_recv_from(socket_id, timeout_ms) {
        Ok((src_ip, src_port, data)) => {
            let buf_slice = user_slice_from_args(buf_ptr, buf_len)?;
            let buf = buf_slice.as_mut_slice();
            let copy_len = core::cmp::min(data.len(), buf.len());
            buf[..copy_len].copy_from_slice(&data[..copy_len]);

            // src_info: [ip0, ip1, ip2, ip3, port_lo, port_hi]
            let src_info_slice = user_slice_from_args(src_info_ptr, 6)?;
            let src_info = src_info_slice.as_mut_slice();
            src_info[0..4].copy_from_slice(&src_ip);
            src_info[4..6].copy_from_slice(&src_port.to_le_bytes());

            Ok(copy_len as u64)
        }
        Err("timeout") => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::Other),
    }
}

/// SYS_NET_UDP_CLOSE: UDP ソケットクローズ
///
/// 引数:
///   arg1 — socket_id
///
/// 戻り値: 0（成功）、負（エラー）
pub(crate) fn sys_net_udp_close(arg1: u64) -> Result<u64, SyscallError> {
    let socket_id = arg1 as u32;
    crate::netstack::udp_close(socket_id).map_err(|_| SyscallError::Other)?;
    Ok(0)
}

/// SYS_NET_PING6: IPv6 ping
///
/// 引数:
///   arg1 — 宛先 IPv6 アドレスバッファ（16 バイト、ユーザー空間）
///   arg2 — タイムアウト（ミリ秒）
///   arg3 — 応答元 IPv6 アドレス書き込み先（16 バイト、ユーザー空間）
///
/// 戻り値: 0（成功）、負（エラー）
pub(crate) fn sys_net_ping6(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    // wait_net_condition で待ちに入るため、割り込みを有効化する
    x86_64::instructions::interrupts::enable();
    let dst_slice = user_slice_from_args(arg1, 16)?;
    let dst_bytes = dst_slice.as_slice();
    let mut dst_ip = [0u8; 16];
    dst_ip.copy_from_slice(&dst_bytes[..16]);

    let timeout_ms = arg2 as u32;

    // Echo Request 送信
    crate::netstack::send_icmpv6_echo_request(&dst_ip, 0x1234, 1);

    // Echo Reply 待ち
    match crate::netstack::wait_icmpv6_echo_reply(timeout_ms as u64) {
        Ok((_id, _seq, src_ip)) => {
            let src_slice = user_slice_from_args(arg3, 16)?;
            let src_buf = src_slice.as_mut_slice();
            src_buf[..16].copy_from_slice(&src_ip);
            Ok(0)
        }
        Err(_) => Err(SyscallError::Other),
    }
}
