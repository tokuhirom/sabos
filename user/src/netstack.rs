// netstack.rs — ユーザー空間ネットワークスタック
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

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

use crate::syscall_netd as syscall;

/// ゲストの IP アドレス (QEMU user mode デフォルト)
pub const MY_IP: [u8; 4] = [10, 0, 2, 15];

/// ループバック IP アドレス (127.0.0.1)
pub const LOOPBACK_IP: [u8; 4] = [127, 0, 0, 1];

/// DNS サーバーの IP アドレス (QEMU user mode デフォルト)
pub const DNS_SERVER_IP: [u8; 4] = [10, 0, 2, 3];

/// ブロードキャスト MAC アドレス
pub const BROADCAST_MAC: [u8; 6] = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

macro_rules! net_debug {
    ($($arg:tt)*) => {{
        let _ = ($($arg)*);
    }};
}

// ============================================================
// 内部状態
// ============================================================

/// UDP ソケット
///
/// UDP はコネクションレスなので、バインドしたポートで送受信を行う。
/// recv_queue にはまだ読み取られていない受信データが溜まる。
pub struct UdpSocketEntry {
    /// ソケット ID（TCP と共有の ID 空間）
    pub id: u32,
    /// バインドしているローカルポート
    pub local_port: u16,
    /// 受信キュー: (送信元 IP, 送信元ポート, データ)
    pub recv_queue: VecDeque<([u8; 4], u16, Vec<u8>)>,
}

/// ネットワークスタックの内部状態
struct NetState {
    mac: [u8; 6],
    tcp_connections: Vec<TcpConnection>,
    tcp_next_id: u32,
    tcp_next_port: u16,
    tcp_listen_port: Option<u16>,
    tcp_pending_accept: VecDeque<u32>,
    udp_response: Option<(u16, Vec<u8>)>,
    /// UDP ソケット一覧
    udp_sockets: Vec<UdpSocketEntry>,
    /// UDP エフェメラルポートの次の候補（49152〜65535）
    udp_next_port: u16,
}

struct NetStateCell(UnsafeCell<Option<NetState>>);

// Safety: netd は単一タスクで動き、並行アクセスしない前提。
unsafe impl Sync for NetStateCell {}

static NET_STATE: NetStateCell = NetStateCell(UnsafeCell::new(None));

fn net_state_mut() -> &'static mut NetState {
    // Safety: netd は単一タスクで動き、並行アクセスしない前提。
    unsafe {
        let slot = &mut *NET_STATE.0.get();
        if slot.is_none() {
            *slot = Some(NetState {
                mac: [0; 6],
                tcp_connections: Vec::new(),
                tcp_next_id: 1,
                tcp_next_port: 49152,
                tcp_listen_port: None,
                tcp_pending_accept: VecDeque::new(),
                udp_response: None,
                udp_sockets: Vec::new(),
                udp_next_port: 49152,
            });
        }
        slot.as_mut().unwrap()
    }
}

/// ネットワークスタックを初期化する（MAC 取得）
pub fn init() -> Result<(), &'static str> {
    let state = net_state_mut();
    let mut mac = [0u8; 6];
    if syscall::net_get_mac(&mut mac) < 0 {
        return Err("net_get_mac failed");
    }
    state.mac = mac;
    Ok(())
}

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
/// TCP
const IP_PROTO_TCP: u8 = 6;
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
}

// ============================================================
// TCP ヘッダーと状態管理
// ============================================================

/// TCP ヘッダー (20 バイト、オプションなし)
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |          Source Port          |       Destination Port        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                        Sequence Number                        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                    Acknowledgment Number                      |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |  Data |       |C|E|U|A|P|R|S|F|                               |
/// | Offset| Rsrvd |W|C|R|C|S|S|Y|I|            Window             |
/// |       |       |R|E|G|K|H|T|N|N|                               |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |           Checksum            |         Urgent Pointer        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct TcpHeader {
    /// 送信元ポート
    pub src_port: [u8; 2],
    /// 宛先ポート
    pub dst_port: [u8; 2],
    /// シーケンス番号
    pub seq_num: [u8; 4],
    /// 確認応答番号
    pub ack_num: [u8; 4],
    /// データオフセット (上位4ビット) + 予約 (下位4ビット)
    pub data_offset_reserved: u8,
    /// フラグ (FIN, SYN, RST, PSH, ACK, URG, ECE, CWR)
    pub flags: u8,
    /// ウィンドウサイズ
    pub window: [u8; 2],
    /// チェックサム
    pub checksum: [u8; 2],
    /// 緊急ポインタ
    pub urgent_ptr: [u8; 2],
}

