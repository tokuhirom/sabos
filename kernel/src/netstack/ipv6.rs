// ipv6.rs — IPv6 / ICMPv6 / NDP プロトコル処理
//
// IPv6 パケットの処理、ICMPv6 Echo Request/Reply、
// NDP (Neighbor Discovery Protocol) の Neighbor Solicitation/Advertisement を行う。

use alloc::vec::Vec;

use crate::serial_println;

use super::{
    BROADCAST_MAC, ETHERTYPE_IPV6, IP_PROTO_ICMPV6, MY_IPV6,
    with_net_state, get_my_mac, send_frame, calculate_checksum,
};
use super::types::EthernetHeader;

/// ICMPv6 Echo Request
const ICMPV6_ECHO_REQUEST: u8 = 128;
/// ICMPv6 Echo Reply
const ICMPV6_ECHO_REPLY: u8 = 129;
/// ICMPv6 Router Advertisement (NDP)
#[allow(dead_code)]
const ICMPV6_ROUTER_ADVERTISEMENT: u8 = 134;
/// ICMPv6 Neighbor Solicitation (NDP)
const ICMPV6_NEIGHBOR_SOLICITATION: u8 = 135;
/// ICMPv6 Neighbor Advertisement (NDP)
const ICMPV6_NEIGHBOR_ADVERTISEMENT: u8 = 136;

/// IPv6 ヘッダー (40 バイト固定)
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct Ipv6Header {
    /// Version(4bit) + Traffic Class(8bit) + Flow Label(20bit)
    pub version_tc_fl: [u8; 4],
    /// ペイロード長（IPv6 ヘッダーを含まない）
    pub payload_length: [u8; 2],
    /// 次のヘッダー
    pub next_header: u8,
    /// Hop Limit
    pub hop_limit: u8,
    /// 送信元 IPv6 アドレス (128 bits)
    pub src_ip: [u8; 16],
    /// 宛先 IPv6 アドレス (128 bits)
    pub dst_ip: [u8; 16],
}

/// ICMPv6 ヘッダー (4 バイト共通部分)
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct Icmpv6Header {
    /// ICMPv6 メッセージタイプ
    pub icmpv6_type: u8,
    /// メッセージコード
    pub code: u8,
    /// チェックサム（IPv6 疑似ヘッダー含む）
    pub checksum: [u8; 2],
}

/// IPv6 パケットを処理する
pub(super) fn handle_ipv6(_eth_header: &EthernetHeader, payload: &[u8]) {
    if payload.len() < 40 {
        return;
    }

    let ipv6_header = unsafe { &*(payload.as_ptr() as *const Ipv6Header) };

    if (ipv6_header.version_tc_fl[0] >> 4) != 6 {
        return;
    }

    let is_my_unicast = ipv6_header.dst_ip == MY_IPV6;
    let is_solicited_node_multicast = is_solicited_node_multicast_for(&ipv6_header.dst_ip, &MY_IPV6);
    let is_all_nodes_multicast = ipv6_header.dst_ip == [0xff, 0x02, 0,0,0,0,0,0, 0,0,0,0,0,0,0, 0x01];

    if !is_my_unicast && !is_solicited_node_multicast && !is_all_nodes_multicast {
        return;
    }

    let ipv6_payload = &payload[40..];

    match ipv6_header.next_header {
        IP_PROTO_ICMPV6 => {
            handle_icmpv6(ipv6_header, ipv6_payload);
        }
        _ => {
            serial_println!("[net] ipv6: unknown next_header {}", ipv6_header.next_header);
        }
    }
}

/// ソリシテッドノードマルチキャストアドレスの判定
fn is_solicited_node_multicast_for(multicast: &[u8; 16], target: &[u8; 16]) -> bool {
    let prefix = [0xff, 0x02, 0,0,0,0,0,0, 0,0,0, 0x01, 0xff];
    if multicast[..13] != prefix {
        return false;
    }
    multicast[13] == target[13] && multicast[14] == target[14] && multicast[15] == target[15]
}

