// net.rs — ネットワーク抽象化ライブラリ（user space）
//
// shell.rs / httpd.rs / telnetd.rs に散らばっていた netd IPC クライアントコードを
// 共通ライブラリとして集約する。std::net 風の TcpStream / TcpListener / DNS API を提供する。
//
// ## 設計方針
//
// - TcpStream は Drop で自動クローズ（RAII パターン）
// - TcpListener は bind + accept のシンプルな API
// - 低レベル API (raw_*) も公開し、telnetd のセッション管理のような
//   conn_id を直接操作する用途に対応する
// - netd のタスク ID は自動検索 + キャッシュし、IPC 失敗時には再解決 + リトライする

#![allow(dead_code)]

use super::json;
use super::syscall;

// =================================================================
// netd IPC プロトコルの定数
// =================================================================

/// DNS 名前解決
const OPCODE_DNS_LOOKUP: u32 = 1;
/// TCP 接続の確立
const OPCODE_TCP_CONNECT: u32 = 2;
/// TCP データ送信
const OPCODE_TCP_SEND: u32 = 3;
/// TCP データ受信
const OPCODE_TCP_RECV: u32 = 4;
/// TCP 接続のクローズ
const OPCODE_TCP_CLOSE: u32 = 5;
/// TCP リッスン開始
const OPCODE_TCP_LISTEN: u32 = 6;
/// TCP 接続の受け入れ
const OPCODE_TCP_ACCEPT: u32 = 7;
/// UDP バインド
const OPCODE_UDP_BIND: u32 = 8;
/// UDP データ送信
const OPCODE_UDP_SEND_TO: u32 = 9;
/// UDP データ受信
const OPCODE_UDP_RECV_FROM: u32 = 10;
/// UDP ソケットのクローズ
const OPCODE_UDP_CLOSE: u32 = 11;

/// IPC リクエストヘッダサイズ: opcode(4) + payload_len(4) = 8 バイト
const IPC_REQ_HEADER: usize = 8;
/// IPC レスポンスヘッダサイズ: opcode(4) + status(4) + data_len(4) = 12 バイト
const IPC_RESP_HEADER: usize = 12;
/// IPC バッファサイズ
const IPC_BUF_SIZE: usize = 2048;
/// タスク一覧取得用バッファサイズ
const TASK_LIST_BUF_SIZE: usize = 4096;

// =================================================================
// グローバル状態: netd のタスク ID をキャッシュする
// =================================================================

/// netd のタスク ID（0 = 未解決）
static mut NETD_TASK_ID: u64 = 0;

// =================================================================
// エラー型
// =================================================================

/// ネットワーク操作のエラー
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// DNS 名前解決に失敗
    DnsLookupFailed,
    /// TCP 接続に失敗
    ConnectionFailed,
    /// データ送信に失敗
    SendFailed,
    /// データ受信に失敗
    RecvFailed,
    /// タイムアウト
    Timeout,
    /// netd が見つからない
    NetdNotFound,
    /// listen に失敗
    ListenFailed,
    /// accept に失敗
    AcceptFailed,
    /// IPC 通信エラー
    IpcError,
    /// UDP バインドに失敗
    UdpBindFailed,
    /// UDP 送信に失敗
    UdpSendFailed,
    /// UDP 受信に失敗
    UdpRecvFailed,
}

// =================================================================
// アドレス型（std::net 互換風）
// =================================================================

/// IPv4 アドレス（std::net::Ipv4Addr 互換風）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Addr {
    /// IPv4 アドレスのオクテット（例: [192, 168, 1, 1]）
    pub octets: [u8; 4],
}

impl Ipv4Addr {
    /// オクテットから Ipv4Addr を生成する
    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self { octets: [a, b, c, d] }
    }

    /// オクテット配列への参照を返す
    pub fn octets(&self) -> &[u8; 4] {
        &self.octets
    }
}

/// ソケットアドレス（IP + ポート）（std::net::SocketAddr 互換風）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketAddr {
    /// IP アドレス
    pub ip: Ipv4Addr,
    /// ポート番号
    pub port: u16,
}

