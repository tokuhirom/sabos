// net.rs — ネットワークプロトコルスタック
//
// Ethernet / ARP / IPv4 / ICMP の最小実装。
// ping (ICMP Echo Request) に応答できることを目標とする。
//
// ## プロトコル階層
//
// [Ethernet] → [ARP] or [IPv4] → [ICMP] / [UDP] / [TCP]
//
// ## QEMU ユーザーモードネットワーク
//
// QEMU の -netdev user (SLIRP) を使うと:
//   - ゲストのデフォルト IP: 10.0.2.15
//   - ゲートウェイ/ホスト: 10.0.2.2
//   - DNS: 10.0.2.3
//
// ホストからゲストへの直接 ping は SLIRP の制限でできないが、
// ゲスト内で ARP/ICMP が動作していることを確認できる。

use crate::serial_println;
use crate::virtio_net;
use alloc::vec::Vec;

/// ゲストの IP アドレス (QEMU user mode デフォルト)
pub const MY_IP: [u8; 4] = [10, 0, 2, 15];

/// ゲートウェイの IP アドレス
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// DNS サーバーの IP アドレス (QEMU user mode デフォルト)
pub const DNS_SERVER_IP: [u8; 4] = [10, 0, 2, 3];

/// ブロードキャスト MAC アドレス
pub const BROADCAST_MAC: [u8; 6] = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

// ============================================================
// EtherType 定数
// ============================================================

/// IPv4
const ETHERTYPE_IPV4: u16 = 0x0800;
/// ARP
const ETHERTYPE_ARP: u16 = 0x0806;

// ============================================================
// ARP 定数
// ============================================================

/// ARP リクエスト
const ARP_OP_REQUEST: u16 = 1;
/// ARP リプライ
const ARP_OP_REPLY: u16 = 2;
/// Ethernet ハードウェアタイプ
const ARP_HTYPE_ETHERNET: u16 = 1;

// ============================================================
// IP プロトコル番号
// ============================================================

/// ICMP
const IP_PROTO_ICMP: u8 = 1;
/// UDP
const IP_PROTO_UDP: u8 = 17;

// ============================================================
// ICMP タイプ
// ============================================================

/// Echo Reply
const ICMP_ECHO_REPLY: u8 = 0;
/// Echo Request
const ICMP_ECHO_REQUEST: u8 = 8;

// ============================================================
// Ethernet フレーム
// ============================================================

/// Ethernet ヘッダー (14 バイト)
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct EthernetHeader {
    /// 宛先 MAC アドレス
    pub dst_mac: [u8; 6],
    /// 送信元 MAC アドレス
    pub src_mac: [u8; 6],
    /// EtherType (ビッグエンディアン)
    pub ethertype: [u8; 2],
}

impl EthernetHeader {
    /// EtherType を u16 で取得
    pub fn ethertype_u16(&self) -> u16 {
        u16::from_be_bytes(self.ethertype)
    }

    /// バイト列からパース
    pub fn from_bytes(data: &[u8]) -> Option<&EthernetHeader> {
        if data.len() < 14 {
            return None;
        }
        Some(unsafe { &*(data.as_ptr() as *const EthernetHeader) })
    }
}

// ============================================================
// ARP パケット
// ============================================================

/// ARP パケット (28 バイト for Ethernet + IPv4)
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct ArpPacket {
    /// ハードウェアタイプ (Ethernet = 1)
    pub htype: [u8; 2],
    /// プロトコルタイプ (IPv4 = 0x0800)
    pub ptype: [u8; 2],
    /// ハードウェアアドレス長 (Ethernet = 6)
    pub hlen: u8,
    /// プロトコルアドレス長 (IPv4 = 4)
    pub plen: u8,
    /// オペレーション (Request = 1, Reply = 2)
    pub oper: [u8; 2],
    /// 送信元 MAC アドレス
    pub sha: [u8; 6],
    /// 送信元 IP アドレス
    pub spa: [u8; 4],
    /// 宛先 MAC アドレス
    pub tha: [u8; 6],
    /// 宛先 IP アドレス
    pub tpa: [u8; 4],
}