/// ICMPv6 パケットを処理する
fn handle_icmpv6(ipv6_header: &Ipv6Header, payload: &[u8]) {
    if payload.len() < 4 {
        return;
    }

    let icmpv6 = unsafe { &*(payload.as_ptr() as *const Icmpv6Header) };

    match icmpv6.icmpv6_type {
        ICMPV6_ECHO_REQUEST => {
            serial_println!("[net] icmpv6: Echo Request received");
            send_icmpv6_echo_reply(ipv6_header, payload);
        }
        ICMPV6_ECHO_REPLY => {
            serial_println!("[net] icmpv6: Echo Reply received");
            if payload.len() >= 8 {
                let id = u16::from_be_bytes([payload[4], payload[5]]);
                let seq = u16::from_be_bytes([payload[6], payload[7]]);
                with_net_state(|state| {
                    state.icmpv6_echo_reply = Some((id, seq, ipv6_header.src_ip));
                });
            }
        }
        134 => {
            // ICMPV6_ROUTER_ADVERTISEMENT
            serial_println!("[net] icmpv6: Router Advertisement received (ignored)");
        }
        ICMPV6_NEIGHBOR_SOLICITATION => {
            serial_println!("[net] icmpv6: Neighbor Solicitation received");
            handle_ndp_neighbor_solicitation(ipv6_header, payload);
        }
        ICMPV6_NEIGHBOR_ADVERTISEMENT => {
            serial_println!("[net] icmpv6: Neighbor Advertisement received (ignored)");
        }
        _ => {
            serial_println!("[net] icmpv6: unknown type {}", icmpv6.icmpv6_type);
        }
    }
}

/// ICMPv6 Echo Reply を送信する
fn send_icmpv6_echo_reply(request_ipv6: &Ipv6Header, icmpv6_data: &[u8]) {
    if icmpv6_data.len() < 8 {
        return;
    }

    let mut reply_payload = Vec::with_capacity(icmpv6_data.len());
    reply_payload.push(ICMPV6_ECHO_REPLY);
    reply_payload.push(0);
    reply_payload.push(0);
    reply_payload.push(0);
    reply_payload.extend_from_slice(&icmpv6_data[4..]);

    let checksum = calculate_icmpv6_checksum(&MY_IPV6, &request_ipv6.src_ip, &reply_payload);
    reply_payload[2] = (checksum >> 8) as u8;
    reply_payload[3] = (checksum & 0xFF) as u8;

    send_ipv6_packet(&request_ipv6.src_ip, IP_PROTO_ICMPV6, &reply_payload);
}

/// NDP Neighbor Solicitation を処理する
fn handle_ndp_neighbor_solicitation(_ipv6_header: &Ipv6Header, payload: &[u8]) {
    if payload.len() < 24 {
        return;
    }

    let mut target = [0u8; 16];
    target.copy_from_slice(&payload[8..24]);

    if target != MY_IPV6 {
        serial_println!("[net] ndp: NS target is not MY_IPV6, ignoring");
        return;
    }

    send_ndp_neighbor_advertisement(&target, &_ipv6_header.src_ip);
}

/// NDP Neighbor Advertisement を送信する
fn send_ndp_neighbor_advertisement(target: &[u8; 16], dst_ip: &[u8; 16]) {
    let my_mac = get_my_mac();

    let mut na_payload = Vec::with_capacity(32);

    na_payload.push(ICMPV6_NEIGHBOR_ADVERTISEMENT);
    na_payload.push(0);
    na_payload.push(0);
    na_payload.push(0);

    // Flags: S=1 (Solicited), O=1 (Override)
    na_payload.push(0x60);
    na_payload.push(0x00);
    na_payload.push(0x00);
    na_payload.push(0x00);

    na_payload.extend_from_slice(target);

    // Option: Target Link-Layer Address
    na_payload.push(2);
    na_payload.push(1);
    na_payload.extend_from_slice(&my_mac);

    let checksum = calculate_icmpv6_checksum(&MY_IPV6, dst_ip, &na_payload);
    na_payload[2] = (checksum >> 8) as u8;
    na_payload[3] = (checksum & 0xFF) as u8;

    send_ipv6_packet(dst_ip, IP_PROTO_ICMPV6, &na_payload);
}

