// sys/net/connection/sabos.rs — SABOS ネットワーク PAL 実装
//
// SABOS のネットワークスタックはカーネル内に実装されており、
// システムコール（int 0x80）で直接 DNS / TCP / UDP の操作を行う。
//
// この PAL は std::net::TcpStream / TcpListener / UdpSocket / lookup_host を
// カーネル syscall に接続する。IPv6 は SABOS が IPv4 のみのため unsupported。

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, ToSocketAddrs};
use crate::time::Duration;
use crate::vec;

// ============================================================
// syscall 番号（sabos-syscall crate は PAL から使えないためここに定義）
// ============================================================
const SYS_NET_DNS_LOOKUP: u64 = 40;
const SYS_NET_TCP_CONNECT: u64 = 41;
const SYS_NET_TCP_SEND: u64 = 42;
const SYS_NET_TCP_RECV: u64 = 43;
const SYS_NET_TCP_CLOSE: u64 = 44;
const SYS_NET_TCP_LISTEN: u64 = 150;
const SYS_NET_TCP_ACCEPT: u64 = 151;
const SYS_NET_UDP_BIND: u64 = 152;
const SYS_NET_UDP_SEND_TO: u64 = 153;
const SYS_NET_UDP_RECV_FROM: u64 = 154;
const SYS_NET_UDP_CLOSE: u64 = 155;

/// デフォルトの TCP recv タイムアウト（5 秒）
const DEFAULT_RECV_TIMEOUT_MS: u64 = 5000;

// ============================================================
// unsupported ヘルパー
// ============================================================
fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::UNSUPPORTED_PLATFORM)
}

// ============================================================
// syscall ラッパー（int 0x80 で直接呼び出し）
// ============================================================

