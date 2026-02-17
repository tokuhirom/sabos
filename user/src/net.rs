// net.rs — ネットワーク抽象化ライブラリ（user space）
//
// std::net 風の TcpStream / TcpListener / UdpSocket / DNS API を提供する。
// カーネル内ネットワークスタックにシステムコール経由で直接アクセスする。
//
// ## 設計方針
//
// - TcpStream は Drop で自動クローズ（RAII パターン）
// - TcpListener は bind + accept のシンプルな API
// - 低レベル API (raw_*) も公開し、telnetd のセッション管理のような
//   conn_id を直接操作する用途に対応する
// - netd は不要。カーネル内ネットワークスタックにシステムコールで直接アクセスする

#![allow(dead_code)]

use super::syscall;

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
    /// listen に失敗
    ListenFailed,
    /// accept に失敗
    AcceptFailed,
    /// UDP バインドに失敗
    UdpBindFailed,
    /// UDP 送信に失敗
    UdpSendFailed,
    /// UDP 受信に失敗
    UdpRecvFailed,
    /// IPv6 ping に失敗
    Ping6Failed,
    /// IPv6 ping タイムアウト
    Ping6Timeout,
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

/// IPv6 アドレス（std::net::Ipv6Addr 互換風）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv6Addr {
    /// IPv6 アドレスのオクテット（16 バイト）
    pub octets: [u8; 16],
}

impl Ipv6Addr {
    /// オクテット配列から Ipv6Addr を生成する
    pub const fn from_octets(octets: [u8; 16]) -> Self {
        Self { octets }
    }