/// TCP フラグ定数
const TCP_FLAG_FIN: u8 = 0x01;
const TCP_FLAG_SYN: u8 = 0x02;
const TCP_FLAG_RST: u8 = 0x04;
const TCP_FLAG_PSH: u8 = 0x08;
const TCP_FLAG_ACK: u8 = 0x10;
#[allow(dead_code)]
const TCP_FLAG_URG: u8 = 0x20;

impl TcpHeader {
    pub fn src_port_u16(&self) -> u16 {
        u16::from_be_bytes(self.src_port)
    }

    pub fn dst_port_u16(&self) -> u16 {
        u16::from_be_bytes(self.dst_port)
    }

    pub fn seq_num_u32(&self) -> u32 {
        u32::from_be_bytes(self.seq_num)
    }

    pub fn ack_num_u32(&self) -> u32 {
        u32::from_be_bytes(self.ack_num)
    }

    /// データオフセット（ヘッダー長、4バイト単位）
    pub fn data_offset(&self) -> usize {
        ((self.data_offset_reserved >> 4) as usize) * 4
    }

    pub fn has_flag(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// TCP コネクションの状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    /// 初期状態
    Closed,
    /// SYN 送信済み、SYN-ACK 待ち
    SynSent,
    /// SYN 受信済み、ACK 待ち（サーバー側）
    SynReceived,
    /// コネクション確立
    Established,
    /// FIN 送信済み、FIN-ACK 待ち
    FinWait1,
    /// FIN-ACK 受信、最終 ACK 待ち
    FinWait2,
    /// 相手から FIN 受信、ACK 送信済み
    CloseWait,
    /// FIN 送信済み（CloseWait から）
    LastAck,
    /// 最終待機（TIME_WAIT は省略）
    TimeWait,
}

/// TCP コネクション
///
/// 簡易実装だが、複数コネクションを管理できるようにする。
pub struct TcpConnection {
    /// コネクション ID
    pub id: u32,
    /// コネクション状態
    pub state: TcpState,
    /// ローカルポート
    pub local_port: u16,
    /// リモート IP アドレス
    pub remote_ip: [u8; 4],
    /// リモートポート
    pub remote_port: u16,
    /// 送信シーケンス番号（次に送るバイトの番号）
    pub seq_num: u32,
    /// 確認応答番号（次に受け取るバイトの番号）
    pub ack_num: u32,
    /// 受信バッファ
    pub recv_buffer: Vec<u8>,
}

impl TcpConnection {
    pub fn new(id: u32, local_port: u16, remote_ip: [u8; 4], remote_port: u16) -> Self {
        // 初期シーケンス番号は簡易的に固定値を使用
        // 本来はランダムにすべきだが、学習用なので省略
        let initial_seq = 1000;
        Self {
            id,
            state: TcpState::Closed,
            local_port,
            remote_ip,
            remote_port,
            seq_num: initial_seq,
            ack_num: 0,
            recv_buffer: Vec::new(),
        }
    }
}

fn alloc_conn_id(state: &mut NetState) -> u32 {
    let mut id = state.tcp_next_id;
    if id == 0 {
        id = 1;
    }
    state.tcp_next_id = state.tcp_next_id.wrapping_add(1);
    if state.tcp_next_id == 0 {
        state.tcp_next_id = 1;
    }
    id
}

fn alloc_local_port(state: &mut NetState) -> u16 {
    let port = state.tcp_next_port;
    let next = state.tcp_next_port.wrapping_add(1);
    state.tcp_next_port = if next < 49152 { 49152 } else { next };
    port
}

fn find_conn_index_by_id(state: &NetState, id: u32) -> Option<usize> {
    state.tcp_connections.iter().position(|c| c.id == id)
}

fn find_conn_index_by_tuple(
    state: &NetState,
    src_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
) -> Option<usize> {
    state.tcp_connections.iter().position(|c| {
        c.remote_ip == src_ip && c.remote_port == src_port && c.local_port == dst_port
    })
}

fn remove_conn_by_id(state: &mut NetState, id: u32) -> Option<TcpConnection> {
    if let Some(idx) = find_conn_index_by_id(state, id) {
        Some(state.tcp_connections.remove(idx))
    } else {
        None
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
            net_debug!("net: unknown ethertype {:#06x}", ethertype);
        }
    }
}