impl ArpPacket {
    pub fn oper_u16(&self) -> u16 {
        u16::from_be_bytes(self.oper)
    }
}

// ============================================================
// IPv4 ヘッダー
// ============================================================

/// IPv4 ヘッダー (20 バイト、オプションなし)
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct Ipv4Header {
    /// バージョン (4) + ヘッダー長 (5 = 20 バイト)
    pub version_ihl: u8,
    /// TOS (Type of Service)
    pub tos: u8,
    /// 全長
    pub total_length: [u8; 2],
    /// 識別子
    pub identification: [u8; 2],
    /// フラグ + フラグメントオフセット
    pub flags_fragment: [u8; 2],
    /// TTL (Time to Live)
    pub ttl: u8,
    /// プロトコル (ICMP = 1, TCP = 6, UDP = 17)
    pub protocol: u8,
    /// ヘッダーチェックサム
    pub checksum: [u8; 2],
    /// 送信元 IP アドレス
    pub src_ip: [u8; 4],
    /// 宛先 IP アドレス
    pub dst_ip: [u8; 4],
}

impl Ipv4Header {
    /// ヘッダー長 (バイト単位)
    pub fn header_length(&self) -> usize {
        ((self.version_ihl & 0x0F) as usize) * 4
    }

    /// 全長 (バイト単位)
    pub fn total_length_u16(&self) -> u16 {
        u16::from_be_bytes(self.total_length)
    }
}

// ============================================================
// ICMP ヘッダー
// ============================================================

/// ICMP ヘッダー (8 バイト for Echo)
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct IcmpHeader {
    /// ICMP タイプ
    pub icmp_type: u8,
    /// ICMP コード
    pub code: u8,
    /// チェックサム
    pub checksum: [u8; 2],
    /// Echo の場合: 識別子
    pub identifier: [u8; 2],
    /// Echo の場合: シーケンス番号
    pub sequence: [u8; 2],
}

// ============================================================
// UDP ヘッダー
// ============================================================

/// UDP ヘッダー (8 バイト)
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct UdpHeader {
    /// 送信元ポート
    pub src_port: [u8; 2],
    /// 宛先ポート
    pub dst_port: [u8; 2],
    /// UDP パケット長（ヘッダー + データ）
    pub length: [u8; 2],
    /// チェックサム（オプション、0 = 未使用）
    pub checksum: [u8; 2],
}

impl UdpHeader {
    pub fn src_port_u16(&self) -> u16 {
        u16::from_be_bytes(self.src_port)
    }

    pub fn dst_port_u16(&self) -> u16 {
        u16::from_be_bytes(self.dst_port)
    }

    pub fn length_u16(&self) -> u16 {
        u16::from_be_bytes(self.length)
    }
}

// ============================================================
// パケット処理
// ============================================================

/// 受信パケットを処理する
pub fn handle_packet(data: &[u8]) {
    if data.len() < 14 {
        return;
    }

    let eth_header = match EthernetHeader::from_bytes(data) {
        Some(h) => h,
        None => return,
    };

    let payload = &data[14..];
    let ethertype = eth_header.ethertype_u16();

    match ethertype {
        ETHERTYPE_ARP => {
            handle_arp(eth_header, payload);
        }
        ETHERTYPE_IPV4 => {
            handle_ipv4(eth_header, payload);
        }
        _ => {
            serial_println!("net: unknown ethertype {:#06x}", ethertype);
        }
    }
}

/// ARP パケットを処理する
fn handle_arp(eth_header: &EthernetHeader, payload: &[u8]) {
    if payload.len() < 28 {
        return;
    }

    let arp = unsafe { &*(payload.as_ptr() as *const ArpPacket) };

    // ARP Request で、宛先 IP が自分の場合は Reply を返す
    if arp.oper_u16() == ARP_OP_REQUEST && arp.tpa == MY_IP {
        serial_println!(
            "net: ARP Request for {}.{}.{}.{} from {}.{}.{}.{}",
            arp.tpa[0], arp.tpa[1], arp.tpa[2], arp.tpa[3],
            arp.spa[0], arp.spa[1], arp.spa[2], arp.spa[3]
        );
        send_arp_reply(arp);
    }
}

