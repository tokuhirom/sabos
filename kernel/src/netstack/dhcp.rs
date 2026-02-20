// dhcp.rs — DHCP クライアント
//
// DHCP (Dynamic Host Configuration Protocol) は RFC 2131 で定義された
// プロトコルで、ネットワーク上のホストに IP アドレスを自動的に割り当てる。
//
// 4 ステップのハンドシェイク:
//   1. Discover: クライアント → ブロードキャスト「IP ください」
//   2. Offer:    サーバー → クライアント「この IP はどう？」
//   3. Request:  クライアント → ブロードキャスト「その IP ください」
//   4. Ack:      サーバー → クライアント「OK、使っていいよ」
//
// QEMU SLIRP は内蔵 DHCP サーバーを持ち、デフォルトで 10.0.2.15 を割り当てる。

use alloc::vec::Vec;

use crate::serial_println;

use super::{
    BROADCAST_MAC, ETHERTYPE_IPV4, IP_PROTO_UDP,
    with_net_state, get_my_mac, send_frame, calculate_checksum,
};
use super::types::{EthernetHeader, Ipv4Header, UdpHeader};
use super::udp::{udp_bind, udp_close};

/// DHCP サーバーポート（67番）
const DHCP_SERVER_PORT: u16 = 67;
/// DHCP クライアントポート（68番）
const DHCP_CLIENT_PORT: u16 = 68;
/// DHCP マジッククッキー（options フィールドの先頭に置く）
const DHCP_MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];

// DHCP メッセージタイプ（option 53 の値）
/// DHCP Discover: IP アドレスを探す
const DHCP_MSG_DISCOVER: u8 = 1;
/// DHCP Offer: サーバーからの提案
const DHCP_MSG_OFFER: u8 = 2;
/// DHCP Request: 提案された IP を要求する
const DHCP_MSG_REQUEST: u8 = 3;
/// DHCP Ack: サーバーからの承認
const DHCP_MSG_ACK: u8 = 5;

/// DHCP レスポンスのパース結果
struct DhcpResponse {
    /// メッセージタイプ（OFFER or ACK）
    msg_type: u8,
    /// 割り当てられた IP アドレス（yiaddr フィールド）
    your_ip: [u8; 4],
    /// DHCP サーバーの IP アドレス（option 54）
    server_ip: [u8; 4],
    /// サブネットマスク（option 1）
    subnet_mask: [u8; 4],
    /// デフォルトゲートウェイ（option 3）
    gateway_ip: [u8; 4],
    /// DNS サーバー（option 6）
    dns_server_ip: [u8; 4],
}

/// DHCP 用 UDP パケット送信（送信元 IP を指定可能）
///
/// 通常の send_udp_packet() は送信元 IP に get_my_ip() を使うが、
/// DHCP Discover 時は送信元 IP = 0.0.0.0 にする必要がある。
/// また宛先は常にブロードキャスト MAC にする。
fn send_dhcp_udp_packet(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Result<(), &'static str> {
    let my_mac = get_my_mac();

    let eth_header = EthernetHeader {
        dst_mac: BROADCAST_MAC,
        src_mac: my_mac,
        ethertype: ETHERTYPE_IPV4.to_be_bytes(),
    };

    let udp_length = 8 + payload.len();
    let udp_header = UdpHeader {
        src_port: src_port.to_be_bytes(),
        dst_port: dst_port.to_be_bytes(),
        length: (udp_length as u16).to_be_bytes(),
        checksum: [0, 0], // UDP チェックサムは 0（任意）
    };

    let total_length = 20 + udp_length;
    let ip_header = Ipv4Header {
        version_ihl: 0x45,
        tos: 0,
        total_length: (total_length as u16).to_be_bytes(),
        identification: [0, 0],
        flags_fragment: [0, 0], // DHCP パケットはフラグメントしない
        ttl: 64,
        protocol: IP_PROTO_UDP,
        checksum: [0, 0],
        src_ip,
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

    let mut udp_header_copy = udp_header;
    udp_header_copy.checksum = [0, 0]; // DHCP では UDP チェックサム省略可
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&udp_header_copy as *const _ as *const u8, 8)
    });

    packet.extend_from_slice(payload);

    send_frame(&packet).map_err(|_| "send failed")
}

