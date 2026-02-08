// sys/net/connection/sabos.rs — SABOS ネットワーク PAL 実装
//
// SABOS のネットワークスタックは netd デーモン（ユーザー空間）が担当しており、
// IPC メッセージ経由で DNS / TCP の操作を行う。
//
// この PAL は std::net::TcpStream / TcpListener / lookup_host を netd IPC に接続する。
// UdpSocket は netd が UDP 未対応のため unsupported。
// IPv6 も SABOS が IPv4 のみのため unsupported。
//
// ## IPC プロトコル（netd）
//
// リクエスト: [opcode:u32 LE][payload_len:u32 LE][payload...]
// レスポンス: [opcode:u32 LE][status:i32 LE][data_len:u32 LE][data...]
//
// | opcode | 操作          | payload                        | response data |
// |--------|--------------|-------------------------------|---------------|
// | 1      | DNS_LOOKUP   | domain string                  | 4 bytes IPv4  |
// | 2      | TCP_CONNECT  | 4B IP + 2B port LE             | 4B conn_id    |
// | 3      | TCP_SEND     | 4B conn_id + data              | (なし)        |
// | 4      | TCP_RECV     | 4B conn_id + 4B max_len + 8B timeout_ms | data bytes |
// | 5      | TCP_CLOSE    | 4B conn_id                     | (なし)        |
// | 6      | TCP_LISTEN   | 2B port LE                     | (なし)        |
// | 7      | TCP_ACCEPT   | 8B timeout_ms                  | 4B conn_id    |

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, ToSocketAddrs};
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::time::Duration;
use crate::vec;

// ============================================================
// syscall 番号
// ============================================================
const SYS_IPC_SEND: u64 = 90;
const SYS_IPC_RECV: u64 = 91;
const SYS_GET_TASK_LIST: u64 = 21;

// ============================================================
// netd IPC プロトコル定数
// ============================================================
const OPCODE_DNS_LOOKUP: u32 = 1;
const OPCODE_TCP_CONNECT: u32 = 2;
const OPCODE_TCP_SEND: u32 = 3;
const OPCODE_TCP_RECV: u32 = 4;
const OPCODE_TCP_CLOSE: u32 = 5;
const OPCODE_TCP_LISTEN: u32 = 6;
const OPCODE_TCP_ACCEPT: u32 = 7;

/// IPC リクエストヘッダサイズ: opcode(4) + payload_len(4) = 8 バイト
const IPC_REQ_HEADER: usize = 8;
/// IPC レスポンスヘッダサイズ: opcode(4) + status(4) + data_len(4) = 12 バイト
const IPC_RESP_HEADER: usize = 12;
/// IPC バッファサイズ
const IPC_BUF_SIZE: usize = 2048;
/// タスク一覧取得用バッファサイズ
const TASK_LIST_BUF_SIZE: usize = 4096;
/// デフォルトの IPC 受信タイムアウト（10 秒）
/// TCP_CONNECT は 3-way handshake + QEMU NAT 越えで時間がかかるため余裕を持たせる
const DEFAULT_IPC_TIMEOUT_MS: u64 = 10000;
/// デフォルトの TCP recv タイムアウト（5 秒）
const DEFAULT_RECV_TIMEOUT_MS: u64 = 5000;

// ============================================================
// netd タスク ID のキャッシュ（遅延初期化）
// ============================================================
static NETD_ID: AtomicU64 = AtomicU64::new(0);

// ============================================================
// unsupported ヘルパー
// ============================================================
fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::UNSUPPORTED_PLATFORM)
}

// ============================================================
// syscall ラッパー（int 0x80 で直接呼び出し）
// ============================================================

/// SYS_IPC_SEND(90): IPC メッセージを送信する
///
/// rdi = 送信先タスク ID, rsi = バッファポインタ, rdx = バッファ長
/// 戻り値: 0 = 成功, 負 = エラー
fn syscall_ipc_send(dest: u64, buf: &[u8]) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_IPC_SEND,
            in("rdi") dest,
            in("rsi") buf.as_ptr() as u64,
            in("rdx") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_IPC_RECV(91): IPC メッセージを受信する