/// ARP Reply を送信する
fn send_arp_reply(request: &ArpPacket) {
    let mut drv = virtio_net::VIRTIO_NET.lock();
    let drv = match drv.as_mut() {
        Some(d) => d,
        None => return,
    };

    let my_mac = drv.mac_address;

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
        spa: MY_IP,
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
    match drv.send_packet(&packet) {
        Ok(()) => serial_println!("net: sent ARP Reply"),
        Err(e) => serial_println!("net: failed to send ARP Reply: {}", e),
    }
}

/// IPv4 パケットを処理する
fn handle_ipv4(_eth_header: &EthernetHeader, payload: &[u8]) {
    if payload.len() < 20 {
        return;
    }

    let ip_header = unsafe { &*(payload.as_ptr() as *const Ipv4Header) };
    let header_len = ip_header.header_length();

    if payload.len() < header_len {
        return;
    }

    // 宛先 IP が自分でなければ無視
    if ip_header.dst_ip != MY_IP {
        return;
    }

    let ip_payload = &payload[header_len..];

    match ip_header.protocol {
        IP_PROTO_ICMP => {
            handle_icmp(ip_header, ip_payload);
        }
        IP_PROTO_UDP => {
            handle_udp(ip_header, ip_payload);
        }
        _ => {
            serial_println!("net: unknown IP protocol {}", ip_header.protocol);
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
        serial_println!(
            "net: ICMP Echo Request from {}.{}.{}.{}",
            ip_header.src_ip[0], ip_header.src_ip[1],
            ip_header.src_ip[2], ip_header.src_ip[3]
        );
        send_icmp_echo_reply(ip_header, payload);
    }
}

/// ICMP Echo Reply を送信する
fn send_icmp_echo_reply(request_ip: &Ipv4Header, icmp_data: &[u8]) {
    let mut drv = virtio_net::VIRTIO_NET.lock();
    let drv = match drv.as_mut() {
        Some(d) => d,
        None => return,
    };

    let my_mac = drv.mac_address;

    // TODO: ARP テーブルから宛先 MAC を引くべきだが、
    // 今回は request_ip.src_ip が直近の ARP リクエスト元と仮定して
    // ブロードキャストで送る（または ARP キャッシュを実装する）
    // 簡易実装: ゲートウェイの MAC = QEMU の仮想 NIC の MAC を使う
    // QEMU SLIRP では最初のパケットで ARP が来るはずなので、
    // ここでは元のパケットの src MAC を使う（実際は外部から取得が必要）

    // Ethernet ヘッダー
    // 宛先 MAC は ARP で解決するのが正しいが、簡易的にブロードキャストを使う
    // または、リクエスト元のパケットから MAC を記憶する必要がある
    // 今回は簡略化のため、受信時に src MAC を保存していないので
    // ブロードキャストで送る
    let dst_mac = BROADCAST_MAC;

    let eth_header = EthernetHeader {
        dst_mac,
        src_mac: my_mac,
        ethertype: ETHERTYPE_IPV4.to_be_bytes(),
    };

    // IP ヘッダー
    let total_length = 20 + icmp_data.len();
    let ip_header = Ipv4Header {
        version_ihl: 0x45, // IPv4, header length = 20 bytes
        tos: 0,
        total_length: (total_length as u16).to_be_bytes(),
        identification: [0, 0],
        flags_fragment: [0x40, 0x00], // Don't Fragment
        ttl: 64,
        protocol: IP_PROTO_ICMP,
        checksum: [0, 0], // 後で計算
        src_ip: MY_IP,
        dst_ip: request_ip.src_ip,
    };

    // IP ヘッダーチェックサムを計算
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

    // ICMP ペイロード（Echo Request のデータ部分）
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

    // Ethernet ヘッダー
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&eth_header as *const _ as *const u8, 14)
    });

    // IP ヘッダー（チェックサムを設定）
    let mut ip_header_with_checksum = ip_header;
    ip_header_with_checksum.checksum = ip_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&ip_header_with_checksum as *const _ as *const u8, 20)
    });

    // ICMP ヘッダー（チェックサムを設定）
    icmp_reply.checksum = icmp_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&icmp_reply as *const _ as *const u8, 8)
    });

    // ICMP ペイロード
    packet.extend_from_slice(icmp_payload);

    // 送信
    match drv.send_packet(&packet) {
        Ok(()) => serial_println!("net: sent ICMP Echo Reply"),
        Err(e) => serial_println!("net: failed to send ICMP Echo Reply: {}", e),
    }
}