/// ARP パケットを処理する
fn handle_arp(_eth_header: &EthernetHeader, payload: &[u8]) {
    if payload.len() < 28 {
        return;
    }

    let arp = unsafe { &*(payload.as_ptr() as *const ArpPacket) };

    // ARP Request で、宛先 IP が自分の場合は Reply を返す
    if arp.oper_u16() == ARP_OP_REQUEST && arp.tpa == MY_IP {
        net_debug!(
            "net: ARP Request for {}.{}.{}.{} from {}.{}.{}.{}",
            arp.tpa[0], arp.tpa[1], arp.tpa[2], arp.tpa[3],
            arp.spa[0], arp.spa[1], arp.spa[2], arp.spa[3]
        );
        send_arp_reply(arp);
    }
}

/// ARP Reply を送信する
fn send_arp_reply(request: &ArpPacket) {
    let my_mac = net_state_mut().mac;

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
    if syscall::net_send_frame(&packet) < 0 {
        net_debug!("net: failed to send ARP Reply");
    } else {
        net_debug!("net: sent ARP Reply");
    }
}

/// IPv4 パケットを処理する
/// 指定された IP がローカル（自分宛）かどうかを判定する
///
/// MY_IP (10.0.2.15) と LOOPBACK_IP (127.0.0.1) をローカルとみなす。
fn is_local_ip(ip: &[u8; 4]) -> bool {
    *ip == MY_IP || *ip == LOOPBACK_IP
}

fn handle_ipv4(_eth_header: &EthernetHeader, payload: &[u8]) {
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
            net_debug!("net: unknown IP protocol {}", ip_header.protocol);
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
        net_debug!(
            "net: ICMP Echo Request from {}.{}.{}.{}",
            ip_header.src_ip[0], ip_header.src_ip[1],
            ip_header.src_ip[2], ip_header.src_ip[3]
        );
        send_icmp_echo_reply(ip_header, payload);
    }
}

