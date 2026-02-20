// icmp.rs — ICMP プロトコル処理
//
// ICMP Echo Request/Reply（ping）の処理を行う。
// IPv4 パケットのプロトコルディスパッチもここに含む。

use alloc::vec::Vec;

use crate::net_config::get_my_ip;
use crate::serial_println;

use super::{
    BROADCAST_MAC, ETHERTYPE_IPV4, IP_PROTO_ICMP, IP_PROTO_TCP, IP_PROTO_UDP,
    ICMP_ECHO_REPLY, ICMP_ECHO_REQUEST,
    arp_lookup, arp_update, get_my_mac, is_local_ip, send_frame, calculate_checksum,
};
use super::types::{EthernetHeader, Ipv4Header, IcmpHeader};
use super::tcp::handle_tcp;
use super::udp::handle_udp;

/// IPv4 パケットを処理する
pub(super) fn handle_ipv4(eth_header: &EthernetHeader, payload: &[u8]) {
    if payload.len() < 20 {
        return;
    }

    let ip_header = unsafe { &*(payload.as_ptr() as *const Ipv4Header) };
    let header_len = ip_header.header_length();

    if payload.len() < header_len {
        return;
    }

    // 宛先 IP がローカルでなければ無視
    if !is_local_ip(&ip_header.dst_ip) {
        return;
    }

    // 受信した IPv4 パケットの送信元 IP/MAC を ARP キャッシュに学習する。
    // これにより、ICMP Echo Reply 等の応答パケット送信時に
    // ARP Request なしで即座に MAC を解決できる。
    let src_mac = eth_header.src_mac;
    if src_mac != BROADCAST_MAC {
        arp_update(ip_header.src_ip, src_mac);
    }

    let ip_payload = &payload[header_len..];

    match ip_header.protocol {
        IP_PROTO_ICMP => {
            handle_icmp(ip_header, ip_payload);
        }
        IP_PROTO_TCP => {
            handle_tcp(ip_header, ip_payload);
        }
        IP_PROTO_UDP => {
            handle_udp(ip_header, ip_payload);
        }
        _ => {
            serial_println!("[net] net: unknown IP protocol {}", ip_header.protocol);
        }
    }
}

/// ICMP パケットを処理する
fn handle_icmp(ip_header: &Ipv4Header, payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }

    let icmp_header = unsafe { &*(payload.as_ptr() as *const IcmpHeader) };

    if icmp_header.icmp_type == ICMP_ECHO_REQUEST {
        serial_println!("[net] net: ICMP Echo Request from {}.{}.{}.{}",
            ip_header.src_ip[0], ip_header.src_ip[1],
            ip_header.src_ip[2], ip_header.src_ip[3]
        );
        send_icmp_echo_reply(ip_header, payload);
    }
}

/// ICMP Echo Reply を送信する
///
/// net_poller タスクから呼ばれるため、ブロッキングする resolve_mac() は使えない。
/// ARP キャッシュから検索し、見つからなければフォールバックでブロードキャスト MAC を使う。
/// （通常は handle_ipv4 で送信元 MAC を学習済みなのでキャッシュヒットする）
fn send_icmp_echo_reply(request_ip: &Ipv4Header, icmp_data: &[u8]) {
    let my_mac = get_my_mac();
    let dst_mac = arp_lookup(&request_ip.src_ip).unwrap_or(BROADCAST_MAC);

    let eth_header = EthernetHeader {
        dst_mac,
        src_mac: my_mac,
        ethertype: ETHERTYPE_IPV4.to_be_bytes(),
    };

    // IP ヘッダー
    let total_length = 20 + icmp_data.len();
    let ip_header = Ipv4Header {
        version_ihl: 0x45,
        tos: 0,
        total_length: (total_length as u16).to_be_bytes(),
        identification: [0, 0],
        flags_fragment: [0x40, 0x00],
        ttl: 64,
        protocol: IP_PROTO_ICMP,
        checksum: [0, 0],
        src_ip: get_my_ip(),
        dst_ip: request_ip.src_ip,
    };

    let ip_header_bytes = unsafe {
        core::slice::from_raw_parts(&ip_header as *const _ as *const u8, 20)
    };
    let ip_checksum = calculate_checksum(ip_header_bytes);

    // ICMP ヘッダーを構築（Echo Reply）
    let request_icmp = unsafe { &*(icmp_data.as_ptr() as *const IcmpHeader) };
    let mut icmp_reply = *request_icmp;
    icmp_reply.icmp_type = ICMP_ECHO_REPLY;
    icmp_reply.code = 0;
    icmp_reply.checksum = [0, 0];

    let icmp_payload = if icmp_data.len() > 8 {
        &icmp_data[8..]
    } else {
        &[]
    };

    // ICMP チェックサムを計算
    let mut icmp_for_checksum = Vec::with_capacity(icmp_data.len());
    icmp_for_checksum.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&icmp_reply as *const _ as *const u8, 8)
    });
    icmp_for_checksum.extend_from_slice(icmp_payload);
    let icmp_checksum = calculate_checksum(&icmp_for_checksum);

    // パケットを構築
    let mut packet = Vec::with_capacity(14 + 20 + icmp_data.len());

    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&eth_header as *const _ as *const u8, 14)
    });

    let mut ip_header_with_checksum = ip_header;
    ip_header_with_checksum.checksum = ip_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&ip_header_with_checksum as *const _ as *const u8, 20)
    });

    icmp_reply.checksum = icmp_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&icmp_reply as *const _ as *const u8, 8)
    });

    packet.extend_from_slice(icmp_payload);

    if send_frame(&packet).is_err() {
        serial_println!("[net] net: failed to send ICMP Echo Reply");
    } else {
        serial_println!("[net] net: sent ICMP Echo Reply");
    }
}