impl SocketAddr {
    /// SocketAddr を生成する
    pub const fn new(ip: Ipv4Addr, port: u16) -> Self {
        Self { ip, port }
    }
}

// =================================================================
// TcpStream — TCP 接続の抽象化
// =================================================================

/// TCP ストリーム（std::net::TcpStream 互換風）
///
/// Drop で自動的にコネクションをクローズする（RAII パターン）。
/// これにより、明示的な close 呼び出しを忘れてもリソースリークしない。
pub struct TcpStream {
    /// netd が管理するコネクション ID
    conn_id: u32,
    /// 受信タイムアウト（ミリ秒）。0 = デフォルト 5000ms
    recv_timeout_ms: u64,
}

impl TcpStream {
    /// 指定アドレスに TCP 接続する
    ///
    /// DNS 解決は呼び出し元が dns_lookup() で行い、SocketAddr を渡す。
    ///
    /// # 例
    /// ```
    /// let ip = net::dns_lookup("example.com")?;
    /// let addr = net::SocketAddr::new(ip, 80);
    /// let stream = net::TcpStream::connect(addr)?;
    /// ```
    pub fn connect(addr: SocketAddr) -> Result<Self, NetError> {
        let mut payload = [0u8; 6];
        payload[0..4].copy_from_slice(&addr.ip.octets);
        payload[4..6].copy_from_slice(&addr.port.to_le_bytes());
        let mut resp = [0u8; IPC_BUF_SIZE];
        let (status, len) = netd_request(OPCODE_TCP_CONNECT, &payload, &mut resp)
            .map_err(|_| NetError::ConnectionFailed)?;
        if status < 0 || len != 4 {
            return Err(NetError::ConnectionFailed);
        }
        let conn_id = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
        Ok(Self {
            conn_id,
            recv_timeout_ms: 5000,
        })
    }

    /// 低レベル conn_id から TcpStream を構築する（accept 用）
    fn from_conn_id(conn_id: u32) -> Self {
        Self {
            conn_id,
            recv_timeout_ms: 5000,
        }
    }

    /// データを送信する
    ///
    /// data が IPC バッファに収まる範囲で 1 回分を送信する。
    /// 大きなデータの場合は write_all() を使う。
    pub fn write(&self, data: &[u8]) -> Result<(), NetError> {
        raw_send(self.conn_id, data)
    }

    /// データを分割して全て送信する
    ///
    /// IPC バッファの制限を超えるデータも 1024 バイトずつ分割して送信する。
    /// HTTP レスポンスの送信など、大きなデータを送る場合に使用する。
    pub fn write_all(&self, data: &[u8]) -> Result<(), NetError> {
        let mut offset = 0usize;
        while offset < data.len() {
            let end = core::cmp::min(offset + 1024, data.len());
            raw_send(self.conn_id, &data[offset..end])?;
            offset = end;
        }
        Ok(())
    }

    /// データを受信する
    ///
    /// 設定された recv_timeout_ms でタイムアウト付き受信を行う。
    /// 戻り値は受信バイト数。0 はデータなし（タイムアウト）。
    /// エラーは接続切断など。
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, NetError> {
        raw_recv(self.conn_id, buf, self.recv_timeout_ms)
    }

    /// 受信タイムアウトを設定する（ミリ秒）
    pub fn set_recv_timeout(&mut self, ms: u64) {
        self.recv_timeout_ms = ms;
    }

    /// 低レベルのコネクション ID を取得する
    ///
    /// telnetd のようにセッション管理で conn_id を直接操作する場合に使用する。
    pub fn conn_id(&self) -> u32 {
        self.conn_id
    }

    /// TcpStream を消費せずにコネクション ID を取り出す（Drop を無効化）
    ///
    /// telnetd のように conn_id のライフサイクルを自分で管理する場合に使う。
    /// into_raw_conn_id() を呼んだ後は、呼び出し側が raw_close() で
    /// 明示的にクローズする責任を持つ。
    pub fn into_raw_conn_id(self) -> u32 {
        let id = self.conn_id;
        // Drop を呼ばせないために forget する
        core::mem::forget(self);
        id
    }
}

