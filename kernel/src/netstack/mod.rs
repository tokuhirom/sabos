// netstack/mod.rs — カーネル内ネットワークスタック
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
// NET_STATE と NIC ドライバ (VIRTIO_NET / E1000E) は異なる Mutex。
// net_poller_task(): NIC ロック→受信→ロック解放→handle_packet() の順。
// handle_packet() 内の send_arp_reply() 等: NET_STATE から MAC 取得→ロック解放→NIC でフレーム送信。
// MAC アドレスは初期化時に MY_MAC に保持し、NET_STATE のロック不要。

mod types;
mod arp;
mod icmp;
mod tcp;
mod udp;
mod dns;
mod ipv6;
mod dhcp;

// Re-exports for external use
pub use types::{TcpConnection, UnackedPacket, TcpState};
pub use arp::resolve_mac;
pub use tcp::{tcp_connect, tcp_listen, tcp_accept, tcp_send, tcp_recv, tcp_close};
pub use udp::{udp_bind, udp_send_to, udp_recv_from, udp_close, udp_local_port};
pub use dns::dns_lookup;
pub use ipv6::{send_icmpv6_echo_request, wait_icmpv6_echo_reply};
pub use dhcp::dhcp_discover;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use spin::Mutex;

use crate::net_config::get_my_ip;
use crate::serial_println;

/// ループバック IP アドレス (127.0.0.1)
pub const LOOPBACK_IP: [u8; 4] = [127, 0, 0, 1];

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
// カーネル内乱数生成
// ============================================================

