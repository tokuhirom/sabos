// tcp.rs — TCP プロトコル処理
//
// TCP の 3-way ハンドシェイク、データ送受信、コネクション管理を行う。

use alloc::vec::Vec;

use crate::net_config::get_my_ip;
use crate::serial_println;

use super::{
    BROADCAST_MAC, ETHERTYPE_IPV4, IP_PROTO_TCP,
    with_net_state, arp_lookup, get_my_mac, send_frame,
    is_local_ip, calculate_checksum, wait_net_condition,
    handle_packet,
};
use super::types::{
    EthernetHeader, Ipv4Header, TcpHeader, TcpState, TcpConnection, UnackedPacket,
    TCP_FLAG_FIN, TCP_FLAG_SYN, TCP_FLAG_RST, TCP_FLAG_PSH, TCP_FLAG_ACK,
    TCP_INITIAL_RTO_TICKS,
    alloc_conn_id, alloc_local_port, find_conn_index_by_id, find_conn_index_by_tuple,
    remove_conn_by_id,
};
use super::arp::resolve_mac;

/// TCP パケットを処理する
pub(super) fn handle_tcp(ip_header: &Ipv4Header, payload: &[u8]) {
    if payload.len() < 20 {
        return;
    }

    let tcp_header = unsafe { &*(payload.as_ptr() as *const TcpHeader) };
    let header_len = tcp_header.data_offset();

    if payload.len() < header_len {
        return;
    }

    let tcp_payload = &payload[header_len..];

    let src_port = tcp_header.src_port_u16();
    let dst_port = tcp_header.dst_port_u16();
    let seq = tcp_header.seq_num_u32();
    let ack = tcp_header.ack_num_u32();
    let flags = tcp_header.flags;

    serial_println!("[net] tcp: packet from {}:{} -> :{}, seq={}, ack={}, flags={:#04x}, len={}",
        ip_header.src_ip[0], src_port, dst_port, seq, ack, flags, tcp_payload.len()
    );

    let mut send_packet: Option<([u8; 4], u16, u16, u32, u32, u8)> = None;
    let mut push_accept: Option<(u32, u16)> = None;

    with_net_state(|state| {
        let idx = find_conn_index_by_tuple(state, ip_header.src_ip, src_port, dst_port);
        if idx.is_none() {
            // リスン中なら SYN を受け付ける
            serial_println!("[net] tcp: no existing conn, listen_ports={:?}, dst_port={}", state.tcp_listen_ports, dst_port);
            if state.tcp_listen_ports.contains(&dst_port) && tcp_header.has_flag(TCP_FLAG_SYN) {
                serial_println!("[net] tcp: accepting SYN on port {}, sending SYN+ACK", dst_port);
                let id = alloc_conn_id(state);
                let mut conn = TcpConnection::new(id, dst_port, ip_header.src_ip, src_port);
                conn.state = TcpState::SynReceived;
                conn.ack_num = seq + 1;
                send_packet = Some((
                    conn.remote_ip,
                    conn.remote_port,
                    conn.local_port,
                    conn.seq_num,
                    conn.ack_num,
                    TCP_FLAG_SYN | TCP_FLAG_ACK,
                ));
                // SYN-ACK の再送情報を記録する
                let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
                conn.unacked_packet = Some(UnackedPacket {
                    seq_num: conn.seq_num,
                    ack_num: conn.ack_num,
                    flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
                    payload: Vec::new(),
                    retransmit_deadline: now + TCP_INITIAL_RTO_TICKS,
                    retransmit_count: 0,
                });
                state.tcp_connections.push(conn);
            }
        } else {
            let idx = idx.unwrap();
            let conn = &mut state.tcp_connections[idx];
            match conn.state {
                TcpState::SynSent => {
                    if tcp_header.has_flag(TCP_FLAG_SYN) && tcp_header.has_flag(TCP_FLAG_ACK) {
                        serial_println!("[net] tcp: received SYN-ACK");
                        if ack == conn.seq_num + 1 {
                            conn.seq_num = ack;
                            conn.ack_num = seq + 1;
                            conn.state = TcpState::Established;
                            conn.unacked_packet = None; // SYN が ACK されたのでクリア
                            send_packet = Some((
                                conn.remote_ip,
                                conn.remote_port,
                                conn.local_port,
                                conn.seq_num,
                                conn.ack_num,
                                TCP_FLAG_ACK,
                            ));
                            serial_println!("[net] tcp: connection established");
                        }
                    } else if tcp_header.has_flag(TCP_FLAG_RST) {
                        serial_println!("[net] tcp: connection refused (RST)");
                        conn.state = TcpState::Closed;
                    }
                }
                TcpState::SynReceived => {
                    if tcp_header.has_flag(TCP_FLAG_ACK) {
                        if ack == conn.seq_num + 1 {
                            conn.seq_num = ack;
                            conn.state = TcpState::Established;
                            conn.unacked_packet = None; // SYN-ACK が ACK されたのでクリア
                            push_accept = Some((conn.id, conn.local_port));
                            serial_println!("[net] tcp: server connection established on port {}", conn.local_port);
                        }
                    }
                }
                TcpState::Established => {
                    // ACK を受信したらデータ再送バッファをクリアする
                    if tcp_header.has_flag(TCP_FLAG_ACK) {
                        conn.unacked_packet = None;
                    }
                    if tcp_header.has_flag(TCP_FLAG_FIN) {
                        serial_println!("[net] tcp: received FIN");
                        conn.ack_num = seq + 1;
                        conn.state = TcpState::CloseWait;
                        send_packet = Some((
                            conn.remote_ip,
                            conn.remote_port,
                            conn.local_port,
                            conn.seq_num,
                            conn.ack_num,
                            TCP_FLAG_ACK,
                        ));
                    } else if !tcp_payload.is_empty() {
                        serial_println!("[net] tcp: received {} bytes of data", tcp_payload.len());
                        conn.recv_buffer.extend_from_slice(tcp_payload);
                        conn.ack_num = seq + tcp_payload.len() as u32;
                        send_packet = Some((
                            conn.remote_ip,
                            conn.remote_port,
                            conn.local_port,
                            conn.seq_num,
                            conn.ack_num,
                            TCP_FLAG_ACK,
                        ));
                    }
                }
                TcpState::FinWait1 => {
                    if tcp_header.has_flag(TCP_FLAG_ACK) {
                        conn.unacked_packet = None; // FIN が ACK されたのでクリア
                        if tcp_header.has_flag(TCP_FLAG_FIN) {
                            conn.ack_num = seq + 1;
                            conn.state = TcpState::TimeWait;
                            // TIME_WAIT タイマー: 10 秒後に接続を削除する。
                            // RFC 793 では 2MSL（通常 120 秒）だが、学習用 OS なので短めに設定。
                            // PIT は約 18.2 Hz なので、10 秒 ≈ 182 ticks。
                            const TIME_WAIT_TICKS: u64 = 182;
                            let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
                            conn.time_wait_deadline = Some(now + TIME_WAIT_TICKS);
                            send_packet = Some((
                                conn.remote_ip,
                                conn.remote_port,
                                conn.local_port,
                                conn.seq_num,
                                conn.ack_num,
                                TCP_FLAG_ACK,
                            ));
                        } else {
                            conn.state = TcpState::FinWait2;
                        }
                    }
                }
                TcpState::FinWait2 => {
                    if tcp_header.has_flag(TCP_FLAG_FIN) {
                        conn.ack_num = seq + 1;
                        conn.state = TcpState::TimeWait;
                        // TIME_WAIT タイマー設定（FinWait1 と同じ）
                        const TIME_WAIT_TICKS: u64 = 182;
                        let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
                        conn.time_wait_deadline = Some(now + TIME_WAIT_TICKS);
                        send_packet = Some((
                            conn.remote_ip,
                            conn.remote_port,
                            conn.local_port,
                            conn.seq_num,
                            conn.ack_num,
                            TCP_FLAG_ACK,
                        ));
                    }
                }
                TcpState::LastAck => {
                    if tcp_header.has_flag(TCP_FLAG_ACK) {
                        conn.state = TcpState::Closed;
                        conn.unacked_packet = None; // FIN が ACK されたのでクリア
                    }
                }
                _ => {}
            }
        }

        if let Some((id, port)) = push_accept {
            serial_println!("[net] tcp: pushing to pending_accept: conn_id={}, port={}, queue_len={}", id, port, state.tcp_pending_accept.len());
            state.tcp_pending_accept.push_back((id, port));
        }
    });

    if let Some((dst_ip, dst_port, src_port, seq_num, ack_num, flags)) = send_packet {
        serial_println!("[net] tcp: sending response to {}.{}.{}.{}:{}, flags={:#04x}", dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3], dst_port, flags);
        let result = send_tcp_packet_internal(dst_ip, dst_port, src_port, seq_num, ack_num, flags, &[]);
        serial_println!("[net] tcp: send result: {:?}", result);
    } else {
        serial_println!("[net] tcp: no response to send (SYN dropped?)");
    }
}