/// ICMP Echo Reply を送信する
fn send_icmp_echo_reply(request_ip: &Ipv4Header, icmp_data: &[u8]) {
    let my_mac = net_state_mut().mac;

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
    if syscall::net_send_frame(&packet) < 0 {
        net_debug!("net: failed to send ICMP Echo Reply");
    } else {
        net_debug!("net: sent ICMP Echo Reply");
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
    let mut buf = [0u8; 1600];
    let n = syscall::net_recv_frame(&mut buf, 0);
    if n > 0 {
        let len = n as usize;
        handle_packet(&buf[..len]);
    }
}

// ============================================================
// UDP 処理
// ============================================================

/// 受信した UDP レスポンスを保存するバッファ
/// DNS クエリの応答を受け取るために使用する。
/// 簡易実装のためグローバルバッファを使用。

/// UDP パケットを処理する
///
/// 宛先ポートにバインドされた UDP ソケットがあれば recv_queue に追加する。
/// なければ既存の DNS レスポンス処理（src_port==53 → udp_response に保存）にフォールバック。
fn handle_udp(ip_header: &Ipv4Header, payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }

    let udp_header = unsafe { &*(payload.as_ptr() as *const UdpHeader) };
    let udp_payload = &payload[8..];

    let src_port = udp_header.src_port_u16();
    let dst_port = udp_header.dst_port_u16();

    net_debug!(
        "net: UDP packet from port {} to port {}, len={}",
        src_port, dst_port, udp_payload.len()
    );

    let state = net_state_mut();

    // 宛先ポートにバインドされた UDP ソケットがあるか探す
    if let Some(sock) = state.udp_sockets.iter_mut().find(|s| s.local_port == dst_port) {
        // バインドされたソケットの recv_queue に追加
        sock.recv_queue.push_back((ip_header.src_ip, src_port, udp_payload.to_vec()));
        return;
    }

    // バインドされたソケットがなければ、既存の DNS レスポンス処理にフォールバック
    // DNS レスポンス (ポート 53 から)
    if src_port == 53 {
        // DNS レスポンスをバッファに保存
        state.udp_response = Some((dst_port, udp_payload.to_vec()));
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
    let my_mac = net_state_mut().mac;

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
    if syscall::net_send_frame(&packet) < 0 {
        Err("send failed")
    } else {
        Ok(())
    }
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
    net_state_mut().udp_response = None;

    // DNS クエリを構築
    let query_id: u16 = 0x1234; // 固定 ID（簡易実装）
    let src_port: u16 = 12345; // 送信元ポート

    let query_packet = build_dns_query(query_id, domain)?;

    // クエリを送信
    net_debug!("dns: sending query for '{}'", domain);
    send_udp_packet(DNS_SERVER_IP, DNS_PORT, src_port, &query_packet)?;

    // レスポンスを待つ（最大 3 秒）
    for _ in 0..30 {
        // ネットワークをポーリング
        poll_and_handle();

        // レスポンスをチェック
        if let Some((port, ref data)) = net_state_mut().udp_response {
            if port == src_port && data.len() >= 12 {
                // レスポンスをパース
                let response_id = u16::from_be_bytes([data[0], data[1]]);
                if response_id == query_id {
                    return parse_dns_response(data);
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
        net_debug!("dns: response error, RCODE={}", rcode);
        return Err("DNS query failed");
    }

    let qdcount = u16::from_be_bytes([data[4], data[5]]);
    let ancount = u16::from_be_bytes([data[6], data[7]]);

    net_debug!("dns: response with {} questions, {} answers", qdcount, ancount);

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
            net_debug!(
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

// ============================================================
// TCP クライアント
// ============================================================
//
// TCP (Transmission Control Protocol) はコネクション指向の
// 信頼性のあるストリームプロトコル。
//
// ## 3-way ハンドシェイク
//
// クライアント → サーバー: SYN (seq=x)
// サーバー → クライアント: SYN-ACK (seq=y, ack=x+1)
// クライアント → サーバー: ACK (seq=x+1, ack=y+1)
//
// ## コネクション終了 (4-way)
//
// クライアント → サーバー: FIN
// サーバー → クライアント: ACK
// サーバー → クライアント: FIN
// クライアント → サーバー: ACK

/// TCP パケットを処理する
fn handle_tcp(ip_header: &Ipv4Header, payload: &[u8]) {
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

    net_debug!(
        "tcp: packet from {}:{} -> :{}, seq={}, ack={}, flags={:#04x}, len={}",
        ip_header.src_ip[0], src_port, dst_port, seq, ack, flags, tcp_payload.len()
    );

    let mut send_packet: Option<([u8; 4], u16, u16, u32, u32, u8)> = None;
    let mut push_accept: Option<u32> = None;
    {
        let state = net_state_mut();
        let idx = find_conn_index_by_tuple(state, ip_header.src_ip, src_port, dst_port);
        if idx.is_none() {
            // リスン中なら SYN を受け付ける
            if let Some(listen_port) = state.tcp_listen_port {
                if dst_port == listen_port && tcp_header.has_flag(TCP_FLAG_SYN) {
                    let id = alloc_conn_id(state);
                    let mut conn = TcpConnection::new(id, listen_port, ip_header.src_ip, src_port);
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
                    state.tcp_connections.push(conn);
                }
            }
        } else {
            let idx = idx.unwrap();
            let conn = &mut state.tcp_connections[idx];
            match conn.state {
                TcpState::SynSent => {
                    if tcp_header.has_flag(TCP_FLAG_SYN) && tcp_header.has_flag(TCP_FLAG_ACK) {
                        net_debug!("tcp: received SYN-ACK");
                        if ack == conn.seq_num + 1 {
                            conn.seq_num = ack;
                            conn.ack_num = seq + 1;
                            conn.state = TcpState::Established;
                            send_packet = Some((
                                conn.remote_ip,
                                conn.remote_port,
                                conn.local_port,
                                conn.seq_num,
                                conn.ack_num,
                                TCP_FLAG_ACK,
                            ));
                            net_debug!("tcp: connection established");
                        }
                    } else if tcp_header.has_flag(TCP_FLAG_RST) {
                        net_debug!("tcp: connection refused (RST)");
                        conn.state = TcpState::Closed;
                    }
                }
                TcpState::SynReceived => {
                    if tcp_header.has_flag(TCP_FLAG_ACK) {
                        if ack == conn.seq_num + 1 {
                            conn.seq_num = ack;
                            conn.state = TcpState::Established;
                            push_accept = Some(conn.id);
                            net_debug!("tcp: server connection established");
                        }
                    }
                }
                TcpState::Established => {
                    if tcp_header.has_flag(TCP_FLAG_FIN) {
                        net_debug!("tcp: received FIN");
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
                        net_debug!("tcp: received {} bytes of data", tcp_payload.len());
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
                        if tcp_header.has_flag(TCP_FLAG_FIN) {
                            conn.ack_num = seq + 1;
                            conn.state = TcpState::TimeWait;
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
                    }
                }
                _ => {}
            }
        }

        if let Some(id) = push_accept {
            state.tcp_pending_accept.push_back(id);
        }
    }

    if let Some((dst_ip, dst_port, src_port, seq_num, ack_num, flags)) = send_packet {
        let _ = send_tcp_packet_internal(dst_ip, dst_port, src_port, seq_num, ack_num, flags, &[]);
    }
}

/// TCP パケットを送信する（内部用）
fn send_tcp_packet_internal(
    dst_ip: [u8; 4],
    dst_port: u16,
    src_port: u16,
    seq_num: u32,
    ack_num: u32,
    flags: u8,
    payload: &[u8],
) -> Result<(), &'static str> {
    let my_mac = net_state_mut().mac;
    let dst_mac = BROADCAST_MAC;

    // Ethernet ヘッダー
    let eth_header = EthernetHeader {
        dst_mac,
        src_mac: my_mac,
        ethertype: ETHERTYPE_IPV4.to_be_bytes(),
    };

    // TCP ヘッダー (20 バイト、オプションなし)
    let tcp_header = TcpHeader {
        src_port: src_port.to_be_bytes(),
        dst_port: dst_port.to_be_bytes(),
        seq_num: seq_num.to_be_bytes(),
        ack_num: ack_num.to_be_bytes(),
        data_offset_reserved: 0x50, // 5 * 4 = 20 bytes
        flags,
        window: 65535u16.to_be_bytes(), // 最大ウィンドウサイズ
        checksum: [0, 0], // 後で計算
        urgent_ptr: [0, 0],
    };

    // IP ヘッダー
    let tcp_length = 20 + payload.len();
    let total_length = 20 + tcp_length;
    let ip_header = Ipv4Header {
        version_ihl: 0x45,
        tos: 0,
        total_length: (total_length as u16).to_be_bytes(),
        identification: [0, 0],
        flags_fragment: [0x40, 0x00], // Don't Fragment
        ttl: 64,
        protocol: IP_PROTO_TCP,
        checksum: [0, 0],
        src_ip: MY_IP,
        dst_ip,
    };

    // IP ヘッダーチェックサムを計算
    let ip_header_bytes = unsafe {
        core::slice::from_raw_parts(&ip_header as *const _ as *const u8, 20)
    };
    let ip_checksum = calculate_checksum(ip_header_bytes);


    // TCP チェックサムを計算（疑似ヘッダー + TCP ヘッダー + データ）
    let tcp_checksum = calculate_tcp_checksum(
        &MY_IP,
        &dst_ip,
        &tcp_header,
        payload,
    );


    // パケットを構築
    let mut packet = Vec::with_capacity(14 + 20 + tcp_length);

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

    // TCP ヘッダー（チェックサムを設定）
    let mut tcp_header_with_checksum = tcp_header;
    tcp_header_with_checksum.checksum = tcp_checksum.to_be_bytes();
    packet.extend_from_slice(unsafe {
        core::slice::from_raw_parts(&tcp_header_with_checksum as *const _ as *const u8, 20)
    });

    // TCP ペイロード
    packet.extend_from_slice(payload);

    // ローカル宛（自分自身 or ループバック）のパケットは
    // ネットワークに出さず、直接パケットハンドラに戻す（ソフトウェアループバック）
    if is_local_ip(&dst_ip) {
        handle_packet(&packet);
        Ok(())
    } else if syscall::net_send_frame(&packet) < 0 {
        Err("send failed")
    } else {
        Ok(())
    }
}

/// TCP チェックサムを計算する
///
/// TCP チェックサムは疑似ヘッダーを含めて計算する:
/// - 送信元 IP (4 bytes)
/// - 宛先 IP (4 bytes)
/// - 0x00 (1 byte)
/// - プロトコル番号 (1 byte, TCP=6)
/// - TCP 長 (2 bytes)
/// - TCP ヘッダー + データ
fn calculate_tcp_checksum(
    src_ip: &[u8; 4],
    dst_ip: &[u8; 4],
    tcp_header: &TcpHeader,
    payload: &[u8],
) -> u16 {
    let tcp_len = 20 + payload.len();

    // 疑似ヘッダー + TCP ヘッダー + データを構築
    let mut data = Vec::with_capacity(12 + tcp_len);

    // 疑似ヘッダー
    data.extend_from_slice(src_ip);
    data.extend_from_slice(dst_ip);
    data.push(0);
    data.push(IP_PROTO_TCP);
    data.extend_from_slice(&(tcp_len as u16).to_be_bytes());

    // TCP ヘッダー
    data.extend_from_slice(unsafe {
        core::slice::from_raw_parts(tcp_header as *const _ as *const u8, 20)
    });

    // ペイロード
    data.extend_from_slice(payload);

    calculate_checksum(&data)
}

/// TCP コネクションを確立する（3-way ハンドシェイク）
///
/// # 引数
/// - `dst_ip`: 宛先 IP アドレス
/// - `dst_port`: 宛先ポート
///
/// # 戻り値
/// - `Ok(conn_id)`: コネクション ID
/// - `Err(&str)`: エラー
pub fn tcp_connect(dst_ip: [u8; 4], dst_port: u16) -> Result<u32, &'static str> {
    let (conn_id, local_port, initial_seq) = {
        let state = net_state_mut();
        let id = alloc_conn_id(state);
        let local_port = alloc_local_port(state);
        let mut conn = TcpConnection::new(id, local_port, dst_ip, dst_port);
        conn.state = TcpState::SynSent;
        let initial_seq = conn.seq_num;
        state.tcp_connections.push(conn);
        (id, local_port, initial_seq)
    };

    net_debug!("tcp: sending SYN");
    send_tcp_packet_internal(
        dst_ip,
        dst_port,
        local_port,
        initial_seq,
        0,
        TCP_FLAG_SYN,
        &[],
    )?;

    for _ in 0..1000000 {
        poll_and_handle();

        let state = net_state_mut();
        if let Some(idx) = find_conn_index_by_id(state, conn_id) {
            let c = &state.tcp_connections[idx];
            if c.state == TcpState::Established {
                return Ok(conn_id);
            }
            if c.state == TcpState::Closed {
                break;
            }
        } else {
            break;
        }

        for _ in 0..10000 {
            core::hint::spin_loop();
        }
    }

    let state = net_state_mut();
    let _ = remove_conn_by_id(state, conn_id);
    Err("connection failed")
}

/// TCP のリッスンを開始する
pub fn tcp_listen(port: u16) -> Result<(), &'static str> {
    let state = net_state_mut();
    state.tcp_listen_port = Some(port);
    Ok(())
}

/// TCP の accept を待つ（複数接続対応）
pub fn tcp_accept(timeout_ms: u64) -> Result<u32, &'static str> {
    let loops = if timeout_ms == 0 {
        1
    } else {
        (timeout_ms as usize).saturating_mul(2000).max(1)
    };

    for _ in 0..loops {
        poll_and_handle();
        {
            let state = net_state_mut();
            if let Some(id) = state.tcp_pending_accept.pop_front() {
                return Ok(id);
            }
        }
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }
    Err("timeout")
}

/// TCP でデータを送信する
pub fn tcp_send(conn_id: u32, data: &[u8]) -> Result<(), &'static str> {
    let (dst_ip, dst_port, local_port, seq_num, ack_num) = {
        let state = net_state_mut();
        let idx = find_conn_index_by_id(state, conn_id).ok_or("no connection")?;
        let conn = &mut state.tcp_connections[idx];

        if conn.state != TcpState::Established {
            return Err("connection not established");
        }

        let result = (conn.remote_ip, conn.remote_port, conn.local_port,
                     conn.seq_num, conn.ack_num);
        conn.seq_num += data.len() as u32;
        result
    };

    net_debug!("tcp: sending {} bytes", data.len());
    send_tcp_packet_internal(
        dst_ip,
        dst_port,
        local_port,
        seq_num,
        ack_num,
        TCP_FLAG_ACK | TCP_FLAG_PSH,
        data,
    )
}

/// TCP でデータを受信する（ブロッキング、タイムアウト付き）
pub fn tcp_recv(conn_id: u32, timeout_ms: u64) -> Result<Vec<u8>, &'static str> {
    let loops = if timeout_ms == 0 {
        500000
    } else {
        (timeout_ms as usize).saturating_mul(2000).max(1)
    };
    for _ in 0..loops {
        poll_and_handle();

        // 受信バッファをチェック
        {
            let state = net_state_mut();
            if let Some(idx) = find_conn_index_by_id(state, conn_id) {
                let c = &mut state.tcp_connections[idx];
                if !c.recv_buffer.is_empty() {
                    let data = core::mem::take(&mut c.recv_buffer);
                    return Ok(data);
                }
                if c.state == TcpState::CloseWait || c.state == TcpState::Closed {
                    if !c.recv_buffer.is_empty() {
                        let data = core::mem::take(&mut c.recv_buffer);
                        return Ok(data);
                    }
                    return Err("connection closed");
                }
            } else {
                return Err("no connection");
            }
        }

        // 簡単なビジーウェイト
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }

    Err("timeout")
}

/// TCP コネクションを閉じる
pub fn tcp_close(conn_id: u32) -> Result<(), &'static str> {
    let (dst_ip, dst_port, local_port, seq_num, ack_num) = {
        let state = net_state_mut();
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
        conn.seq_num += 1; // FIN は 1 バイト消費
        result
    };

    // FIN を送信
    net_debug!("tcp: sending FIN");
    send_tcp_packet_internal(
        dst_ip,
        dst_port,
        local_port,
        seq_num,
        ack_num,
        TCP_FLAG_FIN | TCP_FLAG_ACK,
        &[],
    )?;

    // ACK を待つ（ポーリング）
    for _ in 0..100000 {
        poll_and_handle();

        {
            let state = net_state_mut();
            if let Some(idx) = find_conn_index_by_id(state, conn_id) {
                let c = &state.tcp_connections[idx];
                if c.state == TcpState::TimeWait || c.state == TcpState::Closed {
                    break;
                }
            }
        }

        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }

    {
        let state = net_state_mut();
        let _ = remove_conn_by_id(state, conn_id);
    }

    net_debug!("tcp: connection closed");
    Ok(())
}

