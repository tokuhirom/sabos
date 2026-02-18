// netstack.rs — カーネル内ネットワークスタック
//
// Ethernet / ARP / IPv4 / IPv6 / ICMP / ICMPv6 / TCP / UDP / DNS の実装。
// もともとユーザー空間の netd デーモンで動作していたが、
// システムコール直接呼び出しに移行するためカーネルに移植した。
//
// ## プロトコル階層
//
// [Ethernet] → [ARP] or [IPv4] → [ICMP] / [UDP] / [TCP]
//                      [IPv6] → [ICMPv6] (NDP, Echo)
//
// ## QEMU ユーザーモードネットワーク
//
// QEMU の -netdev user (SLIRP) を使うと:
//   - ゲストのデフォルト IP: 10.0.2.15
//   - ゲートウェイ/ホスト: 10.0.2.2
//   - DNS: 10.0.2.3
//
// ## Mutex デッドロック対策
//
// NET_STATE と VIRTIO_NET は異なる Mutex。
// net_poller_task(): VIRTIO_NET ロック→受信→ロック解放→handle_packet() の順。
// handle_packet() 内の send_arp_reply() 等: NET_STATE から MAC 取得→ロック解放→VIRTIO_NET でフレーム送信。
// MAC アドレスは初期化時に MY_MAC に保持し、NET_STATE のロック不要。

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use spin::Mutex;

use crate::net_config::GATEWAY_IP;
use crate::serial_println;

/// ゲストの IP アドレス (QEMU user mode デフォルト)
pub const MY_IP: [u8; 4] = [10, 0, 2, 15];

/// ループバック IP アドレス (127.0.0.1)
pub const LOOPBACK_IP: [u8; 4] = [127, 0, 0, 1];

/// DNS サーバーの IP アドレス (QEMU user mode デフォルト)
pub const DNS_SERVER_IP: [u8; 4] = [10, 0, 2, 3];

/// ブロードキャスト MAC アドレス
pub const BROADCAST_MAC: [u8; 6] = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

/// ゲストの IPv6 アドレス (QEMU SLIRP デフォルト: fec0::15)
pub const MY_IPV6: [u8; 16] = [0xfe, 0xc0, 0,0,0,0,0,0, 0,0,0,0,0,0,0, 0x15];

/// 自分の MAC アドレス（初期化時に設定、以降変更なし）
/// NET_STATE のロックなしでアクセスできるよう別のグローバル変数に保持する。
static MY_MAC: Mutex<[u8; 6]> = Mutex::new([0u8; 6]);

/// ネットワークスタックのログマクロ
///
/// 重要なイベント（接続確立、accept、エラー等）をシリアルに出力する。
macro_rules! net_debug {
    ($($arg:tt)*) => {{
        serial_println!("[net] {}", format_args!($($arg)*));
    }};
}