/// TCP パケットを送信する（内部用）
///
/// net_poller タスクから呼ばれる場合があるため、ブロッキングする resolve_mac() は使えない。
/// ARP キャッシュから検索し、見つからなければフォールバックでブロードキャスト MAC を使う。
/// 呼び出し元（tcp_connect 等）で事前に resolve_mac() を呼んでキャッシュを温めておくこと。
pub(super) fn send_tcp_packet_internal(
    dst_ip: [u8; 4],
    dst_port: u16,
    src_port: u16,
    seq_num: u32,
    ack_num: u32,
    flags: u8,
    payload: &[u8],
) -> Result<(), &'static str> {
    let my_mac = get_my_mac();
    let dst_mac = arp_lookup(&dst_ip).unwrap_or(BROADCAST_MAC);

    let eth_header = EthernetHeader {
        dst_mac,
        src_mac: my_mac,
        ethertype: ETHERTYPE_IPV4.to_be_bytes(),
    };

    let tcp_header = TcpHeader {
        src_port: src_port.to_be_bytes(),
        dst_port: dst_port.to_be_bytes(),
        seq_num: seq_num.to_be_bytes(),
        ack_num: ack_num.to_be_bytes(),
        data_offset_reserved: 0x50,
        flags,
        window: 65535u16.to_be_bytes(),
        checksum: [0, 0],
        urgent_ptr: [0, 0],
    };

    let tcp_length = 20 + payload.len();
    let total_length = 20 + tcp_length;
    let ip_header = Ipv4Header {
        version_ihl: 0x45,
        tos: 0,
        total_length: (total_length as u16).to_be_bytes(),
        identification: [0, 0],
        flags_fragment: [0x40, 0x00],
        ttl: 64,
        protocol: IP_PROTO_TCP,
        checksum: [0, 0],
        src_ip: get_my_ip(),
        dst_ip,
    };

    let ip_header_bytes = unsafe {
        core::slice::from_raw_parts(&ip_header as *const _ as *const u8, 20)
    };
    let ip_checksum = calculate_checksum(ip_header_bytes);

    let tcp_checksum = calculate_tcp_checksum(&get_my_ip(), &dst_ip, &tcp_header, payload);

    let mut packet = Vec::with_capacity(14 + 20 + tcp_length);

    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&eth_header as *const _ as *const u8, 14)
    });

    let mut ip_header_with_checksum = ip_header;
    ip_header_with_checksum.checksum = ip_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&ip_header_with_checksum as *const _ as *const u8, 20)
    });

    let mut tcp_header_with_checksum = tcp_header;
    tcp_header_with_checksum.checksum = tcp_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&tcp_header_with_checksum as *const _ as *const u8, 20)
    });

    packet.extend_from_slice(payload);

    // ローカル宛のパケットはソフトウェアループバック
    if is_local_ip(&dst_ip) {
        handle_packet(&packet);
        Ok(())
    } else {
        send_frame(&packet).map_err(|_| "send failed")
    }
}