/// インターネットチェックサムを計算する
///
/// RFC 1071 に従って 16 ビット 1 の補数の和の 1 の補数を計算
fn calculate_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    // 16 ビット単位で加算
    let mut i = 0;
    while i + 1 < data.len() {
        let word = u16::from_be_bytes([data[i], data[i + 1]]);
        sum += word as u32;
        i += 2;
    }

    // 奇数バイトの場合、最後の 1 バイトを処理
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }

    // キャリーを折り返す
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    // 1 の補数
    !(sum as u16)
}

/// 受信パケットをポーリングして処理する
pub fn poll_and_handle() {
    let packet = {
        let mut drv = virtio_net::VIRTIO_NET.lock();
        match drv.as_mut() {
            Some(d) => d.receive_packet(),
            None => None,
        }
    };

    if let Some(data) = packet {
        handle_packet(&data);
    }
}

// ============================================================
// UDP 処理
// ============================================================

/// 受信した UDP レスポンスを保存するバッファ
/// DNS クエリの応答を受け取るために使用する。
/// 簡易実装のためグローバルバッファを使用。
use spin::Mutex;
static UDP_RESPONSE_BUFFER: Mutex<Option<(u16, Vec<u8>)>> = Mutex::new(None);

/// UDP パケットを処理する
fn handle_udp(_ip_header: &Ipv4Header, payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }

    let udp_header = unsafe { &*(payload.as_ptr() as *const UdpHeader) };
    let udp_payload = &payload[8..];

    let src_port = udp_header.src_port_u16();
    let dst_port = udp_header.dst_port_u16();

    serial_println!(
        "net: UDP packet from port {} to port {}, len={}",
        src_port, dst_port, udp_payload.len()
    );

    // DNS レスポンス (ポート 53 から)
    if src_port == 53 {
        // DNS レスポンスをバッファに保存
        let mut buf = UDP_RESPONSE_BUFFER.lock();
        *buf = Some((dst_port, udp_payload.to_vec()));
    }
}

/// UDP パケットを送信する
///
/// # 引数
/// - `dst_ip`: 宛先 IP アドレス
/// - `dst_port`: 宛先ポート
/// - `src_port`: 送信元ポート
/// - `payload`: UDP ペイロード
pub fn send_udp_packet(
    dst_ip: [u8; 4],
    dst_port: u16,
    src_port: u16,
    payload: &[u8],
) -> Result<(), &'static str> {
    let mut drv = virtio_net::VIRTIO_NET.lock();
    let drv = match drv.as_mut() {
        Some(d) => d,
        None => return Err("no network device"),
    };

    let my_mac = drv.mac_address;

    // Ethernet ヘッダー
    // 宛先 MAC は ARP 解決が必要だが、QEMU SLIRP では
    // ゲートウェイ (10.0.2.2) 経由で全て送られるので
    // ブロードキャストまたはゲートウェイの MAC を使う
    let dst_mac = BROADCAST_MAC;

    let eth_header = EthernetHeader {
        dst_mac,
        src_mac: my_mac,
        ethertype: ETHERTYPE_IPV4.to_be_bytes(),
    };

    // UDP ヘッダー
    let udp_length = 8 + payload.len();
    let udp_header = UdpHeader {
        src_port: src_port.to_be_bytes(),
        dst_port: dst_port.to_be_bytes(),
        length: (udp_length as u16).to_be_bytes(),
        checksum: [0, 0], // UDP チェックサムはオプション（IPv4）
    };

    // IP ヘッダー
    let total_length = 20 + udp_length;
    let ip_header = Ipv4Header {
        version_ihl: 0x45,
        tos: 0,
        total_length: (total_length as u16).to_be_bytes(),
        identification: [0, 0],
        flags_fragment: [0x40, 0x00], // Don't Fragment
        ttl: 64,
        protocol: IP_PROTO_UDP,
        checksum: [0, 0],
        src_ip: MY_IP,
        dst_ip,
    };

    // IP ヘッダーチェックサムを計算
    let ip_header_bytes = unsafe {
        core::slice::from_raw_parts(&ip_header as *const _ as *const u8, 20)
    };
    let ip_checksum = calculate_checksum(ip_header_bytes);

    // パケットを構築
    let mut packet = Vec::with_capacity(14 + 20 + udp_length);

    // Ethernet ヘッダー
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&eth_header as *const _ as *const u8, 14)
    });

    // IP ヘッダー（チェックサムを設定）
    let mut ip_header_with_checksum = ip_header;
    ip_header_with_checksum.checksum = ip_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&ip_header_with_checksum as *const _ as *const u8, 20)
    });

    // UDP ヘッダー
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&udp_header as *const _ as *const u8, 8)
    });

    // UDP ペイロード
    packet.extend_from_slice(payload);

    // 送信
    drv.send_packet(&packet)
}