impl Drop for TcpStream {
    /// コネクションを自動クローズする
    fn drop(&mut self) {
        let _ = raw_close(self.conn_id);
    }
}

// =================================================================
// TcpListener — TCP リスナーの抽象化
// =================================================================

/// TCP リスナー（std::net::TcpListener 互換風）
///
/// 指定ポートで接続を待ち受け、accept で TcpStream を返す。
pub struct TcpListener {
    /// リッスンしているポート番号
    port: u16,
}

impl TcpListener {
    /// 指定ポートでリッスンを開始する
    ///
    /// # 例
    /// ```
    /// let listener = net::TcpListener::bind(8080)?;
    /// loop {
    ///     let stream = listener.accept()?;
    ///     handle_connection(stream);
    /// }
    /// ```
    pub fn bind(port: u16) -> Result<Self, NetError> {
        raw_listen(port)?;
        Ok(Self { port })
    }

    /// 接続を受け入れる（ブロッキング）
    ///
    /// クライアントが接続してくるまでブロックする。
    pub fn accept(&self) -> Result<TcpStream, NetError> {
        let conn_id = raw_accept(0)?;
        Ok(TcpStream::from_conn_id(conn_id))
    }

    /// タイムアウト付きで接続を受け入れる
    ///
    /// timeout_ms ミリ秒待っても接続がなければ Err(NetError::Timeout) を返す。
    /// httpd のメインループなど、定期的に他の処理も行いたい場合に使う。
    pub fn accept_timeout(&self, timeout_ms: u64) -> Result<TcpStream, NetError> {
        let conn_id = raw_accept(timeout_ms)?;
        Ok(TcpStream::from_conn_id(conn_id))
    }

    /// リッスンしているポート番号を返す
    pub fn port(&self) -> u16 {
        self.port
    }
}

// =================================================================
// netd 初期化 + DNS
// =================================================================

/// netd のタスク ID を自動検索して初期化する
///
/// タスク一覧から "NETD.ELF" を探し、見つかれば内部にキャッシュする。
/// 見つからなければ NetError::NetdNotFound を返す。
pub fn init_netd() -> Result<(), NetError> {
    let id = resolve_task_id_by_name("NETD.ELF").ok_or(NetError::NetdNotFound)?;
    unsafe {
        NETD_TASK_ID = id;
    }
    Ok(())
}

/// netd のタスク ID を明示的にセットする
///
/// init が先に netd を起動している場合など、タスク ID が既知のときに使う。
pub fn set_netd_id(id: u64) {
    unsafe {
        NETD_TASK_ID = id;
    }
}

/// netd のタスク ID を取得する（0 = 未解決）
pub fn get_netd_id() -> u64 {
    unsafe { NETD_TASK_ID }
}

/// DNS 名前解決を行う
///
/// ドメイン名から IPv4 アドレスを解決する。
/// netd が初期化されていなければ自動的に init_netd() を試みる。
///
/// # 例
/// ```
/// let ip = net::dns_lookup("example.com")?;
/// ```
pub fn dns_lookup(domain: &str) -> Result<Ipv4Addr, NetError> {
    let payload = domain.as_bytes();
    let mut resp = [0u8; IPC_BUF_SIZE];
    let (status, len) = netd_request(OPCODE_DNS_LOOKUP, payload, &mut resp)
        .map_err(|_| NetError::DnsLookupFailed)?;
    if status < 0 || len != 4 {
        return Err(NetError::DnsLookupFailed);
    }
    Ok(Ipv4Addr::new(resp[0], resp[1], resp[2], resp[3]))
}

// =================================================================
// 低レベル API（telnetd のセッション管理等向け）
// =================================================================

/// 低レベル: TCP データ送信（conn_id 指定）
///
/// TcpStream を経由せず、conn_id を直接指定して送信する。
/// telnetd のように複数セッションを管理する場合に使う。
pub fn raw_send(conn_id: u32, data: &[u8]) -> Result<(), NetError> {
    let mut payload = [0u8; IPC_BUF_SIZE];
    if 4 + data.len() > payload.len() {
        return Err(NetError::SendFailed);
    }
    payload[0..4].copy_from_slice(&conn_id.to_le_bytes());
    payload[4..4 + data.len()].copy_from_slice(data);
    let mut resp = [0u8; IPC_BUF_SIZE];
    let (status, _) = netd_request(OPCODE_TCP_SEND, &payload[..4 + data.len()], &mut resp)
        .map_err(|_| NetError::SendFailed)?;
    if status < 0 {
        Err(NetError::SendFailed)
    } else {
        Ok(())
    }
}