/// TCP チェックサムを計算する（疑似ヘッダー含む）
fn calculate_tcp_checksum(
    src_ip: &[u8; 4],
    dst_ip: &[u8; 4],
    tcp_header: &TcpHeader,
    payload: &[u8],
) -> u16 {
    let tcp_len = 20 + payload.len();

    let mut data = Vec::with_capacity(12 + tcp_len);

    data.extend_from_slice(src_ip);
    data.extend_from_slice(dst_ip);
    data.push(0);
    data.push(IP_PROTO_TCP);
    data.extend_from_slice(&(tcp_len as u16).to_be_bytes());

    data.extend_from_slice(unsafe {
        core::slice::from_raw_parts(tcp_header as *const _ as *const u8, 20)
    });

    data.extend_from_slice(payload);

    calculate_checksum(&data)
}

/// TCP コネクションを確立する（3-way ハンドシェイク）
pub fn tcp_connect(dst_ip: [u8; 4], dst_port: u16) -> Result<u32, &'static str> {
    // SYN 送信前に ARP キャッシュを温めておく。
    // send_tcp_packet_internal は net_poller から呼ばれる可能性があるため
    // ブロッキングする resolve_mac() を使えない。ここで事前解決する。
    resolve_mac(&dst_ip)?;

    let (conn_id, local_port, initial_seq) = with_net_state(|state| {
        let id = alloc_conn_id(state);
        let local_port = alloc_local_port(state);
        let mut conn = TcpConnection::new(id, local_port, dst_ip, dst_port);
        conn.state = TcpState::SynSent;
        let initial_seq = conn.seq_num;
        state.tcp_connections.push(conn);
        (id, local_port, initial_seq)
    });

    serial_println!("[net] tcp: sending SYN");
    send_tcp_packet_internal(dst_ip, dst_port, local_port, initial_seq, 0, TCP_FLAG_SYN, &[])?;

    // SYN の再送情報を記録する
    let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    with_net_state(|state| {
        if let Some(idx) = find_conn_index_by_id(state, conn_id) {
            state.tcp_connections[idx].unacked_packet = Some(UnackedPacket {
                seq_num: initial_seq,
                ack_num: 0,
                flags: TCP_FLAG_SYN,
                payload: Vec::new(),
                retransmit_deadline: now + TCP_INITIAL_RTO_TICKS,
                retransmit_count: 0,
            });
        }
    });

    // net_poller がパケットを処理するのを待ち、接続状態をチェックする
    let result = wait_net_condition(5000, || {
        with_net_state(|state| {
            if let Some(idx) = find_conn_index_by_id(state, conn_id) {
                let c = &state.tcp_connections[idx];
                if c.state == TcpState::Established {
                    return Some(Ok(conn_id));
                }
                if c.state == TcpState::Closed {
                    return Some(Err("connection refused"));
                }
            }
            None
        })
    });

    match result {
        Some(Ok(id)) => return Ok(id),
        Some(Err(e)) => {
            with_net_state(|state| {
                let _ = remove_conn_by_id(state, conn_id);
            });
            return Err(e);
        }
        None => {}
    }

    with_net_state(|state| {
        let _ = remove_conn_by_id(state, conn_id);
    });
    Err("connection failed")
}