/// DHCP Discover パケットを構築する
///
/// DHCP パケット構造（RFC 2131）:
///   - 固定フィールド (236 バイト): op, htype, hlen, hops, xid, secs, flags, ciaddr, yiaddr, siaddr, giaddr, chaddr, sname, file
///   - マジッククッキー (4 バイト)
///   - options (可変長、TLV 形式)
fn build_dhcp_discover(mac: [u8; 6], xid: u32) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(300);

    // --- 固定フィールド ---
    pkt.push(1);  // op: BOOTREQUEST
    pkt.push(1);  // htype: Ethernet
    pkt.push(6);  // hlen: MAC アドレス長
    pkt.push(0);  // hops: 0
    pkt.extend_from_slice(&xid.to_be_bytes());  // xid: トランザクション ID
    pkt.extend_from_slice(&0u16.to_be_bytes());  // secs: 0
    pkt.extend_from_slice(&0x8000u16.to_be_bytes());  // flags: broadcast bit
    pkt.extend_from_slice(&[0; 4]);  // ciaddr: 0.0.0.0（まだ IP がない）
    pkt.extend_from_slice(&[0; 4]);  // yiaddr: 0.0.0.0
    pkt.extend_from_slice(&[0; 4]);  // siaddr: 0.0.0.0
    pkt.extend_from_slice(&[0; 4]);  // giaddr: 0.0.0.0
    pkt.extend_from_slice(&mac);     // chaddr: MAC アドレス
    pkt.extend_from_slice(&[0; 10]); // chaddr padding（16 - 6 = 10 バイト）
    pkt.extend_from_slice(&[0; 64]); // sname: 空
    pkt.extend_from_slice(&[0; 128]); // file: 空

    // --- マジッククッキー ---
    pkt.extend_from_slice(&DHCP_MAGIC_COOKIE);

    // --- Options ---
    // Option 53: DHCP Message Type = Discover (1)
    pkt.extend_from_slice(&[53, 1, DHCP_MSG_DISCOVER]);
    // Option 55: Parameter Request List（ほしい情報のリスト）
    //   1 = Subnet Mask, 3 = Router, 6 = DNS Server
    pkt.extend_from_slice(&[55, 3, 1, 3, 6]);
    // Option 255: End
    pkt.push(255);

    pkt
}

/// DHCP Request パケットを構築する
///
/// Offer で提案された IP を「この IP をください」と要求する。
fn build_dhcp_request(mac: [u8; 6], xid: u32, offered_ip: [u8; 4], server_ip: [u8; 4]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(300);

    // --- 固定フィールド ---
    pkt.push(1);  // op: BOOTREQUEST
    pkt.push(1);  // htype: Ethernet
    pkt.push(6);  // hlen: MAC アドレス長
    pkt.push(0);  // hops: 0
    pkt.extend_from_slice(&xid.to_be_bytes());  // xid: 同じトランザクション ID
    pkt.extend_from_slice(&0u16.to_be_bytes());  // secs: 0
    pkt.extend_from_slice(&0x8000u16.to_be_bytes());  // flags: broadcast bit
    pkt.extend_from_slice(&[0; 4]);  // ciaddr: 0.0.0.0（まだ確定していない）
    pkt.extend_from_slice(&[0; 4]);  // yiaddr: 0.0.0.0
    pkt.extend_from_slice(&[0; 4]);  // siaddr: 0.0.0.0
    pkt.extend_from_slice(&[0; 4]);  // giaddr: 0.0.0.0
    pkt.extend_from_slice(&mac);     // chaddr: MAC アドレス
    pkt.extend_from_slice(&[0; 10]); // chaddr padding
    pkt.extend_from_slice(&[0; 64]); // sname: 空
    pkt.extend_from_slice(&[0; 128]); // file: 空

    // --- マジッククッキー ---
    pkt.extend_from_slice(&DHCP_MAGIC_COOKIE);

    // --- Options ---
    // Option 53: DHCP Message Type = Request (3)
    pkt.extend_from_slice(&[53, 1, DHCP_MSG_REQUEST]);
    // Option 50: Requested IP Address（Offer で提案された IP）
    pkt.push(50);
    pkt.push(4);
    pkt.extend_from_slice(&offered_ip);
    // Option 54: Server Identifier（Offer を送ってきたサーバーの IP）
    pkt.push(54);
    pkt.push(4);
    pkt.extend_from_slice(&server_ip);
    // Option 55: Parameter Request List
    pkt.extend_from_slice(&[55, 3, 1, 3, 6]);
    // Option 255: End
    pkt.push(255);

    pkt
}