/// 汎用 syscall: 引数1つ
#[inline]
fn syscall1(nr: u64, a1: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") a1,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// 汎用 syscall: 引数2つ
#[inline]
fn syscall2(nr: u64, a1: u64, a2: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") a1,
            in("rsi") a2,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// 汎用 syscall: 引数3つ
#[inline]
fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// 汎用 syscall: 引数4つ
#[inline]
fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

// ============================================================
// syscall 戻り値のエラー変換ヘルパー
// ============================================================

/// syscall の戻り値を io::Result<i64> に変換する。
/// 負の値はエラー、0以上は成功。
/// -42 は TimedOut として特別扱いする。
fn syscall_result(ret: u64, err_msg: &'static str) -> io::Result<i64> {
    let val = ret as i64;
    if val >= 0 {
        Ok(val)
    } else if val == -42 {
        Err(io::Error::new(io::ErrorKind::TimedOut, err_msg))
    } else {
        Err(io::Error::new(io::ErrorKind::Other, err_msg))
    }
}

// ============================================================
// UDP 引数構造体（sabos-syscall crate と同じレイアウト）
// ============================================================

/// UDP send_to の引数構造体
///
/// カーネルに渡す引数が多いため、構造体をスタック上に作ってポインタで渡す。
#[repr(C)]
struct UdpSendToArgs {
    socket_id: u32,
    dst_ip: [u8; 4],
    dst_port: u16,
    _pad: u16,
    data_ptr: u64,
    data_len: u64,
}

/// UDP recv_from の引数構造体
#[repr(C)]
struct UdpRecvFromArgs {
    socket_id: u32,
    _pad: u32,
    buf_ptr: u64,
    buf_len: u64,
    timeout_ms: u64,
    src_info_ptr: u64, // [u8; 6] = [ip0, ip1, ip2, ip3, port_lo, port_hi]
}

// ============================================================
// SocketAddr ↔ バイト列の変換ヘルパー
// ============================================================

/// SocketAddr から IPv4 アドレス 4 バイト + ポート 2 バイト LE を取得する。
/// IPv6 アドレスの場合はエラーを返す。
fn socket_addr_to_ipv4_port(addr: &SocketAddr) -> io::Result<([u8; 4], u16)> {
    match addr {
        SocketAddr::V4(v4) => Ok((v4.ip().octets(), v4.port())),
        SocketAddr::V6(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "IPv6 is not supported on SABOS",
        )),
    }
}

// ============================================================
// TcpStream
// ============================================================

/// TCP ストリーム: カーネルの conn_id で管理される TCP 接続
pub struct TcpStream {
    /// カーネルが管理するコネクション ID
    conn_id: u32,
    /// 接続先アドレス（connect 時に記録）
    peer_addr: SocketAddr,
    /// 読み取りタイムアウト
    read_timeout: Option<Duration>,
    /// 書き込みタイムアウト
    write_timeout: Option<Duration>,
}

impl TcpStream {
    /// 指定アドレスに TCP 接続する
    ///
    /// addr が複数のアドレスに解決される場合、最初の IPv4 アドレスに接続を試みる。
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        let addrs = addr.to_socket_addrs()?;
        let mut last_err = io::Error::new(io::ErrorKind::InvalidInput, "no addresses resolved");
        for a in addrs {
            match Self::connect_inner(&a) {
                Ok(s) => return Ok(s),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// タイムアウト付きで指定アドレスに TCP 接続する
    ///
    /// カーネルの TCP_CONNECT は内部タイムアウトを持つため、
    /// 外部からのタイムアウト制御は未対応だが、接続自体は行う。
    pub fn connect_timeout(addr: &SocketAddr, _timeout: Duration) -> io::Result<TcpStream> {
        Self::connect_inner(addr)
    }

    /// 単一の SocketAddr に対して TCP 接続を行う内部関数
    ///
    /// SYS_NET_TCP_CONNECT(ip_ptr, port) → conn_id（成功時）/ 負（エラー時）
    fn connect_inner(addr: &SocketAddr) -> io::Result<TcpStream> {
        let (ip_bytes, port) = socket_addr_to_ipv4_port(addr)?;

        let ret = syscall2(
            SYS_NET_TCP_CONNECT,
            ip_bytes.as_ptr() as u64,
            port as u64,
        );
        let conn_id = syscall_result(ret, "TCP connect failed")?;

        Ok(TcpStream {
            conn_id: conn_id as u32,
            peer_addr: *addr,
            read_timeout: None,
            write_timeout: None,
        })
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        // &self しかないため unsafe で変更する（std PAL の慣例）
        let self_mut = unsafe { &mut *(self as *const Self as *mut Self) };
        self_mut.read_timeout = dur;
        Ok(())
    }

    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        let self_mut = unsafe { &mut *(self as *const Self as *mut Self) };
        self_mut.write_timeout = dur;
        Ok(())
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.read_timeout)
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.write_timeout)
    }

    pub fn peek(&self, _buf: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        // タイムアウト値を決定（デフォルト 5 秒）
        let timeout_ms = self
            .read_timeout
            .map(|d| d.as_millis() as u64)
            .unwrap_or(DEFAULT_RECV_TIMEOUT_MS);

        // SYS_NET_TCP_RECV(conn_id, buf_ptr, buf_len, timeout_ms) → 受信バイト数
        let ret = syscall4(
            SYS_NET_TCP_RECV,
            self.conn_id as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            timeout_ms,
        );

        let n = syscall_result(ret, "TCP recv timed out")
            .map_err(|e| {
                if e.kind() == io::ErrorKind::TimedOut {
                    e
                } else {
                    io::Error::new(io::ErrorKind::ConnectionReset, "TCP recv failed")
                }
            })?;
        Ok(n as usize)
    }

    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_>) -> io::Result<()> {
        let buf = cursor.ensure_init();
        let n = self.read(buf.init_mut())?;
        cursor.advance(n);
        Ok(())
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        // 最初の非空バッファだけに読む
        for buf in bufs {
            if !buf.is_empty() {
                return self.read(buf);
            }
        }
        Ok(0)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // SYS_NET_TCP_SEND(conn_id, data_ptr, data_len) → 0（成功）/ 負（エラー）
        let ret = syscall3(
            SYS_NET_TCP_SEND,
            self.conn_id as u64,
            buf.as_ptr() as u64,
            buf.len() as u64,
        );
        syscall_result(ret, "TCP send failed")
            .map_err(|_| io::Error::new(io::ErrorKind::ConnectionReset, "TCP send failed"))?;
        Ok(buf.len())
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        // 最初の非空バッファだけを書く
        for buf in bufs {
            if !buf.is_empty() {
                return self.write(buf);
            }
        }
        Ok(0)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        // ローカルアドレスは不明 → 0.0.0.0:0 を返す
        Ok(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            0,
        )))
    }

    pub fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
        // half-close 未対応なので TCP_CLOSE で全閉じする
        syscall1(SYS_NET_TCP_CLOSE, self.conn_id as u64);
        Ok(())
    }

    pub fn duplicate(&self) -> io::Result<TcpStream> {
        unsupported()
    }

    pub fn set_linger(&self, _linger: Option<Duration>) -> io::Result<()> {
        unsupported()
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        unsupported()
    }

    pub fn set_nodelay(&self, _nodelay: bool) -> io::Result<()> {
        // スタブ: Nagle 制御未対応だが、エラーにはしない
        Ok(())
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        // 常に true（SABOS の TCP は小さなパケットをすぐ送る）
        Ok(true)
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn ttl(&self) -> io::Result<u32> {
        Ok(64)
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
        unsupported()
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        // コネクションを自動クローズ（エラーは無視）
        syscall1(SYS_NET_TCP_CLOSE, self.conn_id as u64);
    }
}