/// TCP のリッスンを開始する
pub fn tcp_listen(port: u16) -> Result<(), &'static str> {
    with_net_state(|state| {
        if !state.tcp_listen_ports.contains(&port) {
            state.tcp_listen_ports.push(port);
        }
        Ok(())
    })
}

/// TCP の accept を待つ（ポート指定）
///
/// net_poller がパケットを処理して pending_accept にエントリを追加するのを待つ。
/// timeout_ms == 0 の場合は短いデフォルトタイムアウト（100ms）を使用する。
/// これにより net_poller がパケットを処理する時間を確保する。
pub fn tcp_accept(timeout_ms: u64, listen_port: u16) -> Result<u32, &'static str> {
    // timeout_ms == 0 は「短いポーリング」の意味（旧 poll_and_handle_timeout(100) 相当）
    let effective_timeout = if timeout_ms == 0 { 100 } else { timeout_ms };

    let check = || {
        with_net_state(|state| {
            if let Some(pos) = state.tcp_pending_accept.iter().position(|(_, port)| *port == listen_port) {
                let (id, _) = state.tcp_pending_accept.remove(pos).unwrap();
                serial_println!("[net] tcp_accept: found conn_id={} for port {}", id, listen_port);
                Some(id)
            } else {
                None
            }
        })
    };

    match wait_net_condition(effective_timeout, check) {
        Some(id) => Ok(id),
        None => Err("timeout"),
    }
}

/// TCP でデータを送信する
pub fn tcp_send(conn_id: u32, data: &[u8]) -> Result<(), &'static str> {
    let (dst_ip, dst_port, local_port, seq_num, ack_num) = with_net_state(|state| {
        let idx = find_conn_index_by_id(state, conn_id).ok_or("no connection")?;
        let conn = &mut state.tcp_connections[idx];

        if conn.state != TcpState::Established {
            return Err("connection not established");
        }

        let result = (conn.remote_ip, conn.remote_port, conn.local_port,
                     conn.seq_num, conn.ack_num);
        conn.seq_num += data.len() as u32;
        Ok(result)
    })?;

    serial_println!("[net] tcp: sending {} bytes", data.len());
    send_tcp_packet_internal(dst_ip, dst_port, local_port, seq_num, ack_num, TCP_FLAG_ACK | TCP_FLAG_PSH, data)?;

    // データの再送情報を記録する
    let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    with_net_state(|state| {
        if let Some(idx) = find_conn_index_by_id(state, conn_id) {
            state.tcp_connections[idx].unacked_packet = Some(UnackedPacket {
                seq_num,
                ack_num,
                flags: TCP_FLAG_ACK | TCP_FLAG_PSH,
                payload: data.to_vec(),
                retransmit_deadline: now + TCP_INITIAL_RTO_TICKS,
                retransmit_count: 0,
            });
        }
    });

    Ok(())
}