// ============================================================
// UDP ソケット API
// ============================================================
//
// netd の IPC オペコード 8-11 から呼ばれる公開 API。
// DNS の既存処理（dns_lookup）は引き続き udp_response を使い、
// ここで管理する UDP ソケットとは独立に動作する。

/// UDP ソケットをバインドする
///
/// # 引数
/// - `port`: バインドするポート番号。0 の場合はエフェメラルポートを自動割り当て。
///
/// # 戻り値
/// - `Ok(socket_id)`: 割り当てられたソケット ID
/// - `Err`: ポートが既に使用中の場合など
pub fn udp_bind(port: u16) -> Result<u32, &'static str> {
    let state = net_state_mut();

    // ポートを決定（0 ならエフェメラルポート自動割り当て）
    let local_port = if port == 0 {
        let p = state.udp_next_port;
        let next = state.udp_next_port.wrapping_add(1);
        state.udp_next_port = if next < 49152 { 49152 } else { next };
        p
    } else {
        // 指定ポートが既に使用されていないか確認
        if state.udp_sockets.iter().any(|s| s.local_port == port) {
            return Err("port already in use");
        }
        port
    };

    // ID は TCP と共有のカウンタから割り当て
    let id = alloc_conn_id(state);

    state.udp_sockets.push(UdpSocketEntry {
        id,
        local_port,
        recv_queue: VecDeque::new(),
    });

    net_debug!("udp: bind socket id={} port={}", id, local_port);
    Ok(id)
}