impl fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpStream")
            .field("conn_id", &self.conn_id)
            .field("peer_addr", &self.peer_addr)
            .finish()
    }
}

// ============================================================
// TcpListener
// ============================================================

/// TCP リスナー: カーネルの listen/accept で管理される
pub struct TcpListener {
    /// リッスンしているポート番号
    port: u16,
}

impl TcpListener {
    /// 指定アドレスでリッスンを開始する
    ///
    /// addr のポート番号でリッスンする。IP アドレスは無視（SABOS は 0.0.0.0 固定）。
    /// SYS_NET_TCP_LISTEN(port) → 0（成功）/ 負（エラー）
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let addrs = addr.to_socket_addrs()?;
        let mut last_err =
            io::Error::new(io::ErrorKind::InvalidInput, "no addresses to bind to");
        for a in addrs {
            let port = a.port();
            let ret = syscall1(SYS_NET_TCP_LISTEN, port as u64);
            match syscall_result(ret, "TCP listen failed") {
                Ok(_) => return Ok(TcpListener { port }),
                Err(_) => last_err = io::Error::new(io::ErrorKind::AddrInUse, "TCP listen failed"),
            }
        }
        Err(last_err)
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            self.port,
        )))
    }

    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        // SYS_NET_TCP_ACCEPT(timeout_ms, listen_port) → conn_id
        // timeout_ms = 0 でブロッキング待ち
        let ret = syscall2(SYS_NET_TCP_ACCEPT, 0, self.port as u64);
        let conn_id = syscall_result(ret, "TCP accept failed")
            .map_err(|_| io::Error::new(io::ErrorKind::ConnectionAborted, "TCP accept failed"))?;

        // accept 時のクライアントアドレスはカーネルが返さないので 0.0.0.0:0 とする
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0));
        Ok((
            TcpStream {
                conn_id: conn_id as u32,
                peer_addr: peer,
                read_timeout: None,
                write_timeout: None,
            },
            peer,
        ))
    }

    pub fn duplicate(&self) -> io::Result<TcpListener> {
        unsupported()
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn ttl(&self) -> io::Result<u32> {
        Ok(64)
    }

    pub fn set_only_v6(&self, _only_v6: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn only_v6(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
        unsupported()
    }
}

impl fmt::Debug for TcpListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpListener")
            .field("port", &self.port)
            .finish()
    }
}