// ============================================================
// DNS クライアント
// ============================================================
//
// DNS プロトコルの最小実装。
// A レコード（ドメイン名 → IPv4 アドレス）のクエリのみ対応。
//
// DNS メッセージ構造:
//   [Header (12 bytes)]
//   [Question Section]
//   [Answer Section] (レスポンスのみ)
//   [Authority Section] (通常無視)
//   [Additional Section] (通常無視)
//
// DNS ヘッダー (12 bytes):
//   - ID (2 bytes): クエリ識別子
//   - Flags (2 bytes): QR, Opcode, AA, TC, RD, RA, Z, RCODE
//   - QDCOUNT (2 bytes): Question 数
//   - ANCOUNT (2 bytes): Answer 数
//   - NSCOUNT (2 bytes): Authority 数
//   - ARCOUNT (2 bytes): Additional 数
//
// Question Entry:
//   - QNAME: ラベル形式のドメイン名 (例: \x07example\x03com\x00)
//   - QTYPE (2 bytes): 1 = A レコード
//   - QCLASS (2 bytes): 1 = IN (Internet)
//
// Answer Entry:
//   - NAME: ラベル or ポインタ
//   - TYPE (2 bytes)
//   - CLASS (2 bytes)
//   - TTL (4 bytes)
//   - RDLENGTH (2 bytes)
//   - RDATA (RDLENGTH bytes): A レコードの場合は IPv4 アドレス (4 bytes)

/// DNS ポート番号
const DNS_PORT: u16 = 53;

/// DNS レコードタイプ: A (IPv4 アドレス)
const DNS_TYPE_A: u16 = 1;

/// DNS クラス: IN (Internet)
const DNS_CLASS_IN: u16 = 1;

/// DNS クエリを送信して IP アドレスを解決する
///
/// # 引数
/// - `domain`: ドメイン名 (例: "example.com")
///
/// # 戻り値
/// - `Ok([u8; 4])`: 解決された IPv4 アドレス
/// - `Err(&str)`: エラーメッセージ
pub fn dns_lookup(domain: &str) -> Result<[u8; 4], &'static str> {
    // レスポンスバッファをクリア
    {
        let mut buf = UDP_RESPONSE_BUFFER.lock();
        *buf = None;
    }

    // DNS クエリを構築
    let query_id: u16 = 0x1234; // 固定 ID（簡易実装）
    let src_port: u16 = 12345; // 送信元ポート

    let query_packet = build_dns_query(query_id, domain)?;

    // クエリを送信
    serial_println!("dns: sending query for '{}'", domain);
    send_udp_packet(DNS_SERVER_IP, DNS_PORT, src_port, &query_packet)?;

    // レスポンスを待つ（最大 3 秒）
    for _ in 0..30 {
        // ネットワークをポーリング
        poll_and_handle();

        // レスポンスをチェック
        {
            let buf = UDP_RESPONSE_BUFFER.lock();
            if let Some((port, ref data)) = *buf {
                if port == src_port && data.len() >= 12 {
                    // レスポンスをパース
                    let response_id = u16::from_be_bytes([data[0], data[1]]);
                    if response_id == query_id {
                        return parse_dns_response(data);
                    }
                }
            }
        }

        // 100ms 待機
        for _ in 0..100000 {
            core::hint::spin_loop();
        }
    }

    Err("DNS query timeout")
}