/// 低レベル: TCP データ受信（conn_id 指定）
///
/// 受信バイト数を返す。0 はタイムアウト（データなし）。
pub fn raw_recv(conn_id: u32, buf: &mut [u8], timeout_ms: u64) -> Result<usize, NetError> {
    let mut payload = [0u8; 16];
    let max_len = buf.len() as u32;
    payload[0..4].copy_from_slice(&conn_id.to_le_bytes());
    payload[4..8].copy_from_slice(&max_len.to_le_bytes());
    payload[8..16].copy_from_slice(&timeout_ms.to_le_bytes());

    let mut resp = [0u8; IPC_BUF_SIZE];
    let (status, len) = netd_request(OPCODE_TCP_RECV, &payload, &mut resp)
        .map_err(|_| NetError::RecvFailed)?;
    // status == -42 はタイムアウト（データなし）
    if status == -42 {
        return Ok(0);
    }
    if status < 0 {
        return Err(NetError::RecvFailed);
    }
    let copy_len = core::cmp::min(buf.len(), len);
    buf[..copy_len].copy_from_slice(&resp[..copy_len]);
    Ok(copy_len)
}

/// 低レベル: TCP 接続をクローズ（conn_id 指定）
pub fn raw_close(conn_id: u32) -> Result<(), NetError> {
    let mut resp = [0u8; IPC_BUF_SIZE];
    let payload = conn_id.to_le_bytes();
    let (status, _) = netd_request(OPCODE_TCP_CLOSE, &payload, &mut resp)
        .map_err(|_| NetError::IpcError)?;
    if status < 0 {
        Err(NetError::IpcError)
    } else {
        Ok(())
    }
}

/// 低レベル: TCP リッスン開始
pub fn raw_listen(port: u16) -> Result<(), NetError> {
    let payload = port.to_le_bytes();
    let (status, _) = netd_request(OPCODE_TCP_LISTEN, &payload, &mut [0u8; 32])
        .map_err(|_| NetError::ListenFailed)?;
    if status < 0 { Err(NetError::ListenFailed) } else { Ok(()) }
}

/// 低レベル: TCP 接続の受け入れ
///
/// timeout_ms=0 でブロッキング待ち。成功時は conn_id を返す。
pub fn raw_accept(timeout_ms: u64) -> Result<u32, NetError> {
    let payload = timeout_ms.to_le_bytes();
    let mut resp = [0u8; 32];
    let (status, len) = netd_request(OPCODE_TCP_ACCEPT, &payload, &mut resp)
        .map_err(|_| NetError::AcceptFailed)?;
    if status < 0 || len != 4 {
        Err(NetError::AcceptFailed)
    } else {
        Ok(u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]))
    }
}

// =================================================================
// UdpSocket — UDP ソケットの抽象化
// =================================================================

/// UDP ソケット（std::net::UdpSocket 互換風）
///
/// Drop で自動的にソケットをクローズする（RAII パターン）。
/// bind() でポートにバインドし、send_to / recv_from でデータを送受信する。
pub struct UdpSocket {
    /// netd が管理するソケット ID
    socket_id: u32,
    /// バインドしているローカルポート
    local_port: u16,
    /// 受信タイムアウト（ミリ秒）。0 = デフォルト 5000ms
    recv_timeout_ms: u64,
}