// ============================================================
// UdpSocket — カーネル syscall 経由で UDP 通信を行う
// ============================================================

/// UDP ソケット: カーネルの socket_id で管理される
pub struct UdpSocket {
    /// カーネルが管理するソケット ID
    socket_id: u32,
    /// バインドしているローカルアドレス
    local_addr: SocketAddr,
    /// 読み取りタイムアウト
    read_timeout: Option<Duration>,
    /// 書き込みタイムアウト（SABOS では未使用だが API 互換のため保持）
    write_timeout: Option<Duration>,
    /// connect() で設定されたデフォルト送信先アドレス
    connected_addr: Option<SocketAddr>,
}

impl UdpSocket {
    /// 指定アドレスにバインドして UDP ソケットを作成する
    ///
    /// SYS_NET_UDP_BIND(port) → socket_id | (local_port << 32)
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        let addrs = addr.to_socket_addrs()?;
        let mut last_err = io::Error::new(io::ErrorKind::InvalidInput, "no addresses to bind to");
        for a in addrs {
            let port = a.port();
            let ret = syscall1(SYS_NET_UDP_BIND, port as u64);
            let val = ret as i64;
            if val >= 0 {
                // 戻り値: socket_id(下位32bit) | local_port(上位32bit)
                let socket_id = ret as u32;
                let local_port = (ret >> 32) as u16;
                return Ok(UdpSocket {
                    socket_id,
                    local_addr: SocketAddr::V4(SocketAddrV4::new(
                        Ipv4Addr::new(0, 0, 0, 0),
                        local_port,
                    )),
                    read_timeout: None,
                    write_timeout: None,
                    connected_addr: None,
                });
            }
            last_err = io::Error::new(io::ErrorKind::AddrInUse, "UDP bind failed");
        }
        Err(last_err)
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.connected_addr.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "UDP socket not connected")
        })
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let timeout_ms = self
            .read_timeout
            .map(|d| d.as_millis() as u64)
            .unwrap_or(DEFAULT_RECV_TIMEOUT_MS);

        // 送信元情報: [ip0, ip1, ip2, ip3, port_lo, port_hi]
        let mut src_info = [0u8; 6];
        let args = UdpRecvFromArgs {
            socket_id: self.socket_id,
            _pad: 0,
            buf_ptr: buf.as_mut_ptr() as u64,
            buf_len: buf.len() as u64,
            timeout_ms,
            src_info_ptr: src_info.as_mut_ptr() as u64,
        };

        let ret = syscall1(SYS_NET_UDP_RECV_FROM, &args as *const _ as u64);
        let n = syscall_result(ret, "UDP recv timed out")
            .map_err(|e| {
                if e.kind() == io::ErrorKind::TimedOut {
                    e
                } else {
                    io::Error::new(io::ErrorKind::Other, "UDP recv failed")
                }
            })?;

        let src_ip = Ipv4Addr::new(src_info[0], src_info[1], src_info[2], src_info[3]);
        let src_port = u16::from_le_bytes([src_info[4], src_info[5]]);
        let addr = SocketAddr::V4(SocketAddrV4::new(src_ip, src_port));
        Ok((n as usize, addr))
    }

    pub fn peek_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        unsupported()
    }

    pub fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> io::Result<usize> {
        let (ip_bytes, port) = socket_addr_to_ipv4_port(addr)?;

        let args = UdpSendToArgs {
            socket_id: self.socket_id,
            dst_ip: ip_bytes,
            dst_port: port,
            _pad: 0,
            data_ptr: buf.as_ptr() as u64,
            data_len: buf.len() as u64,
        };

        let ret = syscall1(SYS_NET_UDP_SEND_TO, &args as *const _ as u64);
        syscall_result(ret, "UDP send failed")
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "UDP send failed"))?;
        Ok(buf.len())
    }

    pub fn duplicate(&self) -> io::Result<UdpSocket> {
        unsupported()
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        let self_mut = unsafe { &mut *(self as *const Self as *mut Self) };
        self_mut.read_timeout = dur;
        Ok(())
    }

    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        let self_mut = unsafe { &mut *(self as *const Self as *mut Self) };
        self_mut.write_timeout = dur;
        Ok(())
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.read_timeout)
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.write_timeout)
    }

    pub fn set_broadcast(&self, _: bool) -> io::Result<()> {
        Ok(())
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        Ok(false)
    }

    pub fn set_multicast_loop_v4(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn set_multicast_ttl_v4(&self, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn set_multicast_loop_v6(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn join_multicast_v4(&self, _: &Ipv4Addr, _: &Ipv4Addr) -> io::Result<()> {
        unsupported()
    }

    pub fn join_multicast_v6(&self, _: &Ipv6Addr, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn leave_multicast_v4(&self, _: &Ipv4Addr, _: &Ipv4Addr) -> io::Result<()> {
        unsupported()
    }

    pub fn leave_multicast_v6(&self, _: &Ipv6Addr, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn set_ttl(&self, _: u32) -> io::Result<()> {
        Ok(())
    }

    pub fn ttl(&self) -> io::Result<u32> {
        Ok(64)
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let (n, _addr) = self.recv_from(buf)?;
        Ok(n)
    }

    pub fn peek(&self, _: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let addr = self.connected_addr.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "UDP socket not connected")
        })?;
        self.send_to(buf, &addr)
    }

    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        let addrs = addr.to_socket_addrs()?;
        for a in addrs {
            let self_mut = unsafe { &mut *(self as *const Self as *mut Self) };
            self_mut.connected_addr = Some(a);
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no addresses resolved",
        ))
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        // ソケットを自動クローズ（エラーは無視）
        syscall1(SYS_NET_UDP_CLOSE, self.socket_id as u64);
    }
}