///
/// rdi = 送信元タスク ID 格納先ポインタ, rsi = バッファポインタ,
/// rdx = バッファ長, r10 = タイムアウト(ms, 0=無限待ち)
/// 戻り値: 受信バイト数（正）, 負 = エラー
fn syscall_ipc_recv(sender: &mut u64, buf: &mut [u8], timeout_ms: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_IPC_RECV,
            in("rdi") sender as *mut u64 as u64,
            in("rsi") buf.as_mut_ptr() as u64,
            in("rdx") buf.len() as u64,
            in("r10") timeout_ms,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

/// SYS_GET_TASK_LIST(36): タスク一覧 JSON を取得する
///
/// rdi = バッファポインタ, rsi = バッファ長
/// 戻り値: JSON バイト数（正）, 負 = エラー
fn syscall_get_task_list(buf: &mut [u8]) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") SYS_GET_TASK_LIST,
            in("rdi") buf.as_mut_ptr() as u64,
            in("rsi") buf.len() as u64,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret as i64
}

// ============================================================
// netd 検索・IPC 通信
// ============================================================

/// netd のタスク ID を取得する（キャッシュ済みならそれを返す）
///
/// タスク一覧 JSON から "NETD.ELF" を探して ID をキャッシュする。
/// 簡易 JSON パーサーで "name":"NETD.ELF" と "id":<数字> を抽出する。
fn ensure_netd() -> io::Result<u64> {
    let cached = NETD_ID.load(Ordering::Relaxed);
    if cached != 0 {
        return Ok(cached);
    }

    // タスク一覧を取得して NETD.ELF を探す
    let mut buf = [0u8; TASK_LIST_BUF_SIZE];
    let n = syscall_get_task_list(&mut buf);
    if n < 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "failed to get task list",
        ));
    }
    let json = core::str::from_utf8(&buf[..n as usize])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8 in task list"))?;

    // 簡易パース: "name":"NETD.ELF" を含むオブジェクトの "id" を取得
    // JSON 構造: {"tasks":[{"id":2,"name":"INIT.ELF",...},{"id":5,"name":"NETD.ELF",...}]}
    let target = "\"NETD.ELF\"";
    if let Some(pos) = json.find(target) {
        // この位置を含むオブジェクト {...} を見つけて "id" を取得する
        // オブジェクト開始位置を逆方向に探す
        let obj_start = json[..pos].rfind('{').unwrap_or(0);
        // オブジェクト終了位置を順方向に探す
        let obj_end = json[pos..].find('}').map(|p| pos + p).unwrap_or(json.len());
        let obj = &json[obj_start..=obj_end.min(json.len() - 1)];

        // "id": の後の数字を抽出
        if let Some(id_pos) = obj.find("\"id\":") {
            let after_id = &obj[id_pos + 5..];
            // 先頭の空白をスキップ
            let trimmed = after_id.trim_start();
            // 数字を読む
            let mut id_val: u64 = 0;
            for ch in trimmed.chars() {
                if ch.is_ascii_digit() {
                    id_val = id_val * 10 + (ch as u64 - '0' as u64);
                } else {
                    break;
                }
            }
            if id_val != 0 {
                NETD_ID.store(id_val, Ordering::Relaxed);
                return Ok(id_val);
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "netd not found in task list",
    ))
}

