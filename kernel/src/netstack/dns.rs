// dns.rs — DNS クライアント
//
// DNS クエリの送信とレスポンスのパースを行い、ドメイン名から IP アドレスを解決する。

use alloc::vec::Vec;

use crate::net_config::get_dns_server_ip;
use crate::serial_println;

use super::{with_net_state, kernel_rdrand64, wait_net_condition};
use super::udp::send_udp_packet;

/// DNS ポート番号
const DNS_PORT: u16 = 53;
/// DNS レコードタイプ: A (IPv4 アドレス)
const DNS_TYPE_A: u16 = 1;
/// DNS クラス: IN (Internet)
const DNS_CLASS_IN: u16 = 1;

/// DNS クエリを送信して IP アドレスを解決する
pub fn dns_lookup(domain: &str) -> Result<[u8; 4], &'static str> {
    // DNS クエリ ID をランダム化する。
    // 固定値だと DNS キャッシュポイズニングに脆弱なため。
    let query_id: u16 = kernel_rdrand64() as u16;
    // DNS ソースポートをランダム化する（エフェメラルポート範囲: 49152-65535）。
    // 固定ポートだと DNS キャッシュポイズニングに脆弱なため。
    let src_port: u16 = 49152 + (kernel_rdrand64() as u16 % (65535 - 49152));

    let query_packet = build_dns_query(query_id, domain)?;

    // 最大 2 回試行する。初回は ARP 未解決で drop される場合があるためリトライする
    for attempt in 0..2 {
        // レスポンスバッファをクリア
        with_net_state(|state| {
            state.udp_response = None;
        });

        serial_println!("[net] dns: sending query for '{}' (attempt {})", domain, attempt);
        let send_result = send_udp_packet(get_dns_server_ip(), DNS_PORT, src_port, &query_packet);
        if send_result.is_err() {
            return send_result.map(|_| [0u8; 4]);
        }

        // net_poller がパケットを処理するのを待ち、DNS レスポンスをチェックする
        let result = wait_net_condition(5000, || {
            with_net_state(|state| {
                if let Some((port, ref data)) = state.udp_response {
                    if port == src_port && data.len() >= 12 {
                        let response_id = u16::from_be_bytes([data[0], data[1]]);
                        if response_id == query_id {
                            return Some(parse_dns_response(data));
                        }
                    }
                }
                None
            })
        });

        if let Some(result) = result {
            return result;
        }
    }

    Err("DNS query timeout")
}

/// DNS クエリパケットを構築する
fn build_dns_query(query_id: u16, domain: &str) -> Result<Vec<u8>, &'static str> {
    let mut packet = Vec::with_capacity(512);

    // DNS ヘッダー (12 bytes)
    packet.extend_from_slice(&query_id.to_be_bytes());
    packet.extend_from_slice(&[0x01, 0x00]); // Flags: RD=1
    packet.extend_from_slice(&[0x00, 0x01]); // QDCOUNT: 1
    packet.extend_from_slice(&[0x00, 0x00]); // ANCOUNT: 0
    packet.extend_from_slice(&[0x00, 0x00]); // NSCOUNT: 0
    packet.extend_from_slice(&[0x00, 0x00]); // ARCOUNT: 0

    // Question Section
    for label in domain.split('.') {
        if label.len() > 63 {
            return Err("DNS label too long");
        }
        if label.is_empty() {
            continue;
        }
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0x00);

    packet.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
    packet.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());

    Ok(packet)
}

/// DNS レスポンスをパースして IP アドレスを抽出する
fn parse_dns_response(data: &[u8]) -> Result<[u8; 4], &'static str> {
    if data.len() < 12 {
        return Err("DNS response too short");
    }

    let flags = u16::from_be_bytes([data[2], data[3]]);
    let rcode = flags & 0x000F;
    if rcode != 0 {
        serial_println!("[net] dns: response error, RCODE={}", rcode);
        return Err("DNS query failed");
    }

    let qdcount = u16::from_be_bytes([data[4], data[5]]);
    let ancount = u16::from_be_bytes([data[6], data[7]]);

    serial_println!("[net] dns: response with {} questions, {} answers", qdcount, ancount);

    if ancount == 0 {
        return Err("No DNS answer");
    }

    // Question Section をスキップ
    let mut offset = 12;
    for _ in 0..qdcount {
        offset = skip_dns_name(data, offset)?;
        offset += 4;
    }

    // Answer Section をパース
    for _ in 0..ancount {
        offset = skip_dns_name(data, offset)?;

        if offset + 10 > data.len() {
            return Err("DNS answer truncated");
        }

        let rtype = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let rclass = u16::from_be_bytes([data[offset + 2], data[offset + 3]]);
        let rdlength = u16::from_be_bytes([data[offset + 8], data[offset + 9]]);
        offset += 10;

        if offset + rdlength as usize > data.len() {
            return Err("DNS RDATA truncated");
        }

        if rtype == DNS_TYPE_A && rclass == DNS_CLASS_IN && rdlength == 4 {
            let ip = [
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ];
            serial_println!("[net] dns: resolved to {}.{}.{}.{}",
                ip[0], ip[1], ip[2], ip[3]
            );
            return Ok(ip);
        }

        offset += rdlength as usize;
    }

    Err("No A record found")
}

/// DNS 名をスキップして次のフィールドのオフセットを返す
fn skip_dns_name(data: &[u8], mut offset: usize) -> Result<usize, &'static str> {
    loop {
        if offset >= data.len() {
            return Err("DNS name out of bounds");
        }

        let len = data[offset];

        if len == 0 {
            return Ok(offset + 1);
        }

        if (len & 0xC0) == 0xC0 {
            return Ok(offset + 2);
        }

        offset += 1 + len as usize;
    }
}