impl fmt::Debug for UdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdpSocket")
            .field("socket_id", &self.socket_id)
            .field("local_addr", &self.local_addr)
            .field("connected_addr", &self.connected_addr)
            .finish()
    }
}

// ============================================================
// LookupHost — DNS 名前解決
// ============================================================

/// DNS 名前解決の結果を保持するイテレータ
pub struct LookupHost {
    /// 解決されたアドレスのリスト
    addrs: vec::Vec<SocketAddr>,
    /// 現在の位置
    pos: usize,
}

impl Iterator for LookupHost {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<SocketAddr> {
        if self.pos < self.addrs.len() {
            let addr = self.addrs[self.pos];
            self.pos += 1;
            Some(addr)
        } else {
            None
        }
    }
}

impl fmt::Debug for LookupHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.addrs.iter())
            .finish()
    }
}

/// DNS 名前解決を行う
///
/// host が IP アドレスリテラル（"1.2.3.4"）の場合はパースして直接返す。
/// ドメイン名の場合は SYS_NET_DNS_LOOKUP で解決する。
///
/// SYS_NET_DNS_LOOKUP(domain_ptr, domain_len, result_ip_ptr) → 0（成功）/ 負（エラー）
pub fn lookup_host(host: &str, port: u16) -> io::Result<LookupHost> {
    // まず IP アドレスリテラルとしてパースを試みる
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(LookupHost {
            addrs: vec![SocketAddr::V4(SocketAddrV4::new(ip, port))],
            pos: 0,
        });
    }

    // ドメイン名として DNS 解決する
    let mut result_ip = [0u8; 4];
    let ret = syscall3(
        SYS_NET_DNS_LOOKUP,
        host.as_ptr() as u64,
        host.len() as u64,
        result_ip.as_mut_ptr() as u64,
    );
    syscall_result(ret, "DNS lookup failed")?;

    let ip = Ipv4Addr::new(result_ip[0], result_ip[1], result_ip[2], result_ip[3]);
    Ok(LookupHost {
        addrs: vec![SocketAddr::V4(SocketAddrV4::new(ip, port))],
        pos: 0,
    })
}