/// netd に IPC リクエストを送信し、レスポンスを受け取る共通関数
///
/// IPC 失敗時は netd の PID を再解決して 1 回だけリトライする。
/// 戻り値: (status, data_len) — status < 0 はアプリケーションレベルのエラー。
/// resp_buf の先頭 data_len バイトにレスポンスデータが格納される。
fn netd_request(opcode: u32, payload: &[u8], resp_buf: &mut [u8]) -> io::Result<(i32, usize)> {
    // リクエストバッファを構築: [opcode:4][payload_len:4][payload...]
    let req_len = IPC_REQ_HEADER + payload.len();
    if req_len > IPC_BUF_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC request too large",
        ));
    }
    let mut req = [0u8; IPC_BUF_SIZE];
    req[0..4].copy_from_slice(&opcode.to_le_bytes());
    req[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    req[8..8 + payload.len()].copy_from_slice(payload);

    // netd タスク ID を取得
    let mut netd_id = ensure_netd()?;

    // IPC 送信（失敗時はリトライ）
    if syscall_ipc_send(netd_id, &req[..req_len]) < 0 {
        // netd が再起動した可能性 → キャッシュをクリアして再検索
        NETD_ID.store(0, Ordering::Relaxed);
        netd_id = ensure_netd()?;
        if syscall_ipc_send(netd_id, &req[..req_len]) < 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "failed to send IPC to netd",
            ));
        }
    }

    // IPC 受信（10 秒タイムアウト — TCP_CONNECT 等の遅い操作に対応）
    let mut sender = 0u64;
    let mut recv_buf = [0u8; IPC_BUF_SIZE];
    let n = syscall_ipc_recv(&mut sender, &mut recv_buf, DEFAULT_IPC_TIMEOUT_MS);
    if n < 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "IPC recv from netd timed out",
        ));
    }
    let n = n as usize;
    if n < IPC_RESP_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC response too short",
        ));
    }

    // レスポンスヘッダをパース
    let resp_opcode = u32::from_le_bytes([recv_buf[0], recv_buf[1], recv_buf[2], recv_buf[3]]);
    if resp_opcode != opcode {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC response opcode mismatch",
        ));
    }
    let status = i32::from_le_bytes([recv_buf[4], recv_buf[5], recv_buf[6], recv_buf[7]]);
    let data_len =
        u32::from_le_bytes([recv_buf[8], recv_buf[9], recv_buf[10], recv_buf[11]]) as usize;
    if IPC_RESP_HEADER + data_len > n {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "IPC response data truncated",
        ));
    }

    // レスポンスデータを resp_buf の先頭にコピー
    let copy_len = data_len.min(resp_buf.len());
    resp_buf[..copy_len].copy_from_slice(&recv_buf[IPC_RESP_HEADER..IPC_RESP_HEADER + copy_len]);

    Ok((status, data_len))
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

/// TCP ストリーム: netd の conn_id で管理される TCP 接続
pub struct TcpStream {
    /// netd が管理するコネクション ID
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
    /// SABOS の netd は接続タイムアウトを直接サポートしないが、
    /// 標準のインターフェースとして提供する。実際には通常の connect と同じ。
    pub fn connect_timeout(addr: &SocketAddr, _timeout: Duration) -> io::Result<TcpStream> {
        // netd の TCP_CONNECT 自体が内部タイムアウトを持つ
        // 外部からのタイムアウト制御は未対応だが、接続自体は行う
        Self::connect_inner(addr)
    }