/// UDP ソケットでデータを送信する
///
/// # 引数
/// - `socket_id`: バインド時に返されたソケット ID
/// - `dst_ip`: 宛先 IP アドレス
/// - `dst_port`: 宛先ポート
/// - `data`: 送信データ
pub fn udp_send_to(
    socket_id: u32,
    dst_ip: [u8; 4],
    dst_port: u16,
    data: &[u8],
) -> Result<(), &'static str> {
    let state = net_state_mut();
    let sock = state
        .udp_sockets
        .iter()
        .find(|s| s.id == socket_id)
        .ok_or("no such UDP socket")?;
    let src_port = sock.local_port;

    net_debug!(
        "udp: send_to socket id={} -> {}.{}.{}.{}:{} len={}",
        socket_id, dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3], dst_port, data.len()
    );

    send_udp_packet(dst_ip, dst_port, src_port, data)
}

/// UDP ソケットからデータを受信する（ブロッキング、タイムアウト付き）
///
/// # 引数
/// - `socket_id`: バインド時に返されたソケット ID
/// - `timeout_ms`: タイムアウト（ミリ秒）。0 = デフォルト 5 秒
///
/// # 戻り値
/// - `Ok((src_ip, src_port, data))`: 受信したデータと送信元情報
/// - `Err`: タイムアウトまたはソケットが見つからない場合
pub fn udp_recv_from(
    socket_id: u32,
    timeout_ms: u64,
) -> Result<([u8; 4], u16, Vec<u8>), &'static str> {
    let effective_timeout = if timeout_ms == 0 { 5000 } else { timeout_ms };
    // ポーリングループ: 約 0.5ms ごとにチェックし、合計 timeout_ms 待つ
    let loops = (effective_timeout as usize).saturating_mul(2000).max(1);

    for _ in 0..loops {
        poll_and_handle();

        {
            let state = net_state_mut();
            let sock = state
                .udp_sockets
                .iter_mut()
                .find(|s| s.id == socket_id);
            match sock {
                Some(s) => {
                    if let Some(item) = s.recv_queue.pop_front() {
                        return Ok(item);
                    }
                }
                None => return Err("no such UDP socket"),
            }
        }

        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }

    Err("timeout")
}

/// UDP ソケットを閉じる
///
/// ソケットを削除し、受信キューに残っているデータも破棄する。
pub fn udp_close(socket_id: u32) -> Result<(), &'static str> {
    let state = net_state_mut();
    let idx = state
        .udp_sockets
        .iter()
        .position(|s| s.id == socket_id)
        .ok_or("no such UDP socket")?;
    state.udp_sockets.remove(idx);
    net_debug!("udp: close socket id={}", socket_id);
    Ok(())
}

/// UDP ソケットのローカルポートを取得する
pub fn udp_local_port(socket_id: u32) -> Result<u16, &'static str> {
    let state = net_state_mut();
    let sock = state
        .udp_sockets
        .iter()
        .find(|s| s.id == socket_id)
        .ok_or("no such UDP socket")?;
    Ok(sock.local_port)
}