/// DHCP レスポンス（Offer / Ack）をパースする
///
/// DHCP パケットの固定フィールドと options を解析し、
/// IP アドレス、サブネットマスク、ゲートウェイ、DNS サーバーを取り出す。
fn parse_dhcp_response(data: &[u8]) -> Option<DhcpResponse> {
    // 最低限のサイズチェック: 固定フィールド 236 + cookie 4 = 240 バイト
    if data.len() < 240 {
        return None;
    }

    // op フィールドが 2（BOOTREPLY）であることを確認
    if data[0] != 2 {
        return None;
    }

    // yiaddr: バイト 16-19（提案された IP アドレス）
    let your_ip = [data[16], data[17], data[18], data[19]];

    // マジッククッキーの確認（バイト 236-239）
    if data[236..240] != DHCP_MAGIC_COOKIE {
        return None;
    }

    // Options をパース（バイト 240 以降）
    let mut msg_type: u8 = 0;
    let mut server_ip = [0u8; 4];
    let mut subnet_mask = [255, 255, 255, 0]; // デフォルト: /24
    let mut gateway_ip = [0u8; 4];
    let mut dns_server_ip = [0u8; 4];

    let mut i = 240;
    while i < data.len() {
        let opt_type = data[i];
        if opt_type == 255 {
            break; // End option
        }
        if opt_type == 0 {
            i += 1; // Pad option
            continue;
        }
        if i + 1 >= data.len() {
            break;
        }
        let opt_len = data[i + 1] as usize;
        if i + 2 + opt_len > data.len() {
            break;
        }
        let opt_data = &data[i + 2..i + 2 + opt_len];

        match opt_type {
            53 => {
                // DHCP Message Type
                if opt_len >= 1 {
                    msg_type = opt_data[0];
                }
            }
            1 => {
                // Subnet Mask
                if opt_len >= 4 {
                    subnet_mask = [opt_data[0], opt_data[1], opt_data[2], opt_data[3]];
                }
            }
            3 => {
                // Router (Gateway)
                if opt_len >= 4 {
                    gateway_ip = [opt_data[0], opt_data[1], opt_data[2], opt_data[3]];
                }
            }
            6 => {
                // DNS Server
                if opt_len >= 4 {
                    dns_server_ip = [opt_data[0], opt_data[1], opt_data[2], opt_data[3]];
                }
            }
            54 => {
                // Server Identifier
                if opt_len >= 4 {
                    server_ip = [opt_data[0], opt_data[1], opt_data[2], opt_data[3]];
                }
            }
            _ => {} // 未知のオプションはスキップ
        }

        i += 2 + opt_len;
    }

    if msg_type == 0 {
        return None; // メッセージタイプが不明
    }

    Some(DhcpResponse {
        msg_type,
        your_ip,
        server_ip,
        subnet_mask,
        gateway_ip,
        dns_server_ip,
    })
}