/// パケット単位の詳細トレースログ（デフォルト無効）
///
/// 有効にするにはコメントを外す。大量のシリアル出力が発生するため
/// 通常はデバッグ時のみ使用する。
#[allow(unused_macros)]
macro_rules! net_trace {
    ($($arg:tt)*) => {{
        // serial_println!("[net-trace] {}", format_args!($($arg)*));
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

/// ARP キャッシュエントリ
///
/// IP アドレスから MAC アドレスへのマッピングを保持する。
/// ARP Reply 受信時やパケット受信時に学習し、送信時に参照する。
struct ArpEntry {
    ip: [u8; 4],
    mac: [u8; 6],
}

/// ARP キャッシュの最大エントリ数
const ARP_CACHE_MAX: usize = 64;

/// ネットワークスタックの内部状態
struct NetState {
    mac: [u8; 6],
    tcp_connections: Vec<TcpConnection>,
    tcp_next_id: u32,
    tcp_next_port: u16,
    /// リスン中のポート一覧（複数サービスが同時に listen 可能）
    tcp_listen_ports: Vec<u16>,
    /// accept 待ちの接続キュー: (conn_id, local_port)
    tcp_pending_accept: VecDeque<(u32, u16)>,
    udp_response: Option<(u16, Vec<u8>)>,
    /// UDP ソケット一覧
    udp_sockets: Vec<UdpSocketEntry>,
    /// UDP エフェメラルポートの次の候補（49152〜65535）
    udp_next_port: u16,
    /// ICMPv6 Echo Reply を受信したときに保存する (id, seq, src_ip)
    icmpv6_echo_reply: Option<(u16, u16, [u8; 16])>,
    /// ネットワークイベントを待っているタスク ID のリスト。
    /// net_poller がパケットを処理した後に全 waiter を起床させる。
    /// これにより tcp_accept 等が個別にパケット受信する必要がなくなる。
    net_waiters: Vec<u64>,
    /// ARP キャッシュ: IP → MAC のマッピングテーブル
    /// 送信時に宛先 MAC を解決するために使う。
    /// ARP Reply 受信時や ARP Request 受信時（送信元）に学習する。
    arp_cache: Vec<ArpEntry>,
}

/// グローバルなネットワーク状態（spin::Mutex で保護）
static NET_STATE: Mutex<Option<NetState>> = Mutex::new(None);

/// NET_STATE のロックを取得し、初期化されていなければ初期化してから返す
fn with_net_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut NetState) -> R,
{
    let mut guard = NET_STATE.lock();
    if guard.is_none() {
        *guard = Some(NetState {
            mac: [0; 6],
            tcp_connections: Vec::new(),
            tcp_next_id: 1,
            tcp_next_port: 49152,
            tcp_listen_ports: Vec::new(),
            tcp_pending_accept: VecDeque::new(),
            udp_response: None,
            udp_sockets: Vec::new(),
            udp_next_port: 49152,
            icmpv6_echo_reply: None,
            net_waiters: Vec::new(),
            arp_cache: Vec::new(),
        });
    }
    f(guard.as_mut().unwrap())
}

/// ネットワークスタックを初期化する（MAC 取得）
pub fn init() {
    let drv = crate::virtio_net::VIRTIO_NET.lock();
    if let Some(ref d) = *drv {
        let mac = d.mac_address;
        drop(drv); // ロック解放

        *MY_MAC.lock() = mac;
        with_net_state(|state| {
            state.mac = mac;
        });
        serial_println!("netstack: initialized with MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
    } else {
        serial_println!("netstack: virtio-net not available, skipping init");
    }
}

/// MAC アドレスを取得する（ロック不要版）
fn get_my_mac() -> [u8; 6] {
    *MY_MAC.lock()
}

// ============================================================
// ARP キャッシュ操作
// ============================================================

/// ARP キャッシュから IP に対応する MAC アドレスを検索する
fn arp_lookup(ip: &[u8; 4]) -> Option<[u8; 6]> {
    with_net_state(|state| {
        for entry in &state.arp_cache {
            if entry.ip == *ip {
                return Some(entry.mac);
            }
        }
        None
    })
}

/// ARP キャッシュに IP → MAC のマッピングを追加/更新する
///
/// 既存エントリがあれば MAC を更新する。
/// キャッシュが満杯（64 エントリ）の場合は最も古いエントリ（先頭）を削除する。
fn arp_update(ip: [u8; 4], mac: [u8; 6]) {
    with_net_state(|state| {
        // 既存エントリを探して更新
        for entry in state.arp_cache.iter_mut() {
            if entry.ip == ip {
                entry.mac = mac;
                return;
            }
        }
        // 新規追加（キャッシュが満杯なら先頭を削除）
        if state.arp_cache.len() >= ARP_CACHE_MAX {
            state.arp_cache.remove(0);
        }
        state.arp_cache.push(ArpEntry { ip, mac });
    });
}

// ============================================================
// フレーム送受信ヘルパー（VIRTIO_NET 直接操作）
// ============================================================

/// Ethernet フレームを送信する
fn send_frame(data: &[u8]) -> Result<(), &'static str> {
    let mut drv = crate::virtio_net::VIRTIO_NET.lock();
    let drv = drv.as_mut().ok_or("virtio-net not available")?;
    drv.send_packet(data)
}

/// Ethernet フレームを受信する（ノンブロッキング）
/// 受信できなければ None を返す
fn recv_frame_nonblocking() -> Option<Vec<u8>> {
    let mut drv = crate::virtio_net::VIRTIO_NET.lock();
    drv.as_mut().and_then(|d| d.receive_packet())
}

/// ISR ステータスを読み取って QEMU のイベントループをキックする
fn kick_virtio_net() {
    let mut drv = crate::virtio_net::VIRTIO_NET.lock();
    if let Some(d) = drv.as_mut() {
        d.read_isr_status();
    }
}

// ============================================================
// EtherType 定数
// ============================================================

/// IPv4
const ETHERTYPE_IPV4: u16 = 0x0800;
/// ARP
const ETHERTYPE_ARP: u16 = 0x0806;
/// IPv6
const ETHERTYPE_IPV6: u16 = 0x86DD;

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
/// ICMPv6
const IP_PROTO_ICMPV6: u8 = 58;

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
        ETHERTYPE_IPV6 => {
            handle_ipv6(eth_header, payload);
        }
        _ => {
            net_debug!("net: unknown ethertype {:#06x}", ethertype);
        }
    }
}