/// TCP でデータを受信する（ブロッキング、タイムアウト付き）
///
/// net_poller がパケットを処理して recv_buffer にデータを追加するのを待つ。
/// timeout_ms == 0 の場合はデフォルトタイムアウト（5000ms）を使用する。
pub fn tcp_recv(conn_id: u32, timeout_ms: u64) -> Result<Vec<u8>, &'static str> {
    // timeout_ms == 0 は「デフォルトタイムアウト」の意味（旧コードでは 50 ループ × 100ms = 5000ms）
    let effective_timeout = if timeout_ms == 0 { 5000 } else { timeout_ms };

    let check = || {
        with_net_state(|state| {
            if let Some(idx) = find_conn_index_by_id(state, conn_id) {
                let c = &mut state.tcp_connections[idx];
                if !c.recv_buffer.is_empty() {
                    let data = core::mem::take(&mut c.recv_buffer);
                    return Some(Ok(data));
                }
                if c.state == TcpState::CloseWait || c.state == TcpState::Closed {
                    return Some(Err("connection closed"));
                }
                None
            } else {
                Some(Err("no connection"))
            }
        })
    };

    match wait_net_condition(effective_timeout, check) {
        Some(Ok(data)) => Ok(data),
        Some(Err(e)) => Err(e),
        None => Err("timeout"),
    }
}

/// TCP コネクションを閉じる
pub fn tcp_close(conn_id: u32) -> Result<(), &'static str> {
    let (dst_ip, dst_port, local_port, seq_num, ack_num) = with_net_state(|state| {
        let idx = find_conn_index_by_id(state, conn_id).ok_or("no connection")?;
        let conn = &mut state.tcp_connections[idx];

        if conn.state != TcpState::Established && conn.state != TcpState::CloseWait {
            return Err("invalid state for close");
        }

        let result = (conn.remote_ip, conn.remote_port, conn.local_port,
                     conn.seq_num, conn.ack_num);

        if conn.state == TcpState::Established {
            conn.state = TcpState::FinWait1;
        } else {
            conn.state = TcpState::LastAck;
        }
        conn.seq_num += 1;
        Ok(result)
    })?;

    serial_println!("[net] tcp: sending FIN");
    send_tcp_packet_internal(dst_ip, dst_port, local_port, seq_num, ack_num, TCP_FLAG_FIN | TCP_FLAG_ACK, &[])?;

    // FIN の再送情報を記録する
    let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    with_net_state(|state| {
        if let Some(idx) = find_conn_index_by_id(state, conn_id) {
            state.tcp_connections[idx].unacked_packet = Some(UnackedPacket {
                seq_num,
                ack_num,
                flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
                payload: Vec::new(),
                retransmit_deadline: now + TCP_INITIAL_RTO_TICKS,
                retransmit_count: 0,
            });
        }
    });

    // net_poller がパケットを処理して接続が TimeWait or Closed になるのを待つ
    let _done = wait_net_condition(5000, || {
        with_net_state(|state| {
            if let Some(idx) = find_conn_index_by_id(state, conn_id) {
                let c = &state.tcp_connections[idx];
                if c.state == TcpState::TimeWait || c.state == TcpState::Closed {
                    Some(true)
                } else {
                    None
                }
            } else {
                Some(true)
            }
        })
    });

    // TimeWait の場合は接続を残す（net_poller がタイマー期限で削除する）。
    // Closed の場合のみ即削除する。
    with_net_state(|state| {
        if let Some(idx) = find_conn_index_by_id(state, conn_id) {
            if state.tcp_connections[idx].state == TcpState::Closed {
                state.tcp_connections.remove(idx);
            }
            // TimeWait の場合はそのまま残す
        }
    });

    serial_println!("[net] tcp: connection closed");
    Ok(())
}
