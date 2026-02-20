// types.rs — パケットヘッダー構造体と TCP 型定義
//
// Ethernet / ARP / IPv4 / ICMP / UDP / TCP のヘッダー構造体、
// TCP コネクション状態、再送パケット管理をまとめる。

use alloc::vec::Vec;

use super::NetState;

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
pub(super) const TCP_FLAG_FIN: u8 = 0x01;
pub(super) const TCP_FLAG_SYN: u8 = 0x02;
pub(super) const TCP_FLAG_RST: u8 = 0x04;
pub(super) const TCP_FLAG_PSH: u8 = 0x08;
pub(super) const TCP_FLAG_ACK: u8 = 0x10;
#[allow(dead_code)]
pub(super) const TCP_FLAG_URG: u8 = 0x20;

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
    /// 最終待機（遅延パケットの誤認を防ぐため 2MSL 待機する）
    TimeWait,
}

/// TCP 再送の初期 RTO（Retransmission Timeout）。
/// PIT は約 18.2 Hz なので、1 秒 ≈ 18 ticks。
pub(super) const TCP_INITIAL_RTO_TICKS: u64 = 18;

/// TCP 再送の最大回数。
/// RTO は指数バックオフで増加: 1s, 2s, 4s, 8s, 16s（合計約 31 秒）。
pub(super) const TCP_MAX_RETRANSMIT: u8 = 5;

/// 再送待ちパケット
///
/// SYN / SYN-ACK / データ / FIN を送信した後、ACK が返ってこなかった場合に
/// 再送するための情報を保持する。ACK を受信したら `unacked_packet` を None にクリアする。
pub struct UnackedPacket {
    /// 送信時のシーケンス番号
    pub seq_num: u32,
    /// 送信時の ACK 番号
    pub ack_num: u32,
    /// TCP フラグ（SYN, FIN, ACK|PSH 等）
    pub flags: u8,
    /// ペイロード（データ送信の場合。SYN/FIN は空）
    pub payload: Vec<u8>,
    /// 再送デッドライン（PIT tick）。この時刻を過ぎたら再送する。
    pub retransmit_deadline: u64,
    /// 再送回数。指数バックオフの計算に使う。
    pub retransmit_count: u8,
}

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
    /// TIME_WAIT 状態の期限（PIT tick）。None なら TIME_WAIT ではない。
    /// TIME_WAIT 期限が来たら net_poller が接続を削除する。
    pub time_wait_deadline: Option<u64>,
    /// 再送バッファ: 未 ACK のパケット。ACK を受信したらクリアする。
    /// 1 パケットのみ保持（Stop-and-Wait 方式）。
    pub unacked_packet: Option<UnackedPacket>,
}

impl TcpConnection {
    pub fn new(id: u32, local_port: u16, remote_ip: [u8; 4], remote_port: u16) -> Self {
        // ISN（Initial Sequence Number）をランダム化する。
        // 固定値だと TCP シーケンス番号予測攻撃に脆弱なため、
        // RDRAND でランダムな初期値を生成する。
        let initial_seq = super::kernel_rdrand64() as u32;
        Self {
            id,
            state: TcpState::Closed,
            local_port,
            remote_ip,
            remote_port,
            seq_num: initial_seq,
            ack_num: 0,
            recv_buffer: Vec::new(),
            time_wait_deadline: None,
            unacked_packet: None,
        }
    }
}

// ============================================================
// TCP コネクション管理ヘルパー関数
// ============================================================

pub(super) fn alloc_conn_id(state: &mut NetState) -> u32 {
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

pub(super) fn alloc_local_port(state: &mut NetState) -> u16 {
    let port = state.tcp_next_port;
    let next = state.tcp_next_port.wrapping_add(1);
    state.tcp_next_port = if next < 49152 { 49152 } else { next };
    port
}

pub(super) fn find_conn_index_by_id(state: &NetState, id: u32) -> Option<usize> {
    state.tcp_connections.iter().position(|c| c.id == id)
}

pub(super) fn find_conn_index_by_tuple(
    state: &NetState,
    src_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
) -> Option<usize> {
    state.tcp_connections.iter().position(|c| {
        c.remote_ip == src_ip && c.remote_port == src_port && c.local_port == dst_port
    })
}

pub(super) fn remove_conn_by_id(state: &mut NetState, id: u32) -> Option<TcpConnection> {
    if let Some(idx) = find_conn_index_by_id(state, id) {
        Some(state.tcp_connections.remove(idx))
    } else {
        None
    }
}