impl UdpSocket {
    /// 指定ポートにバインドして UDP ソケットを作成する
    ///
    /// port=0 でエフェメラルポートを自動割り当て。
    ///
    /// # 例
    /// ```
    /// let sock = net::UdpSocket::bind(0)?;  // エフェメラルポート
    /// let sock = net::UdpSocket::bind(5353)?;  // 特定ポート
    /// ```
    pub fn bind(port: u16) -> Result<Self, NetError> {
        let payload = port.to_le_bytes();
        let mut resp = [0u8; IPC_BUF_SIZE];
        let (status, len) = netd_request(OPCODE_UDP_BIND, &payload, &mut resp)
            .map_err(|_| NetError::UdpBindFailed)?;
        if status < 0 || len < 6 {
            return Err(NetError::UdpBindFailed);
        }
        let socket_id = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
        let local_port = u16::from_le_bytes([resp[4], resp[5]]);
        Ok(Self {
            socket_id,
            local_port,
            recv_timeout_ms: 5000,
        })
    }

    /// データを指定アドレスに送信する
    ///
    /// # 引数
    /// - `data`: 送信データ
    /// - `addr`: 宛先アドレス（IP + ポート）
    pub fn send_to(&self, data: &[u8], addr: SocketAddr) -> Result<usize, NetError> {
        // payload: [socket_id: u32][dst_ip: 4B][dst_port: u16 LE][data...]
        let mut payload = [0u8; IPC_BUF_SIZE];
        let total = 10 + data.len();
        if total > payload.len() {
            return Err(NetError::UdpSendFailed);
        }
        payload[0..4].copy_from_slice(&self.socket_id.to_le_bytes());
        payload[4..8].copy_from_slice(&addr.ip.octets);
        payload[8..10].copy_from_slice(&addr.port.to_le_bytes());
        payload[10..10 + data.len()].copy_from_slice(data);

        let mut resp = [0u8; 32];
        let (status, _) = netd_request(OPCODE_UDP_SEND_TO, &payload[..total], &mut resp)
            .map_err(|_| NetError::UdpSendFailed)?;
        if status < 0 {
            Err(NetError::UdpSendFailed)
        } else {
            Ok(data.len())
        }
    }

    /// データを受信し、送信元アドレスも返す
    ///
    /// # 引数
    /// - `buf`: 受信バッファ
    ///
    /// # 戻り値
    /// - `Ok((n, addr))`: 受信バイト数と送信元アドレス
    pub fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), NetError> {
        // payload: [socket_id: u32][max_len: u32 LE][timeout_ms: u64 LE]
        let mut payload = [0u8; 16];
        let max_len = buf.len() as u32;
        payload[0..4].copy_from_slice(&self.socket_id.to_le_bytes());
        payload[4..8].copy_from_slice(&max_len.to_le_bytes());
        payload[8..16].copy_from_slice(&self.recv_timeout_ms.to_le_bytes());

        let mut resp = [0u8; IPC_BUF_SIZE];
        let (status, len) = netd_request(OPCODE_UDP_RECV_FROM, &payload, &mut resp)
            .map_err(|_| NetError::UdpRecvFailed)?;

        // status == -42 はタイムアウト
        if status == -42 {
            return Err(NetError::Timeout);
        }
        if status < 0 || len < 6 {
            return Err(NetError::UdpRecvFailed);
        }

        // レスポンス: [src_ip: 4B][src_port: u16 LE][data...]
        let src_ip = Ipv4Addr::new(resp[0], resp[1], resp[2], resp[3]);
        let src_port = u16::from_le_bytes([resp[4], resp[5]]);
        let data_len = len - 6;
        let copy_len = core::cmp::min(data_len, buf.len());
        buf[..copy_len].copy_from_slice(&resp[6..6 + copy_len]);

        let addr = SocketAddr::new(src_ip, src_port);
        Ok((copy_len, addr))
    }

    /// 受信タイムアウトを設定する（ミリ秒）
    pub fn set_recv_timeout(&mut self, ms: u64) {
        self.recv_timeout_ms = ms;
    }

    /// バインドしているローカルポートを返す
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// ソケット ID を返す
    pub fn socket_id(&self) -> u32 {
        self.socket_id
    }
}

impl Drop for UdpSocket {
    /// ソケットを自動クローズする
    fn drop(&mut self) {
        let payload = self.socket_id.to_le_bytes();
        let mut resp = [0u8; 32];
        let _ = netd_request(OPCODE_UDP_CLOSE, &payload, &mut resp);
    }
}