/// ARP パケットを処理する
///
/// ARP Request: 自分宛なら Reply を返す。送信元をキャッシュに学習する。
/// ARP Reply: 送信元をキャッシュに学習する（ARP Request の応答）。
fn handle_arp(_eth_header: &EthernetHeader, payload: &[u8]) {
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
            if arp.tpa == MY_IP {
                net_debug!(
                    "net: ARP Request for {}.{}.{}.{} from {}.{}.{}.{}",
                    arp.tpa[0], arp.tpa[1], arp.tpa[2], arp.tpa[3],
                    arp.spa[0], arp.spa[1], arp.spa[2], arp.spa[3]
                );
                send_arp_reply(arp);
            }
        }
        ARP_OP_REPLY => {
            // ARP Reply を受信（arp_update は上で済み）
            net_debug!(
                "net: ARP Reply: {}.{}.{}.{} is {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
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
    if send_frame(&packet).is_err() {
        net_debug!("net: failed to send ARP Reply");
    } else {
        net_debug!("net: sent ARP Reply");
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
        spa: MY_IP,
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
        net_debug!("net: failed to send ARP Request");
    } else {
        net_debug!(
            "net: sent ARP Request for {}.{}.{}.{}",
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
    let resolve_ip = if dst_ip[0] == MY_IP[0] && dst_ip[1] == MY_IP[1] && dst_ip[2] == MY_IP[2] {
        *dst_ip
    } else {
        GATEWAY_IP
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

/// 指定された IP がローカル（自分宛）かどうかを判定する
fn is_local_ip(ip: &[u8; 4]) -> bool {
    *ip == MY_IP || *ip == LOOPBACK_IP
}

/// IPv4 パケットを処理する
fn handle_ipv4(eth_header: &EthernetHeader, payload: &[u8]) {
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
        src_ip: MY_IP,
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
        net_debug!("net: failed to send ICMP Echo Reply");
    } else {
        net_debug!("net: sent ICMP Echo Reply");
    }
}

/// インターネットチェックサムを計算する（RFC 1071）
fn calculate_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    let mut i = 0;
    while i + 1 < data.len() {
        let word = u16::from_be_bytes([data[i], data[i + 1]]);
        sum += word as u32;
        i += 2;
    }

    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }

    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    !(sum as u16)
}

/// UDP チェックサムを計算する（疑似ヘッダー含む）
fn calculate_udp_checksum(
    src_ip: &[u8; 4],
    dst_ip: &[u8; 4],
    udp_header: &UdpHeader,
    payload: &[u8],
) -> u16 {
    let udp_len = 8 + payload.len();

    let mut data = Vec::with_capacity(12 + udp_len);

    // 疑似ヘッダー
    data.extend_from_slice(src_ip);
    data.extend_from_slice(dst_ip);
    data.push(0);
    data.push(IP_PROTO_UDP);
    data.extend_from_slice(&(udp_len as u16).to_be_bytes());

    // UDP ヘッダー
    data.extend_from_slice(unsafe {
        core::slice::from_raw_parts(udp_header as *const _ as *const u8, 8)
    });

    // ペイロード
    data.extend_from_slice(payload);

    let checksum = calculate_checksum(&data);
    if checksum == 0 { 0xFFFF } else { checksum }
}

// ============================================================
// net_poller: パケット処理を集約するカーネルタスク
// ============================================================
//
// 従来は各 syscall（tcp_accept, tcp_recv 等）が個別に poll_and_handle_timeout() を
// 呼んでパケット受信・処理を行っていた。この設計だと、httpd と telnetd が同時に
// tcp_accept を呼ぶとパケットを取り合い、一方が接続を受け取れなくなる問題があった。
//
// net_poller はパケット受信・処理を専用カーネルタスクに集約し、
// 各 syscall は wait_net_condition() で条件成立を待つだけにする。
// net_poller がパケットを処理したら全 waiter を起床させ、
// 各 waiter は自分の条件をチェックする。

/// 現在のタスクをネットワーク waiter として登録する
fn register_net_waiter() {
    let task_id = crate::scheduler::current_task_id();
    with_net_state(|state| {
        if !state.net_waiters.contains(&task_id) {
            state.net_waiters.push(task_id);
        }
    });
}

/// 現在のタスクをネットワーク waiter から削除する
fn unregister_net_waiter() {
    let task_id = crate::scheduler::current_task_id();
    with_net_state(|state| {
        state.net_waiters.retain(|&id| id != task_id);
    });
}

/// 全ネットワーク waiter を起床させる
///
/// NET_STATE のロック内で waiter リストをコピーし、ロック解放後に wake_task を呼ぶ。
/// これにより NET_STATE と SCHEDULER のロック順序の問題を回避する。
fn wake_all_net_waiters() {
    let waiters: Vec<u64> = with_net_state(|state| {
        state.net_waiters.clone()
    });
    for task_id in waiters {
        crate::scheduler::wake_task(task_id);
    }
}

/// ネットワーク条件の成立を待つ汎用関数
///
/// net_poller がパケットを処理して waiter を起床させるまでスリープし、
/// 起床後に check_fn で条件をチェックする。条件が成立したら結果を返す。
/// タイムアウトに達したら None を返す。
///
/// ## 動作フロー
/// 1. 即座にチェック（既に条件が成立していれば即座に返す）
/// 2. waiter 登録
/// 3. sleep/wake ループ: 55ms ごとに自動起床 + net_poller からの wake で即起床
/// 4. タイムアウト or 条件成立で waiter 解除して返す
fn wait_net_condition<T, F>(timeout_ms: u64, check_fn: F) -> Option<T>
where
    F: Fn() -> Option<T>,
{
    // 即座チェック
    if let Some(result) = check_fn() {
        return Some(result);
    }

    if timeout_ms == 0 {
        return None;
    }

    register_net_waiter();

    let start_tick = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);

    loop {
        // 1 ティック（約 55ms）後に自動起床するようスリープ設定。
        // net_poller が wake_task を呼べばそれより早く起きる。
        let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        crate::scheduler::set_current_sleeping(now + 1);
        crate::scheduler::yield_now();

        // 起床後に条件チェック
        if let Some(result) = check_fn() {
            unregister_net_waiter();
            return Some(result);
        }

        // タイムアウトチェック
        let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
        let elapsed_ticks = now.saturating_sub(start_tick);
        let elapsed_ms = elapsed_ticks * 55;
        if elapsed_ms >= timeout_ms {
            unregister_net_waiter();
            return None;
        }
    }
}

/// ネットワークパケットを受信・処理する専用カーネルタスク
///
/// 無限ループでパケットを受信し、handle_packet() で処理する。
/// パケット処理後は全 waiter を起床させて条件チェックを促す。
/// パケットがないときは enable_and_hlt() で CPU を省電力モードにする
/// （QEMU SLIRP のイベントループ処理にも必要）。
pub fn net_poller_task() {
    net_debug!("net_poller: started");
    loop {
        let mut received = false;

        // 受信キューのフレームをすべて処理する
        while let Some(frame) = recv_frame_nonblocking() {
            handle_packet(&frame);
            received = true;
        }

        // パケットを処理した場合は全 waiter を起床させる
        if received {
            wake_all_net_waiters();
        }

        // QEMU SLIRP のイベントループをキックする
        kick_virtio_net();

        // CPU を一時停止して割り込みを待つ。
        // QEMU TCG モードでは、CPU がビジーループしていると
        // SLIRP のネットワーク I/O が処理されないため、
        // enable_and_hlt() で QEMU に処理時間を与える。
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

// ============================================================
// UDP 処理
// ============================================================

/// UDP パケットを処理する
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

    let udp_checksum = calculate_udp_checksum(&MY_IP, &dst_ip, &udp_header, payload);

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
        src_ip: MY_IP,
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
// DNS クライアント
// ============================================================

/// DNS ポート番号
const DNS_PORT: u16 = 53;
/// DNS レコードタイプ: A (IPv4 アドレス)
const DNS_TYPE_A: u16 = 1;
/// DNS クラス: IN (Internet)
const DNS_CLASS_IN: u16 = 1;

/// DNS クエリを送信して IP アドレスを解決する
pub fn dns_lookup(domain: &str) -> Result<[u8; 4], &'static str> {
    let query_id: u16 = 0x1234;
    let src_port: u16 = 12345;

    let query_packet = build_dns_query(query_id, domain)?;

    // 最大 2 回試行する。初回は ARP 未解決で drop される場合があるためリトライする
    for attempt in 0..2 {
        // レスポンスバッファをクリア
        with_net_state(|state| {
            state.udp_response = None;
        });

        net_debug!("dns: sending query for '{}' (attempt {})", domain, attempt);
        let send_result = send_udp_packet(DNS_SERVER_IP, DNS_PORT, src_port, &query_packet);
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

// ============================================================
// TCP クライアント
// ============================================================

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
    let mut push_accept: Option<(u32, u16)> = None;

    with_net_state(|state| {
        let idx = find_conn_index_by_tuple(state, ip_header.src_ip, src_port, dst_port);
        if idx.is_none() {
            // リスン中なら SYN を受け付ける
            net_debug!("tcp: no existing conn, listen_ports={:?}, dst_port={}", state.tcp_listen_ports, dst_port);
            if state.tcp_listen_ports.contains(&dst_port) && tcp_header.has_flag(TCP_FLAG_SYN) {
                net_debug!("tcp: accepting SYN on port {}, sending SYN+ACK", dst_port);
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
                state.tcp_connections.push(conn);
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
                            push_accept = Some((conn.id, conn.local_port));
                            net_debug!("tcp: server connection established on port {}", conn.local_port);
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

        if let Some((id, port)) = push_accept {
            net_debug!("tcp: pushing to pending_accept: conn_id={}, port={}, queue_len={}", id, port, state.tcp_pending_accept.len());
            state.tcp_pending_accept.push_back((id, port));
        }
    });

    if let Some((dst_ip, dst_port, src_port, seq_num, ack_num, flags)) = send_packet {
        net_debug!("tcp: sending response to {}.{}.{}.{}:{}, flags={:#04x}", dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3], dst_port, flags);
        let result = send_tcp_packet_internal(dst_ip, dst_port, src_port, seq_num, ack_num, flags, &[]);
        net_debug!("tcp: send result: {:?}", result);
    } else {
        net_debug!("tcp: no response to send (SYN dropped?)");
    }
}

/// TCP パケットを送信する（内部用）
///
/// net_poller タスクから呼ばれる場合があるため、ブロッキングする resolve_mac() は使えない。
/// ARP キャッシュから検索し、見つからなければフォールバックでブロードキャスト MAC を使う。
/// 呼び出し元（tcp_connect 等）で事前に resolve_mac() を呼んでキャッシュを温めておくこと。
fn send_tcp_packet_internal(
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
        src_ip: MY_IP,
        dst_ip,
    };

    let ip_header_bytes = unsafe {
        core::slice::from_raw_parts(&ip_header as *const _ as *const u8, 20)
    };
    let ip_checksum = calculate_checksum(ip_header_bytes);

    let tcp_checksum = calculate_tcp_checksum(&MY_IP, &dst_ip, &tcp_header, payload);

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

    net_debug!("tcp: sending SYN");
    send_tcp_packet_internal(dst_ip, dst_port, local_port, initial_seq, 0, TCP_FLAG_SYN, &[])?;

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
                net_debug!("tcp_accept: found conn_id={} for port {}", id, listen_port);
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

    net_debug!("tcp: sending {} bytes", data.len());
    send_tcp_packet_internal(dst_ip, dst_port, local_port, seq_num, ack_num, TCP_FLAG_ACK | TCP_FLAG_PSH, data)
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

    net_debug!("tcp: sending FIN");
    send_tcp_packet_internal(dst_ip, dst_port, local_port, seq_num, ack_num, TCP_FLAG_FIN | TCP_FLAG_ACK, &[])?;

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

    with_net_state(|state| {
        let _ = remove_conn_by_id(state, conn_id);
    });

    net_debug!("tcp: connection closed");
    Ok(())
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

        let id = alloc_conn_id(state);

        state.udp_sockets.push(UdpSocketEntry {
            id,
            local_port,
            recv_queue: VecDeque::new(),
        });

        net_debug!("udp: bind socket id={} port={}", id, local_port);
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

    net_debug!(
        "udp: send_to socket id={} -> {}.{}.{}.{}:{} len={}",
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
        net_debug!("udp: close socket id={}", socket_id);
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

// ============================================================
// IPv6 / ICMPv6 / NDP
// ============================================================

/// ICMPv6 Echo Request
const ICMPV6_ECHO_REQUEST: u8 = 128;
/// ICMPv6 Echo Reply
const ICMPV6_ECHO_REPLY: u8 = 129;
/// ICMPv6 Router Advertisement (NDP)
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
fn handle_ipv6(_eth_header: &EthernetHeader, payload: &[u8]) {
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
            net_debug!("ipv6: unknown next_header {}", ipv6_header.next_header);
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
            net_debug!("icmpv6: Echo Request received");
            send_icmpv6_echo_reply(ipv6_header, payload);
        }
        ICMPV6_ECHO_REPLY => {
            net_debug!("icmpv6: Echo Reply received");
            if payload.len() >= 8 {
                let id = u16::from_be_bytes([payload[4], payload[5]]);
                let seq = u16::from_be_bytes([payload[6], payload[7]]);
                with_net_state(|state| {
                    state.icmpv6_echo_reply = Some((id, seq, ipv6_header.src_ip));
                });
            }
        }
        ICMPV6_ROUTER_ADVERTISEMENT => {
            net_debug!("icmpv6: Router Advertisement received (ignored)");
        }
        ICMPV6_NEIGHBOR_SOLICITATION => {
            net_debug!("icmpv6: Neighbor Solicitation received");
            handle_ndp_neighbor_solicitation(ipv6_header, payload);
        }
        ICMPV6_NEIGHBOR_ADVERTISEMENT => {
            net_debug!("icmpv6: Neighbor Advertisement received (ignored)");
        }
        _ => {
            net_debug!("icmpv6: unknown type {}", icmpv6.icmpv6_type);
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
        net_debug!("ndp: NS target is not MY_IPV6, ignoring");
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
        net_debug!("ipv6: failed to send packet");
    } else {
        net_debug!("ipv6: sent packet, next_header={}, len={}", next_header, payload.len());
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

    match wait_net_condition(timeout_ms, check) {
        Some(reply) => Ok(reply),
        None => Err("timeout"),
    }
}
