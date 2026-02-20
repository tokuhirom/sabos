// udp.rs — UDP プロトコル処理
//
// UDP パケットの送受信と UDP ソケット API を提供する。

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::net_config::get_my_ip;
use crate::serial_println;

use super::{
    ETHERTYPE_IPV4, IP_PROTO_UDP,
    with_net_state, get_my_mac, send_frame,
    calculate_checksum, calculate_udp_checksum, wait_net_condition,
    UdpSocketEntry,
};
use super::types::{EthernetHeader, Ipv4Header, UdpHeader};
use super::arp::resolve_mac;

/// UDP パケットを処理する
pub(super) fn handle_udp(ip_header: &Ipv4Header, payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }

    let udp_header = unsafe { &*(payload.as_ptr() as *const UdpHeader) };
    let udp_payload = &payload[8..];

    let src_port = udp_header.src_port_u16();
    let dst_port = udp_header.dst_port_u16();

    serial_println!("[net] net: UDP packet from port {} to port {}, len={}",
        src_port, dst_port, udp_payload.len()
    );

    with_net_state(|state| {
        // 宛先ポートにバインドされた UDP ソケットがあるか探す
        if let Some(sock) = state.udp_sockets.iter_mut().find(|s| s.local_port == dst_port) {
            sock.recv_queue.push_back((ip_header.src_ip, src_port, udp_payload.to_vec()));
            return;
        }

        // バインドされたソケットがなければ、既存の DNS レスポンス処理にフォールバック
        if src_port == 53 {
            state.udp_response = Some((dst_port, udp_payload.to_vec()));
        }
    });
}

/// UDP パケットを送信する
pub fn send_udp_packet(
    dst_ip: [u8; 4],
    dst_port: u16,
    src_port: u16,
    payload: &[u8],
) -> Result<(), &'static str> {
    let my_mac = get_my_mac();
    let dst_mac = resolve_mac(&dst_ip)?;

    let eth_header = EthernetHeader {
        dst_mac,
        src_mac: my_mac,
        ethertype: ETHERTYPE_IPV4.to_be_bytes(),
    };

    let udp_length = 8 + payload.len();
    let udp_header = UdpHeader {
        src_port: src_port.to_be_bytes(),
        dst_port: dst_port.to_be_bytes(),
        length: (udp_length as u16).to_be_bytes(),
        checksum: [0, 0],
    };

    let my_ip = get_my_ip();
    let udp_checksum = calculate_udp_checksum(&my_ip, &dst_ip, &udp_header, payload);

    let total_length = 20 + udp_length;
    let ip_header = Ipv4Header {
        version_ihl: 0x45,
        tos: 0,
        total_length: (total_length as u16).to_be_bytes(),
        identification: [0, 0],
        flags_fragment: [0x40, 0x00],
        ttl: 64,
        protocol: IP_PROTO_UDP,
        checksum: [0, 0],
        src_ip: my_ip,
        dst_ip,
    };

    let ip_header_bytes = unsafe {
        core::slice::from_raw_parts(&ip_header as *const _ as *const u8, 20)
    };
    let ip_checksum = calculate_checksum(ip_header_bytes);

    let mut packet = Vec::with_capacity(14 + 20 + udp_length);

    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&eth_header as *const _ as *const u8, 14)
    });

    let mut ip_header_with_checksum = ip_header;
    ip_header_with_checksum.checksum = ip_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&ip_header_with_checksum as *const _ as *const u8, 20)
    });

    let mut udp_header_with_checksum = udp_header;
    udp_header_with_checksum.checksum = udp_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&udp_header_with_checksum as *const _ as *const u8, 8)
    });

    packet.extend_from_slice(payload);

    send_frame(&packet).map_err(|_| "send failed")
}

// ============================================================
// UDP ソケット API
// ============================================================

/// UDP ソケットをバインドする
pub fn udp_bind(port: u16) -> Result<u32, &'static str> {
    with_net_state(|state| {
        let local_port = if port == 0 {
            let p = state.udp_next_port;
            let next = state.udp_next_port.wrapping_add(1);
            state.udp_next_port = if next < 49152 { 49152 } else { next };
            p
        } else {
            if state.udp_sockets.iter().any(|s| s.local_port == port) {
                return Err("port already in use");
            }
            port
        };

        let id = super::types::alloc_conn_id(state);

        state.udp_sockets.push(UdpSocketEntry {
            id,
            local_port,
            recv_queue: VecDeque::new(),
        });

        serial_println!("[net] udp: bind socket id={} port={}", id, local_port);
        Ok(id)
    })
}

/// UDP ソケットでデータを送信する
pub fn udp_send_to(
    socket_id: u32,
    dst_ip: [u8; 4],
    dst_port: u16,
    data: &[u8],
) -> Result<(), &'static str> {
    let src_port = with_net_state(|state| {
        let sock = state
            .udp_sockets
            .iter()
            .find(|s| s.id == socket_id)
            .ok_or("no such UDP socket")?;
        Ok(sock.local_port)
    })?;

    serial_println!("[net] udp: send_to socket id={} -> {}.{}.{}.{}:{} len={}",
        socket_id, dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3], dst_port, data.len()
    );

    send_udp_packet(dst_ip, dst_port, src_port, data)
}

/// UDP ソケットからデータを受信する（ブロッキング、タイムアウト付き）
///
/// net_poller がパケットを処理して recv_queue にデータを追加するのを待つ。
/// timeout_ms == 0 の場合はデフォルトタイムアウト（5000ms）を使用する。
pub fn udp_recv_from(
    socket_id: u32,
    timeout_ms: u64,
) -> Result<([u8; 4], u16, Vec<u8>), &'static str> {
    // timeout_ms == 0 は「デフォルトタイムアウト」の意味
    let effective_timeout = if timeout_ms == 0 { 5000 } else { timeout_ms };

    let check = || {
        with_net_state(|state| {
            let sock = state
                .udp_sockets
                .iter_mut()
                .find(|s| s.id == socket_id);
            match sock {
                Some(s) => {
                    if let Some(item) = s.recv_queue.pop_front() {
                        Some(Ok(item))
                    } else {
                        None
                    }
                }
                None => Some(Err("no such UDP socket")),
            }
        })
    };

    match wait_net_condition(effective_timeout, check) {
        Some(Ok(item)) => Ok(item),
        Some(Err(e)) => Err(e),
        None => Err("timeout"),
    }
}

/// UDP ソケットを閉じる
pub fn udp_close(socket_id: u32) -> Result<(), &'static str> {
    with_net_state(|state| {
        let idx = state
            .udp_sockets
            .iter()
            .position(|s| s.id == socket_id)
            .ok_or("no such UDP socket")?;
        state.udp_sockets.remove(idx);
        serial_println!("[net] udp: close socket id={}", socket_id);
        Ok(())
    })
}

/// UDP ソケットのローカルポートを取得する
pub fn udp_local_port(socket_id: u32) -> Result<u16, &'static str> {
    with_net_state(|state| {
        let sock = state
            .udp_sockets
            .iter()
            .find(|s| s.id == socket_id)
            .ok_or("no such UDP socket")?;
        Ok(sock.local_port)
    })
}