// =================================================================
// 内部実装: netd IPC 通信
// =================================================================

/// netd に IPC リクエストを送信し、レスポンスを受け取る共通関数
///
/// IPC 失敗時は netd の PID 再解決 + 1 回だけリトライを行う。
/// これにより netd が再起動しても自動的にリカバリできる。
fn netd_request(opcode: u32, payload: &[u8], resp_buf: &mut [u8]) -> Result<(i32, usize), ()> {
    let mut req = [0u8; IPC_BUF_SIZE];
    if IPC_REQ_HEADER + payload.len() > req.len() {
        return Err(());
    }
    req[0..4].copy_from_slice(&opcode.to_le_bytes());
    req[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    req[8..8 + payload.len()].copy_from_slice(payload);

    let mut netd_id = unsafe { NETD_TASK_ID };
    if netd_id == 0 {
        // 未解決なら自動検索
        find_netd();
        netd_id = unsafe { NETD_TASK_ID };
        if netd_id == 0 {
            return Err(());
        }
    }

    if syscall::ipc_send(netd_id, &req[..8 + payload.len()]) < 0 {
        // netd の PID が変わった可能性があるので再解決して 1 回だけリトライ
        find_netd();
        netd_id = unsafe { NETD_TASK_ID };
        if netd_id == 0 {
            return Err(());
        }
        if syscall::ipc_send(netd_id, &req[..8 + payload.len()]) < 0 {
            return Err(());
        }
    }

    let mut sender = 0u64;
    let n = syscall::ipc_recv(&mut sender, resp_buf, 5000);
    if n < 0 {
        return Err(());
    }
    let n = n as usize;
    if n < IPC_RESP_HEADER {
        return Err(());
    }

    let resp_opcode = u32::from_le_bytes([resp_buf[0], resp_buf[1], resp_buf[2], resp_buf[3]]);
    if resp_opcode != opcode {
        return Err(());
    }
    let status = i32::from_le_bytes([resp_buf[4], resp_buf[5], resp_buf[6], resp_buf[7]]);
    let len = u32::from_le_bytes([resp_buf[8], resp_buf[9], resp_buf[10], resp_buf[11]]) as usize;
    if IPC_RESP_HEADER + len > n {
        return Err(());
    }
    // レスポンスデータをバッファ先頭に移動して、呼び出し元が使いやすくする
    resp_buf.copy_within(IPC_RESP_HEADER..IPC_RESP_HEADER + len, 0);

    Ok((status, len))
}

/// netd の PID をタスク一覧から検索してキャッシュに保存する
fn find_netd() {
    let netd_id = resolve_task_id_by_name("NETD.ELF").unwrap_or(0);
    unsafe {
        NETD_TASK_ID = netd_id;
    }
}

/// タスク一覧 JSON から指定名のタスク ID を探す
///
/// get_task_list システムコールで取得した JSON を解析し、
/// 指定した名前のタスクの ID を返す。
fn resolve_task_id_by_name(name: &str) -> Option<u64> {
    let mut buf = [0u8; TASK_LIST_BUF_SIZE];
    let result = syscall::get_task_list(&mut buf);
    if result < 0 {
        return None;
    }
    let len = result as usize;
    let s = core::str::from_utf8(&buf[..len]).ok()?;

    let (tasks_start, tasks_end) = json::json_find_array_bounds(s, "tasks")?;
    let mut i = tasks_start;
    let bytes = s.as_bytes();
    while i < tasks_end {
        while i < tasks_end && bytes[i] != b'{' && bytes[i] != b']' {
            i += 1;
        }
        if i >= tasks_end || bytes[i] == b']' {
            break;
        }

        let obj_end = json::find_matching_brace(s, i)?;
        if obj_end > tasks_end {
            break;
        }

        let obj = &s[i + 1..obj_end];
        let id = json::json_find_u64(obj, "id");
        let task_name = json::json_find_str(obj, "name");
        if let (Some(id), Some(task_name)) = (id, task_name) {
            if task_name == name {
                return Some(id);
            }
        }
        i = obj_end + 1;
    }
    None
}