/// カーネル内で RDRAND 命令を使って 64 ビット乱数を取得する。
/// TCP ISN や DNS クエリ ID のランダム化に使用する。
/// 失敗時は簡易フォールバック（0 を返す）。
pub(super) fn kernel_rdrand64() -> u64 {
    for _ in 0..10 {
        let value: u64;
        let success: u8;
        unsafe {
            core::arch::asm!(
                "rdrand {val}",
                "setc {ok}",
                val = out(reg) value,
                ok = out(reg_byte) success,
            );
        }
        if success != 0 {
            return value;
        }
    }
    // フォールバック: RDRAND が使えない場合は PIT カウンタなどで代替すべきだが、
    // 現代の x86_64 CPU では RDRAND が使えない状況は稀なので 0 を返す
    0
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
pub(self) struct NetState {
    pub(self) mac: [u8; 6],
    pub(self) tcp_connections: Vec<types::TcpConnection>,
    pub(self) tcp_next_id: u32,
    pub(self) tcp_next_port: u16,
    /// リスン中のポート一覧（複数サービスが同時に listen 可能）
    pub(self) tcp_listen_ports: Vec<u16>,
    /// accept 待ちの接続キュー: (conn_id, local_port)
    pub(self) tcp_pending_accept: VecDeque<(u32, u16)>,
    pub(self) udp_response: Option<(u16, Vec<u8>)>,
    /// UDP ソケット一覧
    pub(self) udp_sockets: Vec<UdpSocketEntry>,
    /// UDP エフェメラルポートの次の候補（49152〜65535）
    pub(self) udp_next_port: u16,
    /// ICMPv6 Echo Reply を受信したときに保存する (id, seq, src_ip)
    pub(self) icmpv6_echo_reply: Option<(u16, u16, [u8; 16])>,
    /// ネットワークイベントを待っているタスク ID のリスト。
    /// net_poller がパケットを処理した後に全 waiter を起床させる。
    /// これにより tcp_accept 等が個別にパケット受信する必要がなくなる。
    pub(self) net_waiters: Vec<u64>,
    /// ARP キャッシュ: IP → MAC のマッピングテーブル
    /// 送信時に宛先 MAC を解決するために使う。
    /// ARP Reply 受信時や ARP Request 受信時（送信元）に学習する。
    pub(self) arp_cache: Vec<ArpEntry>,
}

/// グローバルなネットワーク状態（spin::Mutex で保護）
static NET_STATE: Mutex<Option<NetState>> = Mutex::new(None);

/// NET_STATE のロックを取得し、初期化されていなければ初期化してから返す
pub(self) fn with_net_state<F, R>(f: F) -> R
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
///
/// virtio-net → e1000e の順で NIC を探し、最初に見つかったデバイスの
/// MAC アドレスを使ってネットワークスタックを初期化する。
/// DHCP で IP アドレスを取得し、失敗してもデフォルト値が残るので安全。
pub fn init() {
    // virtio-net から MAC アドレスを取得
    let mac = {
        let drv = crate::virtio_net::VIRTIO_NET.lock();
        drv.as_ref().map(|d| d.mac_address)
    };
    // virtio-net がなければ e1000e から取得
    let mac = mac.or_else(|| {
        let drv = crate::e1000e::E1000E.lock();
        drv.as_ref().map(|d| d.mac_address)
    });

    if let Some(mac) = mac {
        *MY_MAC.lock() = mac;
        with_net_state(|state| {
            state.mac = mac;
        });
        serial_println!("netstack: initialized with MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

        // DHCP で IP アドレスを取得する
        // 失敗してもデフォルト値（10.0.2.15 等）が残るので安全
        match dhcp_discover() {
            Ok(()) => {
                let ip = get_my_ip();
                serial_println!("netstack: DHCP configured IP={}.{}.{}.{}",
                    ip[0], ip[1], ip[2], ip[3]);
            }
            Err(e) => {
                serial_println!("netstack: DHCP failed ({}), using default config", e);
            }
        }
    } else {
        serial_println!("netstack: no network device available, skipping init");
    }
}

/// ネットワークリンクの状態を返す。
///
/// virtio-net を優先し、なければ e1000e を確認する。
/// いずれのデバイスも存在しなければ false を返す。
pub fn is_network_link_up() -> bool {
    // virtio-net を優先チェック
    let drv = crate::virtio_net::VIRTIO_NET.lock();
    if let Some(ref d) = *drv {
        return d.is_link_up();
    }
    drop(drv);
    // e1000e にフォールバック
    let drv = crate::e1000e::E1000E.lock();
    if let Some(ref d) = *drv {
        return d.is_link_up();
    }
    false
}

/// MAC アドレスを取得する（ロック不要版）
pub(self) fn get_my_mac() -> [u8; 6] {
    *MY_MAC.lock()
}

// ============================================================
// ARP キャッシュ操作
// ============================================================

/// ARP キャッシュから IP に対応する MAC アドレスを検索する
pub(self) fn arp_lookup(ip: &[u8; 4]) -> Option<[u8; 6]> {
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
pub(self) fn arp_update(ip: [u8; 4], mac: [u8; 6]) {
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
// フレーム送受信ヘルパー（NIC 抽象化）
// ============================================================
//
// virtio-net と e1000e の両方に対応する。
// virtio-net が存在すれば優先的に使い、なければ e1000e にフォールバックする。
// これにより QEMU では virtio-net（高速）、実機では e1000e が自動選択される。

/// Ethernet フレームを送信する（NIC 抽象化）
///
/// virtio-net を優先し、なければ e1000e にフォールバックする。
/// どちらも存在しなければエラーを返す。
pub(self) fn send_frame(data: &[u8]) -> Result<(), &'static str> {
    // virtio-net を優先（QEMU デフォルト）
    {
        let mut drv = crate::virtio_net::VIRTIO_NET.lock();
        if let Some(ref mut d) = *drv {
            return d.send_packet(data);
        }
    }
    // e1000e にフォールバック
    {
        let mut drv = crate::e1000e::E1000E.lock();
        if let Some(ref mut d) = *drv {
            return d.send_packet(data);
        }
    }
    Err("no network device available")
}

/// Ethernet フレームを受信する（ノンブロッキング、NIC 抽象化）
///
/// virtio-net を優先し、なければ e1000e にフォールバックする。
/// 受信できなければ None を返す。
fn recv_frame_nonblocking() -> Option<Vec<u8>> {
    // virtio-net を優先
    {
        let mut drv = crate::virtio_net::VIRTIO_NET.lock();
        if let Some(ref mut d) = *drv {
            return d.receive_packet();
        }
    }
    // e1000e にフォールバック
    {
        let mut drv = crate::e1000e::E1000E.lock();
        if let Some(ref mut d) = *drv {
            return d.receive_packet();
        }
    }
    None
}

/// NIC デバイスのイベントフラグをクリアする
///
/// virtio-net の場合は ISR ステータスを読み取って QEMU のイベントループをキックする。
/// e1000e の場合は ICR を読み取って保留中の割り込みをクリアする。
fn kick_net_device() {
    // virtio-net
    {
        let mut drv = crate::virtio_net::VIRTIO_NET.lock();
        if let Some(d) = drv.as_mut() {
            d.read_isr_status();
            return;
        }
    }
    // e1000e
    {
        let mut drv = crate::e1000e::E1000E.lock();
        if let Some(d) = drv.as_mut() {
            d.clear_interrupts();
        }
    }
}

// ============================================================
// プロトコル/タイプ定数
// ============================================================

/// IPv4
pub(self) const ETHERTYPE_IPV4: u16 = 0x0800;
/// ARP
pub(self) const ETHERTYPE_ARP: u16 = 0x0806;
/// IPv6
pub(self) const ETHERTYPE_IPV6: u16 = 0x86DD;

/// ARP リクエスト
pub(self) const ARP_OP_REQUEST: u16 = 1;
/// ARP リプライ
pub(self) const ARP_OP_REPLY: u16 = 2;
/// Ethernet ハードウェアタイプ
pub(self) const ARP_HTYPE_ETHERNET: u16 = 1;

/// ICMP
pub(self) const IP_PROTO_ICMP: u8 = 1;
/// TCP
pub(self) const IP_PROTO_TCP: u8 = 6;
/// UDP
pub(self) const IP_PROTO_UDP: u8 = 17;
/// ICMPv6
pub(self) const IP_PROTO_ICMPV6: u8 = 58;

/// Echo Reply
pub(self) const ICMP_ECHO_REPLY: u8 = 0;
/// Echo Request
pub(self) const ICMP_ECHO_REQUEST: u8 = 8;

// ============================================================
// パケット処理
// ============================================================

/// 受信パケットを処理する
pub fn handle_packet(data: &[u8]) {
    if data.len() < 14 {
        return;
    }

    let eth_header = match types::EthernetHeader::from_bytes(data) {
        Some(h) => h,
        None => return,
    };

    let payload = &data[14..];
    let ethertype = eth_header.ethertype_u16();

    match ethertype {
        ETHERTYPE_ARP => {
            arp::handle_arp(eth_header, payload);
        }
        ETHERTYPE_IPV4 => {
            icmp::handle_ipv4(eth_header, payload);
        }
        ETHERTYPE_IPV6 => {
            ipv6::handle_ipv6(eth_header, payload);
        }
        _ => {
            net_debug!("net: unknown ethertype {:#06x}", ethertype);
        }
    }
}

/// 指定された IP がローカル（自分宛）かどうかを判定する
pub(self) fn is_local_ip(ip: &[u8; 4]) -> bool {
    *ip == get_my_ip() || *ip == LOOPBACK_IP || *ip == [255, 255, 255, 255]
}

// ============================================================
// チェックサム計算
// ============================================================

/// インターネットチェックサムを計算する（RFC 1071）
pub(self) fn calculate_checksum(data: &[u8]) -> u16 {
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
pub(self) fn calculate_udp_checksum(
    src_ip: &[u8; 4],
    dst_ip: &[u8; 4],
    udp_header: &types::UdpHeader,
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
pub(self) fn wait_net_condition<T, F>(timeout_ms: u64, check_fn: F) -> Option<T>
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

        // TIME_WAIT 接続の期限切れチェック
        // タイマー期限が来た接続を削除して、ポートを再利用可能にする。
        with_net_state(|state| {
            let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
            state.tcp_connections.retain(|conn| {
                if conn.state == TcpState::TimeWait {
                    if let Some(deadline) = conn.time_wait_deadline {
                        if now >= deadline {
                            net_debug!("tcp: TIME_WAIT expired for port {}", conn.local_port);
                            return false; // 削除
                        }
                    }
                }
                true // 保持
            });
        });

        // TCP 再送タイマーチェック
        // デッドラインを超えた未 ACK パケットを再送する。
        // Mutex デッドロック防止のため、再送情報を収集してから Mutex 外で送信する。
        let retransmit_list: Vec<(u32, [u8; 4], u16, u16, u32, u32, u8, Vec<u8>)> = with_net_state(|state| {
            let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
            let mut list = Vec::new();
            let mut closed_ids = Vec::new();

            for conn in state.tcp_connections.iter_mut() {
                if let Some(ref mut pkt) = conn.unacked_packet {
                    if now >= pkt.retransmit_deadline {
                        if pkt.retransmit_count >= types::TCP_MAX_RETRANSMIT {
                            // 最大再送回数を超えた → 接続を諦める
                            net_debug!(
                                "tcp: retransmit limit exceeded for conn {} (port {}), closing",
                                conn.id, conn.local_port
                            );
                            closed_ids.push(conn.id);
                        } else {
                            // 指数バックオフで次のデッドラインを計算
                            pkt.retransmit_count += 1;
                            let backoff = types::TCP_INITIAL_RTO_TICKS << pkt.retransmit_count as u64;
                            pkt.retransmit_deadline = now + backoff;
                            net_debug!(
                                "tcp: retransmitting conn {} (port {}), attempt {}, next RTO={}",
                                conn.id, conn.local_port, pkt.retransmit_count, backoff
                            );
                            list.push((
                                conn.id,
                                conn.remote_ip,
                                conn.remote_port,
                                conn.local_port,
                                pkt.seq_num,
                                pkt.ack_num,
                                pkt.flags,
                                pkt.payload.clone(),
                            ));
                        }
                    }
                }
            }

            // 最大再送超過の接続を Closed に遷移
            for id in closed_ids {
                if let Some(idx) = types::find_conn_index_by_id(state, id) {
                    state.tcp_connections[idx].state = TcpState::Closed;
                    state.tcp_connections[idx].unacked_packet = None;
                }
            }

            list
        });

        // Mutex 外で再送パケットを送信する
        for (_conn_id, dst_ip, dst_port, src_port, seq_num, ack_num, flags, payload) in &retransmit_list {
            let _ = tcp::send_tcp_packet_internal(*dst_ip, *dst_port, *src_port, *seq_num, *ack_num, *flags, payload);
        }

        // 再送パケットを送信した場合は waiter を起床させる（状態変更を通知）
        if !retransmit_list.is_empty() {
            wake_all_net_waiters();
        }

        // NIC デバイスのイベントフラグをクリアする
        // virtio-net: QEMU SLIRP のイベントループをキック
        // e1000e: ICR を読み取って割り込み原因をクリア
        kick_net_device();

        // CPU を一時停止して割り込みを待つ。
        // QEMU TCG モードでは、CPU がビジーループしていると
        // SLIRP のネットワーク I/O が処理されないため、
        // enable_and_hlt() で QEMU に処理時間を与える。
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}
