// arp.rs — ARP プロトコル処理
//
// ARP Request/Reply の送受信と MAC アドレス解決を行う。

use alloc::vec::Vec;

use crate::net_config::{get_my_ip, get_gateway_ip, get_subnet_mask};
use crate::serial_println;

use super::{
    BROADCAST_MAC, ETHERTYPE_ARP, ETHERTYPE_IPV4,
    ARP_OP_REQUEST, ARP_OP_REPLY, ARP_HTYPE_ETHERNET,
    arp_lookup, arp_update, get_my_mac, send_frame, wait_net_condition,
};
use super::types::{EthernetHeader, ArpPacket};

/// ARP パケットを処理する
///
/// ARP Request: 自分宛なら Reply を返す。送信元をキャッシュに学習する。
/// ARP Reply: 送信元をキャッシュに学習する（ARP Request の応答）。
pub(super) fn handle_arp(_eth_header: &EthernetHeader, payload: &[u8]) {
    if payload.len() < 28 {
        return;
    }

    let arp = unsafe { &*(payload.as_ptr() as *const ArpPacket) };

    // すべての ARP パケットから送信元 IP/MAC を学習する
    // （Gratuitous ARP にも対応）
    arp_update(arp.spa, arp.sha);

    match arp.oper_u16() {
        ARP_OP_REQUEST => {
            // ARP Request で、宛先 IP が自分の場合は Reply を返す
            if arp.tpa == get_my_ip() {
                serial_println!("[net] net: ARP Request for {}.{}.{}.{} from {}.{}.{}.{}",
                    arp.tpa[0], arp.tpa[1], arp.tpa[2], arp.tpa[3],
                    arp.spa[0], arp.spa[1], arp.spa[2], arp.spa[3]
                );
                send_arp_reply(arp);
            }
        }
        ARP_OP_REPLY => {
            // ARP Reply を受信（arp_update は上で済み）
            serial_println!("[net] net: ARP Reply: {}.{}.{}.{} is {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                arp.spa[0], arp.spa[1], arp.spa[2], arp.spa[3],
                arp.sha[0], arp.sha[1], arp.sha[2], arp.sha[3], arp.sha[4], arp.sha[5]
            );
        }
        _ => {}
    }
}

/// ARP Reply を送信する
fn send_arp_reply(request: &ArpPacket) {
    let my_mac = get_my_mac();

    // Ethernet ヘッダー
    let eth_header = EthernetHeader {
        dst_mac: request.sha,
        src_mac: my_mac,
        ethertype: ETHERTYPE_ARP.to_be_bytes(),
    };

    // ARP Reply
    let arp_reply = ArpPacket {
        htype: ARP_HTYPE_ETHERNET.to_be_bytes(),
        ptype: ETHERTYPE_IPV4.to_be_bytes(),
        hlen: 6,
        plen: 4,
        oper: ARP_OP_REPLY.to_be_bytes(),
        sha: my_mac,
        spa: get_my_ip(),
        tha: request.sha,
        tpa: request.spa,
    };

    // パケットを構築
    let mut packet = Vec::with_capacity(42);
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&eth_header as *const _ as *const u8, 14)
    });
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&arp_reply as *const _ as *const u8, 28)
    });

    // 送信
    if send_frame(&packet).is_err() {
        serial_println!("[net] net: failed to send ARP Reply");
    } else {
        serial_println!("[net] net: sent ARP Reply");
    }
}

/// ARP Request を送信する
///
/// 指定した IP アドレスの MAC アドレスを問い合わせる。
/// 宛先 MAC = ブロードキャスト、ターゲット MAC = 00:00:00:00:00:00（不明）。
fn send_arp_request(target_ip: [u8; 4]) {
    let my_mac = get_my_mac();

    let eth_header = EthernetHeader {
        dst_mac: BROADCAST_MAC,
        src_mac: my_mac,
        ethertype: ETHERTYPE_ARP.to_be_bytes(),
    };

    let arp_request = ArpPacket {
        htype: ARP_HTYPE_ETHERNET.to_be_bytes(),
        ptype: ETHERTYPE_IPV4.to_be_bytes(),
        hlen: 6,
        plen: 4,
        oper: ARP_OP_REQUEST.to_be_bytes(),
        sha: my_mac,
        spa: get_my_ip(),
        tha: [0; 6], // 不明（これから問い合わせる）
        tpa: target_ip,
    };

    let mut packet = Vec::with_capacity(42);
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&eth_header as *const _ as *const u8, 14)
    });
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&arp_request as *const _ as *const u8, 28)
    });

    if send_frame(&packet).is_err() {
        serial_println!("[net] net: failed to send ARP Request");
    } else {
        serial_println!("[net] net: sent ARP Request for {}.{}.{}.{}",
            target_ip[0], target_ip[1], target_ip[2], target_ip[3]
        );
    }
}

/// 宛先 IP アドレスに対応する MAC アドレスを解決する
///
/// 1. ブロードキャスト IP → ブロードキャスト MAC
/// 2. サブネット外 → ゲートウェイの MAC を解決対象にする
/// 3. ARP キャッシュを検索 → ヒットすれば返す
/// 4. ミスなら ARP Request を送信し、応答を待つ（最大 3 回リトライ）
pub fn resolve_mac(dst_ip: &[u8; 4]) -> Result<[u8; 6], &'static str> {
    // ブロードキャスト IP はそのままブロードキャスト MAC
    if *dst_ip == [255, 255, 255, 255] {
        return Ok(BROADCAST_MAC);
    }

    // サブネット判定: 10.0.2.0/24（最初の 3 バイトが一致するか）
    // サブネット外の場合はゲートウェイの MAC を解決する
    // サブネットマスクを使ってサブネット判定する
    let my_ip = get_my_ip();
    let mask = get_subnet_mask();
    let resolve_ip = if (dst_ip[0] & mask[0]) == (my_ip[0] & mask[0])
        && (dst_ip[1] & mask[1]) == (my_ip[1] & mask[1])
        && (dst_ip[2] & mask[2]) == (my_ip[2] & mask[2])
        && (dst_ip[3] & mask[3]) == (my_ip[3] & mask[3])
    {
        *dst_ip
    } else {
        get_gateway_ip()
    };

    // ARP キャッシュを検索
    if let Some(mac) = arp_lookup(&resolve_ip) {
        return Ok(mac);
    }

    // キャッシュミス: ARP Request を送信して応答を待つ
    // 最大 3 回リトライ、各回 1000ms タイムアウト
    for _ in 0..3 {
        send_arp_request(resolve_ip);

        // ARP Reply を待つ（wait_net_condition で net_poller からの wake を受け取る）
        let result = wait_net_condition(1000, || {
            arp_lookup(&resolve_ip)
        });

        if let Some(mac) = result {
            return Ok(mac);
        }
    }

    Err("ARP resolve timeout")
}