/// DHCP で IP アドレスを取得する
///
/// Discover → Offer → Request → Ack の 4 ステップを実行し、
/// 取得した設定を net_config に反映する。
///
/// 失敗してもデフォルト値（10.0.2.15 等）が残るので安全。
pub fn dhcp_discover() -> Result<(), &'static str> {
    let mac = get_my_mac();
    if mac == [0; 6] {
        return Err("no MAC address");
    }

    // トランザクション ID（簡易的に MAC の一部を使用）
    let xid: u32 = u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);

    serial_println!("[net] dhcp: starting DHCP discovery (xid=0x{:08x})", xid);

    // UDP ソケットをポート 68 にバインド
    let sock_id = udp_bind(DHCP_CLIENT_PORT)?;

    // --- Step 1: DHCP Discover 送信 ---
    let discover_pkt = build_dhcp_discover(mac, xid);
    send_dhcp_udp_packet(
        [0, 0, 0, 0],         // src_ip: 0.0.0.0（まだ IP がない）
        [255, 255, 255, 255],  // dst_ip: ブロードキャスト
        DHCP_CLIENT_PORT,
        DHCP_SERVER_PORT,
        &discover_pkt,
    )?;
    serial_println!("[net] dhcp: sent Discover");

    // --- Step 2: DHCP Offer を待つ ---
    let offer = wait_dhcp_response(sock_id, xid, DHCP_MSG_OFFER, 5000)?;
    serial_println!("[net] dhcp: received Offer: IP={}.{}.{}.{} from server {}.{}.{}.{}",
        offer.your_ip[0], offer.your_ip[1], offer.your_ip[2], offer.your_ip[3],
        offer.server_ip[0], offer.server_ip[1], offer.server_ip[2], offer.server_ip[3]
    );

    // --- Step 3: DHCP Request 送信 ---
    let request_pkt = build_dhcp_request(mac, xid, offer.your_ip, offer.server_ip);
    send_dhcp_udp_packet(
        [0, 0, 0, 0],
        [255, 255, 255, 255],
        DHCP_CLIENT_PORT,
        DHCP_SERVER_PORT,
        &request_pkt,
    )?;
    serial_println!("[net] dhcp: sent Request for {}.{}.{}.{}",
        offer.your_ip[0], offer.your_ip[1], offer.your_ip[2], offer.your_ip[3]);

    // --- Step 4: DHCP Ack を待つ ---
    let ack = wait_dhcp_response(sock_id, xid, DHCP_MSG_ACK, 5000)?;
    serial_println!("[net] dhcp: received Ack: IP={}.{}.{}.{} mask={}.{}.{}.{} gw={}.{}.{}.{} dns={}.{}.{}.{}",
        ack.your_ip[0], ack.your_ip[1], ack.your_ip[2], ack.your_ip[3],
        ack.subnet_mask[0], ack.subnet_mask[1], ack.subnet_mask[2], ack.subnet_mask[3],
        ack.gateway_ip[0], ack.gateway_ip[1], ack.gateway_ip[2], ack.gateway_ip[3],
        ack.dns_server_ip[0], ack.dns_server_ip[1], ack.dns_server_ip[2], ack.dns_server_ip[3],
    );

    // --- Step 5: ネットワーク設定を更新 ---
    crate::net_config::set_config(
        ack.your_ip,
        ack.gateway_ip,
        ack.dns_server_ip,
        ack.subnet_mask,
    );

    // UDP ソケットを閉じる
    let _ = udp_close(sock_id);

    serial_println!(
        "dhcp: configured IP={}.{}.{}.{} mask={}.{}.{}.{} gw={}.{}.{}.{} dns={}.{}.{}.{}",
        ack.your_ip[0], ack.your_ip[1], ack.your_ip[2], ack.your_ip[3],
        ack.subnet_mask[0], ack.subnet_mask[1], ack.subnet_mask[2], ack.subnet_mask[3],
        ack.gateway_ip[0], ack.gateway_ip[1], ack.gateway_ip[2], ack.gateway_ip[3],
        ack.dns_server_ip[0], ack.dns_server_ip[1], ack.dns_server_ip[2], ack.dns_server_ip[3],
    );

    Ok(())
}

/// DHCP レスポンスを UDP ソケット経由で待つ
///
/// 指定した xid とメッセージタイプ (OFFER or ACK) に一致するレスポンスを待つ。
fn wait_dhcp_response(
    sock_id: u32,
    xid: u32,
    expected_msg_type: u8,
    timeout_ms: u64,
) -> Result<DhcpResponse, &'static str> {
    let check = || {
        with_net_state(|state| {
            let sock = state.udp_sockets.iter_mut().find(|s| s.id == sock_id)?;
            // キューの先頭を覗いてパース試行
            while let Some((_src_ip, _src_port, data)) = sock.recv_queue.pop_front() {
                if let Some(resp) = parse_dhcp_response(&data) {
                    // xid の確認（バイト 4-7）
                    if data.len() >= 8 {
                        let pkt_xid = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                        if pkt_xid == xid && resp.msg_type == expected_msg_type {
                            return Some(resp);
                        }
                    }
                }
            }
            None
        })
    };

    super::wait_net_condition(timeout_ms, check).ok_or("DHCP timeout")
}