/// DNS クエリパケットを構築する
fn build_dns_query(query_id: u16, domain: &str) -> Result<Vec<u8>, &'static str> {
    let mut packet = Vec::with_capacity(512);

    // DNS ヘッダー (12 bytes)
    packet.extend_from_slice(&query_id.to_be_bytes()); // ID
    packet.extend_from_slice(&[0x01, 0x00]); // Flags: RD=1 (Recursion Desired)
    packet.extend_from_slice(&[0x00, 0x01]); // QDCOUNT: 1
    packet.extend_from_slice(&[0x00, 0x00]); // ANCOUNT: 0
    packet.extend_from_slice(&[0x00, 0x00]); // NSCOUNT: 0
    packet.extend_from_slice(&[0x00, 0x00]); // ARCOUNT: 0

    // Question Section
    // QNAME: ドメイン名をラベル形式に変換
    // "example.com" → "\x07example\x03com\x00"
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
    packet.push(0x00); // ラベル終端

    // QTYPE: A (1)
    packet.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
    // QCLASS: IN (1)
    packet.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());

    Ok(packet)
}

/// DNS レスポンスをパースして IP アドレスを抽出する
fn parse_dns_response(data: &[u8]) -> Result<[u8; 4], &'static str> {
    if data.len() < 12 {
        return Err("DNS response too short");
    }

    // Flags をチェック
    let flags = u16::from_be_bytes([data[2], data[3]]);
    let rcode = flags & 0x000F;
    if rcode != 0 {
        serial_println!("dns: response error, RCODE={}", rcode);
        return Err("DNS query failed");
    }

    let qdcount = u16::from_be_bytes([data[4], data[5]]);
    let ancount = u16::from_be_bytes([data[6], data[7]]);

    serial_println!("dns: response with {} questions, {} answers", qdcount, ancount);

    if ancount == 0 {
        return Err("No DNS answer");
    }

    // Question Section をスキップ
    let mut offset = 12;
    for _ in 0..qdcount {
        offset = skip_dns_name(data, offset)?;
        offset += 4; // QTYPE (2) + QCLASS (2)
    }

    // Answer Section をパース
    for _ in 0..ancount {
        // NAME (ラベルまたはポインタ)
        offset = skip_dns_name(data, offset)?;

        if offset + 10 > data.len() {
            return Err("DNS answer truncated");
        }

        let rtype = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let rclass = u16::from_be_bytes([data[offset + 2], data[offset + 3]]);
        // TTL は data[offset+4..offset+8] だが無視
        let rdlength = u16::from_be_bytes([data[offset + 8], data[offset + 9]]);
        offset += 10;

        if offset + rdlength as usize > data.len() {
            return Err("DNS RDATA truncated");
        }

        // A レコード (TYPE=1, CLASS=1, RDLENGTH=4) を探す
        if rtype == DNS_TYPE_A && rclass == DNS_CLASS_IN && rdlength == 4 {
            let ip = [
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ];
            serial_println!(
                "dns: resolved to {}.{}.{}.{}",
                ip[0], ip[1], ip[2], ip[3]
            );
            return Ok(ip);
        }

        offset += rdlength as usize;
    }

    Err("No A record found")
}

/// DNS 名をスキップして次のフィールドのオフセットを返す
///
/// DNS 名はラベル形式またはポインタ形式。
/// ラベル形式: \x07example\x03com\x00
/// ポインタ形式: \xC0\x0C (上位 2 ビットが 11 ならポインタ)
fn skip_dns_name(data: &[u8], mut offset: usize) -> Result<usize, &'static str> {
    loop {
        if offset >= data.len() {
            return Err("DNS name out of bounds");
        }

        let len = data[offset];

        if len == 0 {
            // ラベル終端
            return Ok(offset + 1);
        }

        if (len & 0xC0) == 0xC0 {
            // ポインタ (2 バイト)
            return Ok(offset + 2);
        }

        // 通常のラベル
        offset += 1 + len as usize;
    }
}