/// IPv6 パケットを送信する
fn send_ipv6_packet(dst_ip: &[u8; 16], next_header: u8, payload: &[u8]) {
    let my_mac = get_my_mac();
    let dst_mac = BROADCAST_MAC;

    let eth_header = EthernetHeader {
        dst_mac,
        src_mac: my_mac,
        ethertype: ETHERTYPE_IPV6.to_be_bytes(),
    };

    let ipv6_header = Ipv6Header {
        version_tc_fl: [0x60, 0x00, 0x00, 0x00],
        payload_length: (payload.len() as u16).to_be_bytes(),
        next_header,
        hop_limit: 64,
        src_ip: MY_IPV6,
        dst_ip: *dst_ip,
    };

    let mut packet = Vec::with_capacity(14 + 40 + payload.len());

    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&eth_header as *const _ as *const u8, 14)
    });

    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&ipv6_header as *const _ as *const u8, 40)
    });

    packet.extend_from_slice(payload);

    if send_frame(&packet).is_err() {
        serial_println!("[net] ipv6: failed to send packet");
    } else {
        serial_println!("[net] ipv6: sent packet, next_header={}, len={}", next_header, payload.len());
    }
}

/// ICMPv6 チェックサムを計算する（IPv6 疑似ヘッダー含む）
fn calculate_icmpv6_checksum(
    src_ip: &[u8; 16],
    dst_ip: &[u8; 16],
    icmpv6_data: &[u8],
) -> u16 {
    let icmpv6_len = icmpv6_data.len();

    let mut data = Vec::with_capacity(40 + icmpv6_len);

    data.extend_from_slice(src_ip);
    data.extend_from_slice(dst_ip);
    data.extend_from_slice(&(icmpv6_len as u32).to_be_bytes());
    data.push(0);
    data.push(0);
    data.push(0);
    data.push(IP_PROTO_ICMPV6);

    data.extend_from_slice(icmpv6_data);

    calculate_checksum(&data)
}

/// ICMPv6 Echo Request を送信する（ping6 用）
pub fn send_icmpv6_echo_request(dst_ip: &[u8; 16], id: u16, seq: u16) {
    with_net_state(|state| {
        state.icmpv6_echo_reply = None;
    });

    let mut echo_payload = Vec::with_capacity(16);
    echo_payload.push(ICMPV6_ECHO_REQUEST);
    echo_payload.push(0);
    echo_payload.push(0);
    echo_payload.push(0);
    echo_payload.extend_from_slice(&id.to_be_bytes());
    echo_payload.extend_from_slice(&seq.to_be_bytes());
    echo_payload.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]);

    let checksum = calculate_icmpv6_checksum(&MY_IPV6, dst_ip, &echo_payload);
    echo_payload[2] = (checksum >> 8) as u8;
    echo_payload[3] = (checksum & 0xFF) as u8;

    send_ipv6_packet(dst_ip, IP_PROTO_ICMPV6, &echo_payload);
}

/// ICMPv6 Echo Reply を待つ（タイムアウト付き）
///
/// net_poller がパケットを処理して icmpv6_echo_reply にデータを格納するのを待つ。
pub fn wait_icmpv6_echo_reply(timeout_ms: u64) -> Result<(u16, u16, [u8; 16]), &'static str> {
    let check = || {
        with_net_state(|state| {
            state.icmpv6_echo_reply.take()
        })
    };

    match super::wait_net_condition(timeout_ms, check) {
        Some(reply) => Ok(reply),
        None => Err("timeout"),
    }
}