    /// 単一の SocketAddr に対して TCP 接続を行う内部関数
    fn connect_inner(addr: &SocketAddr) -> io::Result<TcpStream> {
        let (ip_bytes, port) = socket_addr_to_ipv4_port(addr)?;

        // payload: 4B IP + 2B port LE
        let mut payload = [0u8; 6];
        payload[0..4].copy_from_slice(&ip_bytes);
        payload[4..6].copy_from_slice(&port.to_le_bytes());

        let mut resp = [0u8; 16];
        let (status, len) = netd_request(OPCODE_TCP_CONNECT, &payload, &mut resp)?;
        if status < 0 || len != 4 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "TCP connect failed",
            ));
        }
        let conn_id = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
        Ok(TcpStream {
            conn_id,
            peer_addr: *addr,
            read_timeout: None,
            write_timeout: None,
        })
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        // self は &self だが、内部的にタイムアウトは read 時に使うので
        // ここでは安全に interior mutability を使う
        // std の実装ではカーネル側にセットするのが普通だが、
        // SABOS では recv 呼び出し時に直接タイムアウト値を渡すため、
        // フィールドに保存する必要がある。
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
        // netd は peek 未対応
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        // タイムアウト値を決定（デフォルト 5 秒）
        let timeout_ms = self
            .read_timeout
            .map(|d| d.as_millis() as u64)
            .unwrap_or(DEFAULT_RECV_TIMEOUT_MS);

        // payload: 4B conn_id + 4B max_len + 8B timeout_ms
        let max_len = buf.len() as u32;
        let mut payload = [0u8; 16];
        payload[0..4].copy_from_slice(&self.conn_id.to_le_bytes());
        payload[4..8].copy_from_slice(&max_len.to_le_bytes());
        payload[8..16].copy_from_slice(&timeout_ms.to_le_bytes());

        let mut resp = [0u8; IPC_BUF_SIZE];
        let (status, len) = netd_request(OPCODE_TCP_RECV, &payload, &mut resp)?;

        // status == -42 はタイムアウト（データなし）
        if status == -42 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TCP recv timed out",
            ));
        }
        if status < 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "TCP recv failed",
            ));
        }
        let copy_len = len.min(buf.len());
        buf[..copy_len].copy_from_slice(&resp[..copy_len]);
        Ok(copy_len)
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

        // IPC バッファの制限を考慮してチャンクサイズを制限
        // payload: 4B conn_id + data → data は最大 IPC_BUF_SIZE - IPC_REQ_HEADER - 4
        let max_chunk = IPC_BUF_SIZE - IPC_REQ_HEADER - 4;
        let chunk_len = buf.len().min(max_chunk);

        // payload: 4B conn_id + data
        let mut payload = vec![0u8; 4 + chunk_len];
        payload[0..4].copy_from_slice(&self.conn_id.to_le_bytes());
        payload[4..4 + chunk_len].copy_from_slice(&buf[..chunk_len]);

        let mut resp = [0u8; 32];
        let (status, _) = netd_request(OPCODE_TCP_SEND, &payload, &mut resp)?;
        if status < 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "TCP send failed",
            ));
        }
        Ok(chunk_len)
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
        // ローカルアドレスは不明（netd が情報を返さないため）
        // 0.0.0.0:0 を返す
        Ok(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            0,
        )))
    }

    pub fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
        // netd は half-close 未対応なので TCP_CLOSE で全閉じする
        let payload = self.conn_id.to_le_bytes();
        let mut resp = [0u8; 32];
        let _ = netd_request(OPCODE_TCP_CLOSE, &payload, &mut resp);
        Ok(())
    }

    pub fn duplicate(&self) -> io::Result<TcpStream> {
        // netd は conn_id の複製を未対応
        unsupported()
    }

    pub fn set_linger(&self, _linger: Option<Duration>) -> io::Result<()> {
        unsupported()
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        unsupported()
    }

    pub fn set_nodelay(&self, _nodelay: bool) -> io::Result<()> {
        // スタブ: netd は Nagle 制御未対応だが、エラーにはしない
        Ok(())
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        // 常に true（SABOS の TCP は小さなパケットをすぐ送る）
        Ok(true)
    }

    pub fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        // スタブ: TTL 設定は未対応だがエラーにはしない
        Ok(())
    }

    pub fn ttl(&self) -> io::Result<u32> {
        Ok(64)
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
        // netd はノンブロッキングモード未対応
        unsupported()
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        // コネクションを自動クローズ（エラーは無視）
        let payload = self.conn_id.to_le_bytes();
        let mut resp = [0u8; 32];
        let _ = netd_request(OPCODE_TCP_CLOSE, &payload, &mut resp);
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

/// TCP リスナー: netd の listen/accept で管理される
pub struct TcpListener {
    /// リッスンしているポート番号
    port: u16,
}

impl TcpListener {
    /// 指定アドレスでリッスンを開始する
    ///
    /// addr のポート番号でリッスンする。IP アドレスは無視（SABOS は 0.0.0.0 固定）。
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        let addrs = addr.to_socket_addrs()?;
        let mut last_err =
            io::Error::new(io::ErrorKind::InvalidInput, "no addresses to bind to");
        for a in addrs {
            let port = a.port();
            let payload = port.to_le_bytes();
            let mut resp = [0u8; 32];
            match netd_request(OPCODE_TCP_LISTEN, &payload, &mut resp) {
                Ok((status, _)) if status >= 0 => {
                    return Ok(TcpListener { port });
                }
                Ok(_) => {
                    last_err = io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "TCP listen failed",
                    );
                }
                Err(e) => {
                    last_err = e;
                }
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
        // timeout_ms = 0 でブロッキング待ち
        let payload = 0u64.to_le_bytes();
        let mut resp = [0u8; 32];
        let (status, len) = netd_request(OPCODE_TCP_ACCEPT, &payload, &mut resp)?;
        if status < 0 || len != 4 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "TCP accept failed",
            ));
        }
        let conn_id = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
        // accept 時のクライアントアドレスは netd が返さないので 0.0.0.0:0 とする
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 0));
        Ok((
            TcpStream {
                conn_id,
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
// UdpSocket（unsupported — netd が UDP 未対応のため）
// ============================================================

pub struct UdpSocket(!);

impl UdpSocket {
    pub fn bind<A: ToSocketAddrs>(_: A) -> io::Result<UdpSocket> {
        unsupported()
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.0
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        self.0
    }

    pub fn recv_from(&self, _: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0
    }

    pub fn peek_from(&self, _: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0
    }

    pub fn send_to(&self, _: &[u8], _: &SocketAddr) -> io::Result<usize> {
        self.0
    }

    pub fn duplicate(&self) -> io::Result<UdpSocket> {
        self.0
    }

    pub fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        self.0
    }

    pub fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        self.0
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        self.0
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        self.0
    }

    pub fn set_broadcast(&self, _: bool) -> io::Result<()> {
        self.0
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        self.0
    }

    pub fn set_multicast_loop_v4(&self, _: bool) -> io::Result<()> {
        self.0
    }

    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        self.0
    }

    pub fn set_multicast_ttl_v4(&self, _: u32) -> io::Result<()> {
        self.0
    }

    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        self.0
    }

    pub fn set_multicast_loop_v6(&self, _: bool) -> io::Result<()> {
        self.0
    }

    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        self.0
    }

    pub fn join_multicast_v4(&self, _: &Ipv4Addr, _: &Ipv4Addr) -> io::Result<()> {
        self.0
    }

    pub fn join_multicast_v6(&self, _: &Ipv6Addr, _: u32) -> io::Result<()> {
        self.0
    }

    pub fn leave_multicast_v4(&self, _: &Ipv4Addr, _: &Ipv4Addr) -> io::Result<()> {
        self.0
    }

    pub fn leave_multicast_v6(&self, _: &Ipv6Addr, _: u32) -> io::Result<()> {
        self.0
    }

    pub fn set_ttl(&self, _: u32) -> io::Result<()> {
        self.0
    }

    pub fn ttl(&self) -> io::Result<u32> {
        self.0
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.0
    }

    pub fn set_nonblocking(&self, _: bool) -> io::Result<()> {
        self.0
    }

    pub fn recv(&self, _: &mut [u8]) -> io::Result<usize> {
        self.0
    }

    pub fn peek(&self, _: &mut [u8]) -> io::Result<usize> {
        self.0
    }

    pub fn send(&self, _: &[u8]) -> io::Result<usize> {
        self.0
    }

    pub fn connect<A: ToSocketAddrs>(&self, _: A) -> io::Result<()> {
        self.0
    }
}

impl fmt::Debug for UdpSocket {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
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
/// ドメイン名の場合は netd の DNS_LOOKUP(opcode 1) で解決する。
pub fn lookup_host(host: &str, port: u16) -> io::Result<LookupHost> {
    // まず IP アドレスリテラルとしてパースを試みる
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(LookupHost {
            addrs: vec![SocketAddr::V4(SocketAddrV4::new(ip, port))],
            pos: 0,
        });
    }

    // ドメイン名として DNS 解決する
    let payload = host.as_bytes();
    let mut resp = [0u8; 16];
    let (status, len) = netd_request(OPCODE_DNS_LOOKUP, payload, &mut resp)?;
    if status < 0 || len != 4 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "DNS lookup failed",
        ));
    }

    let ip = Ipv4Addr::new(resp[0], resp[1], resp[2], resp[3]);
    Ok(LookupHost {
        addrs: vec![SocketAddr::V4(SocketAddrV4::new(ip, port))],
        pos: 0,
    })
}