    /// オクテット配列への参照を返す
    pub fn octets(&self) -> &[u8; 16] {
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
    /// カーネルが管理するコネクション ID
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
        let result = syscall::net_tcp_connect(&addr.ip.octets, addr.port);
        if result < 0 {
            return Err(NetError::ConnectionFailed);
        }
        Ok(Self {
            conn_id: result as u32,
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
    /// data が送信バッファに収まる範囲で 1 回分を送信する。
    /// 大きなデータの場合は write_all() を使う。
    pub fn write(&self, data: &[u8]) -> Result<(), NetError> {
        raw_send(self.conn_id, data)
    }

    /// データを分割して全て送信する
    ///
    /// バッファの制限を超えるデータも 1024 バイトずつ分割して送信する。
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
    /// 自身の listen ポートへの接続のみ accept する。
    pub fn accept(&self) -> Result<TcpStream, NetError> {
        let conn_id = raw_accept(0, self.port)?;
        Ok(TcpStream::from_conn_id(conn_id))
    }

    /// タイムアウト付きで接続を受け入れる
    ///
    /// timeout_ms ミリ秒待っても接続がなければ Err(NetError::AcceptFailed) を返す。
    /// httpd のメインループなど、定期的に他の処理も行いたい場合に使う。
    /// 自身の listen ポートへの接続のみ accept する。
    pub fn accept_timeout(&self, timeout_ms: u64) -> Result<TcpStream, NetError> {
        let conn_id = raw_accept(timeout_ms, self.port)?;
        Ok(TcpStream::from_conn_id(conn_id))
    }

    /// リッスンしているポート番号を返す
    pub fn port(&self) -> u16 {
        self.port
    }
}

// =================================================================
// DNS + IPv6 ping
// =================================================================

/// DNS 名前解決を行う
///
/// ドメイン名から IPv4 アドレスを解決する。
///
/// # 例
/// ```
/// let ip = net::dns_lookup("example.com")?;
/// ```
pub fn dns_lookup(domain: &str) -> Result<Ipv4Addr, NetError> {
    let mut result_ip = [0u8; 4];
    let ret = syscall::net_dns_lookup(domain, &mut result_ip);
    if ret < 0 {
        return Err(NetError::DnsLookupFailed);
    }
    Ok(Ipv4Addr::new(result_ip[0], result_ip[1], result_ip[2], result_ip[3]))
}

/// IPv6 ping (ICMPv6 Echo) を実行する
///
/// 指定した IPv6 アドレスに ICMPv6 Echo Request を送信し、
/// Echo Reply が返ってくるまで待つ。
///
/// # 引数
/// - `addr`: 宛先 IPv6 アドレス
/// - `timeout_ms`: タイムアウト（ミリ秒）
///
/// # 戻り値
/// - `Ok(src_ip)`: 応答元の IPv6 アドレス（16 バイト）
/// - `Err(Ping6Timeout)`: タイムアウト
/// - `Err(Ping6Failed)`: その他のエラー
pub fn ping6(addr: &Ipv6Addr, timeout_ms: u32) -> Result<[u8; 16], NetError> {
    let mut src_ip = [0u8; 16];
    let ret = syscall::net_ping6(&addr.octets, timeout_ms, &mut src_ip);
    if ret == -42 {
        return Err(NetError::Ping6Timeout);
    }
    if ret < 0 {
        return Err(NetError::Ping6Failed);
    }
    Ok(src_ip)
}

// =================================================================
// 低レベル API（telnetd のセッション管理等向け）
// =================================================================

/// 低レベル: TCP データ送信（conn_id 指定）
///
/// TcpStream を経由せず、conn_id を直接指定して送信する。
/// telnetd のように複数セッションを管理する場合に使う。
pub fn raw_send(conn_id: u32, data: &[u8]) -> Result<(), NetError> {
    let ret = syscall::net_tcp_send(conn_id, data);
    if ret < 0 {
        Err(NetError::SendFailed)
    } else {
        Ok(())
    }
}

/// 低レベル: TCP データ受信（conn_id 指定）
///
/// 受信バイト数を返す。0 はタイムアウト（データなし）。
pub fn raw_recv(conn_id: u32, buf: &mut [u8], timeout_ms: u64) -> Result<usize, NetError> {
    let ret = syscall::net_tcp_recv(conn_id, buf, timeout_ms);
    if ret < 0 {
        Err(NetError::RecvFailed)
    } else {
        Ok(ret as usize)
    }
}

/// 低レベル: TCP 接続をクローズ（conn_id 指定）
pub fn raw_close(conn_id: u32) -> Result<(), NetError> {
    let ret = syscall::net_tcp_close(conn_id);
    if ret < 0 {
        Err(NetError::SendFailed)
    } else {
        Ok(())
    }
}

/// 低レベル: TCP リッスン開始
pub fn raw_listen(port: u16) -> Result<(), NetError> {
    let ret = syscall::net_tcp_listen(port);
    if ret < 0 { Err(NetError::ListenFailed) } else { Ok(()) }
}

/// 低レベル: TCP 接続の受け入れ
///
/// listen_port で指定したポートへの接続だけを accept する。
/// timeout_ms=0 で短いポーリング。成功時は conn_id を返す。
pub fn raw_accept(timeout_ms: u64, listen_port: u16) -> Result<u32, NetError> {
    let ret = syscall::net_tcp_accept(timeout_ms, listen_port);
    if ret < 0 {
        Err(NetError::AcceptFailed)
    } else {
        Ok(ret as u32)
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
    /// カーネルが管理するソケット ID
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
        let ret = syscall::net_udp_bind(port);
        if ret < 0 {
            return Err(NetError::UdpBindFailed);
        }
        // 戻り値: socket_id | (local_port << 32)
        let val = ret as u64;
        let socket_id = val as u32;
        let local_port = (val >> 32) as u16;
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
        let ret = syscall::net_udp_send_to(self.socket_id, &addr.ip.octets, addr.port, data);
        if ret < 0 {
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
        let mut src_info = [0u8; 6]; // [ip0, ip1, ip2, ip3, port_lo, port_hi]
        let ret = syscall::net_udp_recv_from(self.socket_id, buf, self.recv_timeout_ms, &mut src_info);
        if ret < 0 {
            return Err(if ret == -42 { NetError::Timeout } else { NetError::UdpRecvFailed });
        }
        let n = ret as usize;
        let src_ip = Ipv4Addr::new(src_info[0], src_info[1], src_info[2], src_info[3]);
        let src_port = u16::from_le_bytes([src_info[4], src_info[5]]);
        let addr = SocketAddr::new(src_ip, src_port);
        Ok((n, addr))
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
        let _ = syscall::net_udp_close(self.socket_id);
    }
}
