// virtio_9p.rs — virtio-9p ドライバ (9P2000.L プロトコル)
//
// 9P (Plan 9 Filesystem Protocol) は Plan 9 OS で設計されたファイル共有プロトコル。
// 9P2000.L は Linux 向け拡張で、POSIX 互換のファイル属性やディレクトリ読み取りをサポートする。
//
// virtio-9p は virtio トランスポート上で 9P プロトコルを運ぶ仮想デバイス。
// QEMU の `-virtfs` オプションで使われ、ホストのディレクトリをゲストにリアルタイム共有する。
// ホスト側でファイルを更新すると、ゲストから即座に最新版にアクセスできるため、
// 開発時のビルド→テストサイクルが大幅に短縮される。
//
// ## プロトコル概要
//
// 9P はクライアント（ゲスト）とサーバー（ホスト）間のメッセージパッシングプロトコル。
// 各メッセージは以下の形式:
//
//   size[4] type[1] tag[2] ...payload...
//
// - size: メッセージ全体のバイト数（size フィールド自身を含む）
// - type: メッセージの種類（Tversion=100, Rattach=105, etc.）
// - tag: リクエストの識別子（レスポンスと対応付けるため）
//
// ## fid (File Identifier)
//
// 9P ではファイルやディレクトリを fid（32ビット整数）で識別する。
// - attach: サーバーのルートに fid を割り当てる
// - walk: 既存 fid からパスを辿って新しい fid を割り当てる
// - lopen: fid を開く（読み取り/書き込み可能にする）
// - read/readdir: 開いた fid からデータを読む
// - clunk: fid を解放する（ファイルディスクリプタの close に相当）
//
// ## virtio トランスポート
//
// virtio-9p は単一の Virtqueue (queue 0) を使用する。
// リクエスト（Txxx）とレスポンス（Rxxx）は 2 つのディスクリプタチェーンで送受信する:
//   [0] 送信バッファ（device-readable）= T メッセージ
//   [1] 受信バッファ（device-writable）= R メッセージ

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{fence, Ordering};
use spin::Mutex;
use x86_64::instructions::port::Port;

use crate::pci;
use crate::serial_println;
use crate::vfs::{FileSystem, VfsDirEntry, VfsError, VfsNode, VfsNodeKind};

// ============================================================
// グローバルインスタンス
// ============================================================

/// グローバルな virtio-9p ドライバインスタンス。
/// init() で初期化される。virtio-9p デバイスがない場合は None のまま。
pub static VIRTIO_9P: Mutex<Option<Virtio9p>> = Mutex::new(None);

// ============================================================
// 9P2000.L メッセージタイプ定数
// ============================================================

/// エラーレスポンス（9P2000.L 固有、errno を返す）
const P9_RLERROR: u8 = 7;
/// ファイル/ディレクトリを開く（9P2000.L 固有）
const P9_TLOPEN: u8 = 12;
const P9_RLOPEN: u8 = 13;
/// ファイル属性を取得（9P2000.L 固有）
const P9_TGETATTR: u8 = 24;
const P9_RGETATTR: u8 = 25;
/// ディレクトリエントリを読む（9P2000.L 固有）
const P9_TREADDIR: u8 = 40;
const P9_RREADDIR: u8 = 41;
/// プロトコルバージョンネゴシエーション（9P2000 共通）
const P9_TVERSION: u8 = 100;
const P9_RVERSION: u8 = 101;
/// サーバーのファイルツリーにアタッチ（9P2000 共通）
const P9_TATTACH: u8 = 104;
const P9_RATTACH: u8 = 105;
/// パスを辿って新しい fid を割り当て（9P2000 共通）
const P9_TWALK: u8 = 110;
const P9_RWALK: u8 = 111;
/// ファイルデータを読む（9P2000 共通）
const P9_TREAD: u8 = 116;
#[allow(dead_code)]
const P9_RREAD: u8 = 117;
/// fid を解放（9P2000 共通）
const P9_TCLUNK: u8 = 120;
#[allow(dead_code)]
const P9_RCLUNK: u8 = 121;

/// Tversion メッセージで使用するタグ値（規約により NOTAG = 0xFFFF）
const P9_NOTAG: u16 = 0xFFFF;
/// Tattach で認証不要の場合に使う fid 値（NOFID = 0xFFFFFFFF）
const P9_NOFID: u32 = 0xFFFFFFFF;

/// Tgetattr で要求する属性マスク。
/// P9_STATS_BASIC = mode, nlink, uid, gid, rdev, atime, mtime, ctime, ino, size, blocks
const P9_GETATTR_BASIC: u64 = 0x000007FF;

/// 初期提案する最大メッセージサイズ（バイト）。
/// QEMU のデフォルトは通常 8168 で、この値以下にネゴシエーションされる。
const INITIAL_MSIZE: u32 = 8192;

// ============================================================
// virtio デバイスステータスフラグ
// ============================================================

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;

// ============================================================
// Virtqueue ディスクリプタのフラグ
// ============================================================

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// ============================================================
// エラー型
// ============================================================

/// 9P 操作のエラー
#[derive(Debug)]
pub enum V9pError {
    /// サーバーが返した errno（Linux errno 値）
    ServerError(u32),
    /// プロトコルエラー（予期しないレスポンスタイプ等）
    /// 文字列はデバッグ用のエラーメッセージ
    #[allow(dead_code)]
    ProtocolError(&'static str),
    /// I/O タイムアウト
    Timeout,
}

// ============================================================
// 9P データ型
// ============================================================

/// QID (Unique identification for a file)
/// 9P ではすべてのファイル/ディレクトリを QID で一意に識別する。
#[derive(Debug, Clone)]
#[allow(dead_code)] // version/path は readdir のパース結果を保持するが、VFS 層では未使用
pub struct Qid {
    /// タイプ: 0x80 = ディレクトリ, 0x00 = 通常ファイル
    pub type_: u8,
    /// バージョン: ファイルが変更されるたびにインクリメントされる
    pub version: u32,
    /// パス: inode 番号に相当するユニーク ID
    pub path: u64,
}

/// ディレクトリエントリ（readdir で取得）
#[derive(Debug)]
#[allow(dead_code)] // qid/offset は readdir のパース結果を保持するが、VFS 変換では dtype/name のみ使用
pub struct DirEntry9p {
    /// エントリの QID
    pub qid: Qid,
    /// 次回 readdir で使うオフセット（cookie）
    pub offset: u64,
    /// エントリタイプ: DT_DIR=4, DT_REG=8, DT_LNK=10, etc.
    pub dtype: u8,
    /// ファイル名
    pub name: String,
}

/// ファイル属性（getattr で取得）
#[derive(Debug)]
pub struct Stat9p {
    /// ファイルモード（permissions + type bits）
    /// S_IFDIR = 0o040000, S_IFREG = 0o100000
    pub mode: u32,
    /// ファイルサイズ（バイト）
    pub size: u64,
}

// ============================================================
// ドライバ構造体
// ============================================================

/// virtio-9p ドライバ
pub struct Virtio9p {
    /// BAR0 から取得した I/O ポートのベースアドレス
    io_base: u16,
    /// Virtqueue のサイズ（エントリ数）
    queue_size: u16,
    /// Virtqueue メモリの先頭ポインタ（ページアラインで確保済み）
    vq_ptr: *mut u8,
    /// 次に使うディスクリプタのインデックス
    next_desc: u16,
    /// 前回 Used Ring から読んだ idx
    last_used_idx: u16,
    /// ネゴシエーション済み最大メッセージサイズ
    msize: u32,
    /// リクエストタグカウンタ（1 から始める。0 は予約的に避ける）
    next_tag: u16,
    /// fid カウンタ（1 から始める。0 は root_fid として予約）
    next_fid: u32,
    /// attach で取得したルート fid
    root_fid: u32,
    /// デバイス config から読んだマウントタグ
    mount_tag: String,
    /// 送信バッファ（msize 分確保）
    tx_buf: Vec<u8>,
    /// 受信バッファ（msize 分確保）
    rx_buf: Vec<u8>,
}

// Virtio9p は raw pointer (vq_ptr) を含むが、Mutex で保護されるため Send/Sync は安全
unsafe impl Send for Virtio9p {}
unsafe impl Sync for Virtio9p {}

// ============================================================
// 初期化
// ============================================================

/// virtio-9p ドライバを初期化する。
/// PCI バスから virtio-9p デバイスを探して初期化し、9P バージョンネゴシエーションと
/// ルートアタッチまで行う。
pub fn init() {
    let dev = match pci::find_virtio_9p() {
        Some(d) => d,
        None => {
            serial_println!("virtio-9p device not found");
            return;
        }
    };

    let mut driver = match Virtio9p::from_pci_device(dev) {
        Some(d) => d,
        None => {
            serial_println!("virtio-9p initialization failed");
            return;
        }
    };

    // 9P プロトコルのバージョンネゴシエーション
    if let Err(e) = driver.do_version() {
        serial_println!("9P version negotiation failed: {:?}", e);
        return;
    }

    // ルートファイルツリーにアタッチ
    if let Err(e) = driver.do_attach() {
        serial_println!("9P attach failed: {:?}", e);
        return;
    }

    serial_println!(
        "virtio-9p: ready (mount_tag={}, msize={})",
        driver.mount_tag,
        driver.msize
    );

    *VIRTIO_9P.lock() = Some(driver);
}

/// virtio-9p デバイスが利用可能かどうか返す。
pub fn is_available() -> bool {
    VIRTIO_9P.lock().is_some()
}

// ============================================================
// バイナリエンコード/デコードヘルパー
// ============================================================
// 9P プロトコルはすべてリトルエンディアンで通信する。
// 各関数は書き込み/読み取り後の新しいオフセットを返す。

/// バッファに u8 を書き込む
fn put_u8(buf: &mut [u8], off: usize, val: u8) -> usize {
    buf[off] = val;
    off + 1
}

/// バッファに u16 (LE) を書き込む
fn put_u16(buf: &mut [u8], off: usize, val: u16) -> usize {
    buf[off] = val as u8;
    buf[off + 1] = (val >> 8) as u8;
    off + 2
}

/// バッファに u32 (LE) を書き込む
fn put_u32(buf: &mut [u8], off: usize, val: u32) -> usize {
    buf[off] = val as u8;
    buf[off + 1] = (val >> 8) as u8;
    buf[off + 2] = (val >> 16) as u8;
    buf[off + 3] = (val >> 24) as u8;
    off + 4
}

/// バッファに u64 (LE) を書き込む
fn put_u64(buf: &mut [u8], off: usize, val: u64) -> usize {
    for i in 0..8 {
        buf[off + i] = (val >> (i * 8)) as u8;
    }
    off + 8
}

/// バッファに 9P 文字列を書き込む（len[2] + data[len]）
fn put_str(buf: &mut [u8], off: usize, s: &str) -> usize {
    let off = put_u16(buf, off, s.len() as u16);
    buf[off..off + s.len()].copy_from_slice(s.as_bytes());
    off + s.len()
}

/// バッファから u8 を読む
fn get_u8(buf: &[u8], off: usize) -> (u8, usize) {
    (buf[off], off + 1)
}

/// バッファから u16 (LE) を読む
fn get_u16(buf: &[u8], off: usize) -> (u16, usize) {
    let val = (buf[off] as u16) | ((buf[off + 1] as u16) << 8);
    (val, off + 2)
}

/// バッファから u32 (LE) を読む
fn get_u32(buf: &[u8], off: usize) -> (u32, usize) {
    let val = (buf[off] as u32)
        | ((buf[off + 1] as u32) << 8)
        | ((buf[off + 2] as u32) << 16)
        | ((buf[off + 3] as u32) << 24);
    (val, off + 4)
}

/// バッファから u64 (LE) を読む
fn get_u64(buf: &[u8], off: usize) -> (u64, usize) {
    let mut val: u64 = 0;
    for i in 0..8 {
        val |= (buf[off + i] as u64) << (i * 8);
    }
    (val, off + 8)
}

/// バッファから 9P 文字列を読む（len[2] + data[len]）
fn get_str(buf: &[u8], off: usize) -> (String, usize) {
    let (len, off) = get_u16(buf, off);
    let s = core::str::from_utf8(&buf[off..off + len as usize])
        .unwrap_or("<invalid utf8>")
        .into();
    (s, off + len as usize)
}

/// バッファから QID (13 bytes) を読む
fn get_qid(buf: &[u8], off: usize) -> (Qid, usize) {
    let (type_, off) = get_u8(buf, off);
    let (version, off) = get_u32(buf, off);
    let (path, off) = get_u64(buf, off);
    (Qid { type_, version, path }, off)
}

// ============================================================
// Virtio9p 実装
// ============================================================

impl Virtio9p {
    /// PCI デバイスから virtio-9p ドライバを初期化する。
    /// virtio legacy (v0.9.5) の初期化シーケンスに従う。
    fn from_pci_device(dev: pci::PciDevice) -> Option<Self> {
        serial_println!(
            "virtio-9p found at PCI {:02x}:{:02x}.{}",
            dev.bus,
            dev.device,
            dev.function
        );

        // BAR0 を読み取って I/O ポートベースアドレスを取得する
        let bar0 = pci::read_bar(dev.bus, dev.device, dev.function, 0);
        if bar0 & 1 == 0 {
            serial_println!("virtio-9p BAR0 is MMIO, not I/O port — unsupported");
            return None;
        }
        let io_base = (bar0 & 0xFFFC) as u16;
        serial_println!("virtio-9p I/O base: {:#x}", io_base);

        // --- デバイス初期化シーケンス ---

        // 1. リセット
        unsafe {
            Port::<u8>::new(io_base + 0x12).write(0);
        }

        // 2. ACKNOWLEDGE
        unsafe {
            Port::<u8>::new(io_base + 0x12).write(VIRTIO_STATUS_ACKNOWLEDGE);
        }

        // 3. DRIVER
        unsafe {
            Port::<u8>::new(io_base + 0x12)
                .write(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);
        }

        // 4. Feature negotiation
        // virtio-9p の feature bit 0 = VIRTIO_9P_MOUNT_TAG（mount tag がデバイス config にある）
        let device_features = unsafe { Port::<u32>::new(io_base + 0x00).read() };
        serial_println!("virtio-9p device features: {:#010x}", device_features);
        // VIRTIO_9P_MOUNT_TAG (bit 0) を受け入れる
        let guest_features = device_features & 0x1;
        unsafe {
            Port::<u32>::new(io_base + 0x04).write(guest_features);
        }

        // 5. Queue 0 (requestq) のセットアップ
        unsafe {
            Port::<u16>::new(io_base + 0x0E).write(0);
        }
        let queue_size = unsafe { Port::<u16>::new(io_base + 0x0C).read() };
        serial_println!("virtio-9p queue size: {}", queue_size);

        if queue_size == 0 {
            serial_println!("virtio-9p queue size is 0 — no queue available");
            return None;
        }

        // Virtqueue メモリのアロケーション（ページアライン）
        let desc_size = (queue_size as usize) * 16;
        let avail_size = 4 + (queue_size as usize) * 2;
        let used_offset = align_up(desc_size + avail_size, 4096);
        let used_size = 4 + (queue_size as usize) * 8;
        let total_size = align_up(used_offset + used_size, 4096);

        let layout = Layout::from_size_align(total_size, 4096).expect("Invalid layout for virtqueue");
        let vq_ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if vq_ptr.is_null() {
            serial_println!("Failed to allocate virtqueue memory");
            return None;
        }

        let vq_phys = vq_ptr as u64;
        serial_println!("virtio-9p virtqueue at phys {:#x}", vq_phys);

        // Queue Address レジスタにページ番号を書き込む
        unsafe {
            Port::<u32>::new(io_base + 0x08).write((vq_phys / 4096) as u32);
        }

        // 6. DRIVER_OK
        unsafe {
            Port::<u8>::new(io_base + 0x12).write(
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_DRIVER_OK,
            );
        }

        // デバイス固有 config から mount_tag を読み取る。
        // virtio-9p の device config (offset 0x14 から):
        //   tag_len[2]: マウントタグ文字列の長さ
        //   tag[tag_len]: マウントタグ文字列（null 終端なし）
        let tag_len = unsafe { Port::<u16>::new(io_base + 0x14).read() } as usize;
        let mut mount_tag = String::new();
        for i in 0..tag_len {
            let b = unsafe { Port::<u8>::new(io_base + 0x16 + i as u16).read() };
            mount_tag.push(b as char);
        }
        serial_println!("virtio-9p mount_tag: \"{}\" (len={})", mount_tag, tag_len);

        let status = unsafe { Port::<u8>::new(io_base + 0x12).read() };
        serial_println!("virtio-9p status after init: {:#x}", status);

        // 送受信バッファをヒープに確保（初期サイズ = 提案 msize）
        let tx_buf = vec![0u8; INITIAL_MSIZE as usize];
        let rx_buf = vec![0u8; INITIAL_MSIZE as usize];

        Some(Virtio9p {
            io_base,
            queue_size,
            vq_ptr,
            next_desc: 0,
            last_used_idx: 0,
            msize: INITIAL_MSIZE,
            next_tag: 1,
            next_fid: 1, // 0 は root_fid として使う
            root_fid: 0,
            mount_tag,
            tx_buf,
            rx_buf,
        })
    }

    // ============================================================
    // Virtqueue 操作
    // ============================================================

    /// Virtqueue のディスクリプタテーブルにエントリを書き込む
    fn write_desc(&self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let offset = (idx as usize) * 16;
        let ptr = unsafe { self.vq_ptr.add(offset) };
        unsafe {
            (ptr as *mut u64).write_volatile(addr);
            (ptr.add(8) as *mut u32).write_volatile(len);
            (ptr.add(12) as *mut u16).write_volatile(flags);
            (ptr.add(14) as *mut u16).write_volatile(next);
        }
    }

    /// 9P メッセージを送信し、レスポンスを受信する。
    ///
    /// tx_buf[0..tx_len] を送信バッファとして、rx_buf 全体を受信バッファとして使う。
    /// virtqueue を通じてメッセージを交換し、レスポンスのバイト数を返す。
    ///
    /// エラーレスポンス (Rlerror) の場合は V9pError::ServerError を返す。
    fn transact(&mut self, tx_len: usize) -> Result<usize, V9pError> {
        let rx_len = self.rx_buf.len();

        // borrow checker 回避: self を借用する前にポインタとサイズを取得する
        let tx_ptr = self.tx_buf.as_ptr() as u64;
        let rx_ptr = self.rx_buf.as_mut_ptr() as u64;

        // 2 つのディスクリプタを使用: tx (device-readable) → rx (device-writable)
        let d0 = self.next_desc;
        let d1 = (d0 + 1) % self.queue_size;

        // desc 0: 送信バッファ（device-readable, NEXT フラグ）
        self.write_desc(d0, tx_ptr, tx_len as u32, VIRTQ_DESC_F_NEXT, d1);
        // desc 1: 受信バッファ（device-writable）
        self.write_desc(d1, rx_ptr, rx_len as u32, VIRTQ_DESC_F_WRITE, 0);

        self.next_desc = (d0 + 2) % self.queue_size;

        // Available Ring に追加
        let avail_offset = (self.queue_size as usize) * 16;
        let avail_ptr = unsafe { self.vq_ptr.add(avail_offset) };

        let avail_idx = unsafe { (avail_ptr.add(2) as *const u16).read_volatile() };
        let ring_entry_offset = 4 + ((avail_idx % self.queue_size) as usize) * 2;
        unsafe {
            (avail_ptr.add(ring_entry_offset) as *mut u16).write_volatile(d0);
        }
        fence(Ordering::SeqCst);
        unsafe {
            (avail_ptr.add(2) as *mut u16).write_volatile(avail_idx.wrapping_add(1));
        }
        fence(Ordering::SeqCst);

        // デバイスに通知 (queue 0)
        unsafe {
            Port::<u16>::new(self.io_base + 0x10).write(0);
        }

        // Used Ring をポーリングして完了を待つ
        let desc_size = (self.queue_size as usize) * 16;
        let avail_ring_size = 4 + (self.queue_size as usize) * 2;
        let used_ring_offset = align_up(desc_size + avail_ring_size, 4096);
        let used_ptr = unsafe { self.vq_ptr.add(used_ring_offset) };

        let expected_used_idx = self.last_used_idx.wrapping_add(1);
        let mut spin_count = 0u64;
        loop {
            fence(Ordering::SeqCst);
            let used_idx = unsafe { (used_ptr.add(2) as *const u16).read_volatile() };
            if used_idx == expected_used_idx {
                break;
            }
            spin_count += 1;
            if spin_count > 100_000_000 {
                return Err(V9pError::Timeout);
            }
            core::hint::spin_loop();
        }
        self.last_used_idx = expected_used_idx;
        fence(Ordering::SeqCst);

        // レスポンスのサイズを 9P ヘッダーから取得
        let (resp_size, _) = get_u32(&self.rx_buf, 0);
        let resp_size = resp_size as usize;

        // Rlerror チェック: type == 7 ならサーバーがエラーを返した
        let (resp_type, _) = get_u8(&self.rx_buf, 4);
        if resp_type == P9_RLERROR {
            // Rlerror のペイロード: errno[4]
            let (errno, _) = get_u32(&self.rx_buf, 7);
            return Err(V9pError::ServerError(errno));
        }

        Ok(resp_size)
    }

    /// リクエストのタグを生成する（毎回インクリメント）
    fn alloc_tag(&mut self) -> u16 {
        let tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1);
        if self.next_tag == P9_NOTAG {
            self.next_tag = 1; // NOTAG (0xFFFF) を避ける
        }
        tag
    }

    /// 新しい fid を割り当てる
    fn alloc_fid(&mut self) -> u32 {
        let fid = self.next_fid;
        self.next_fid += 1;
        fid
    }

    // ============================================================
    // 9P プロトコル操作
    // ============================================================

    /// Tversion / Rversion: プロトコルバージョンをネゴシエーションする。
    ///
    /// クライアントが提案する msize（最大メッセージサイズ）とプロトコルバージョンを送り、
    /// サーバーが受け入れた msize とバージョンが返ってくる。
    fn do_version(&mut self) -> Result<(), V9pError> {
        let version = "9P2000.L";
        // Tversion: size[4] type[1] tag[2] msize[4] version[s]
        let mut off = 0;
        off = put_u32(&mut self.tx_buf, off, 0); // size（後で埋める）
        off = put_u8(&mut self.tx_buf, off, P9_TVERSION);
        off = put_u16(&mut self.tx_buf, off, P9_NOTAG); // Tversion は必ず NOTAG
        off = put_u32(&mut self.tx_buf, off, INITIAL_MSIZE);
        off = put_str(&mut self.tx_buf, off, version);
        // size フィールドを埋める
        put_u32(&mut self.tx_buf, 0, off as u32);

        let resp_size = self.transact(off)?;

        // Rversion をパース: size[4] type[1] tag[2] msize[4] version[s]
        let (resp_type, _) = get_u8(&self.rx_buf, 4);
        if resp_type != P9_RVERSION {
            return Err(V9pError::ProtocolError("expected Rversion"));
        }
        let (server_msize, roff) = get_u32(&self.rx_buf, 7);
        let (server_version, _) = get_str(&self.rx_buf, roff);

        serial_println!(
            "9P: version negotiated: msize={}, version=\"{}\" (resp_size={})",
            server_msize,
            server_version,
            resp_size
        );

        if server_version != "9P2000.L" {
            return Err(V9pError::ProtocolError("server does not support 9P2000.L"));
        }

        // ネゴシエーションされた msize を保存
        self.msize = server_msize;
        // バッファサイズを msize に合わせる（縮小の場合は resize で対応）
        self.tx_buf.resize(self.msize as usize, 0);
        self.rx_buf.resize(self.msize as usize, 0);

        Ok(())
    }

    /// Tattach / Rattach: サーバーのルートファイルツリーにアタッチする。
    ///
    /// ルートディレクトリに root_fid (= 0) を割り当てる。
    /// 以後のすべてのファイル操作は、この fid から walk して行う。
    fn do_attach(&mut self) -> Result<(), V9pError> {
        let tag = self.alloc_tag();
        // Tattach: size[4] type[1] tag[2] fid[4] afid[4] uname[s] aname[s] n_uname[4]
        let mut off = 0;
        off = put_u32(&mut self.tx_buf, off, 0); // size placeholder
        off = put_u8(&mut self.tx_buf, off, P9_TATTACH);
        off = put_u16(&mut self.tx_buf, off, tag);
        off = put_u32(&mut self.tx_buf, off, self.root_fid); // fid = 0 (root)
        off = put_u32(&mut self.tx_buf, off, P9_NOFID); // afid = NOFID (no auth)
        off = put_str(&mut self.tx_buf, off, "root"); // uname
        off = put_str(&mut self.tx_buf, off, ""); // aname (default tree)
        off = put_u32(&mut self.tx_buf, off, 0); // n_uname (uid 0 = root)
        put_u32(&mut self.tx_buf, 0, off as u32);

        self.transact(off)?;

        // Rattach をパース: size[4] type[1] tag[2] qid[13]
        let (resp_type, _) = get_u8(&self.rx_buf, 4);
        if resp_type != P9_RATTACH {
            return Err(V9pError::ProtocolError("expected Rattach"));
        }
        let (qid, _) = get_qid(&self.rx_buf, 7);
        serial_println!(
            "9P: attached to root (qid type={:#x}, path={})",
            qid.type_,
            qid.path
        );

        Ok(())
    }

    /// Twalk / Rwalk: パスを辿って新しい fid を割り当てる。
    ///
    /// root_fid からパスのコンポーネントを順に辿り、最終的なファイル/ディレクトリに
    /// 新しい fid を割り当てる。9P の walk は 1 回のメッセージで最大 16 コンポーネント
    /// まで辿れる。16 を超える場合は連鎖 walk を行う。
    ///
    /// 空パス（""）の場合は nwname=0 で root_fid をクローンする。
    ///
    /// 戻り値: (新しい fid, 最後の QID)
    fn walk(&mut self, path: &str) -> Result<(u32, Qid), V9pError> {
        // パスを "/" で分割してコンポーネントに分ける
        let components: Vec<&str> = if path.is_empty() {
            Vec::new()
        } else {
            path.split('/').filter(|s| !s.is_empty()).collect()
        };

        let new_fid = self.alloc_fid();
        let mut current_fid = self.root_fid;
        let mut last_qid = Qid {
            type_: 0x80,
            version: 0,
            path: 0,
        }; // ルートは dir

        // 空パスの場合は nwname=0 で root_fid をクローンする（ルートディレクトリを参照）
        if components.is_empty() {
            let tag = self.alloc_tag();
            let mut off = 0;
            off = put_u32(&mut self.tx_buf, off, 0);
            off = put_u8(&mut self.tx_buf, off, P9_TWALK);
            off = put_u16(&mut self.tx_buf, off, tag);
            off = put_u32(&mut self.tx_buf, off, self.root_fid);
            off = put_u32(&mut self.tx_buf, off, new_fid);
            off = put_u16(&mut self.tx_buf, off, 0); // nwname=0
            put_u32(&mut self.tx_buf, 0, off as u32);

            self.transact(off)?;

            let (resp_type, _) = get_u8(&self.rx_buf, 4);
            if resp_type != P9_RWALK {
                return Err(V9pError::ProtocolError("expected Rwalk"));
            }

            return Ok((new_fid, last_qid));
        }

        // 16 コンポーネントずつ walk する（9P の制限）
        // 注意: `start < components.len()` で正しい。`<=` だと全コンポーネント処理後に
        // 余分なイテレーションが走り、既に割り当て済みの new_fid への再 walk が発生して
        // サーバーエラーになる。
        let mut start = 0;
        while start < components.len() {
            let end = core::cmp::min(start + 16, components.len());
            let chunk = &components[start..end];

            let is_last_chunk = end >= components.len();
            // 最後のチャンクなら new_fid を使う。途中なら中間 fid を使う。
            let target_fid = if is_last_chunk {
                new_fid
            } else {
                self.alloc_fid()
            };

            let tag = self.alloc_tag();
            // Twalk: size[4] type[1] tag[2] fid[4] newfid[4] nwname[2] wname[s]...
            let mut off = 0;
            off = put_u32(&mut self.tx_buf, off, 0); // size placeholder
            off = put_u8(&mut self.tx_buf, off, P9_TWALK);
            off = put_u16(&mut self.tx_buf, off, tag);
            off = put_u32(&mut self.tx_buf, off, current_fid);
            off = put_u32(&mut self.tx_buf, off, target_fid);
            off = put_u16(&mut self.tx_buf, off, chunk.len() as u16);
            for &comp in chunk {
                off = put_str(&mut self.tx_buf, off, comp);
            }
            put_u32(&mut self.tx_buf, 0, off as u32);

            self.transact(off)?;

            // Rwalk をパース: size[4] type[1] tag[2] nwqid[2] qid[13]...
            let (resp_type, _) = get_u8(&self.rx_buf, 4);
            if resp_type != P9_RWALK {
                return Err(V9pError::ProtocolError("expected Rwalk"));
            }
            let (nwqid, mut roff) = get_u16(&self.rx_buf, 7);

            // walk 成功: nwqid == nwname ならすべてのコンポーネントが辿れた
            if (nwqid as usize) != chunk.len() {
                // 部分的な walk — ファイルが見つからなかった
                // target_fid を clunk する必要はない（walk が失敗した fid はサーバー側で割り当てられていない）
                return Err(V9pError::ServerError(2)); // ENOENT
            }

            // 最後の QID を取得
            for _ in 0..nwqid {
                let (qid, new_roff) = get_qid(&self.rx_buf, roff);
                last_qid = qid;
                roff = new_roff;
            }

            // 中間 fid の場合は、次のチャンクの出発点として使い、後で clunk しない
            // （walk が成功した fid は target_fid として割り当て済み）
            if !is_last_chunk {
                // 中間 fid から次のチャンクを walk する
                // 前の current_fid が root_fid でなければ clunk する
                if current_fid != self.root_fid {
                    let _ = self.clunk(current_fid);
                }
                current_fid = target_fid;
            }

            start = end;
        }

        Ok((new_fid, last_qid))
    }

    /// Tlopen / Rlopen: fid を開く。
    ///
    /// walk で取得した fid に対して open を行い、読み取り可能にする。
    /// flags=0 (O_RDONLY) で開く。
    ///
    /// 戻り値: iounit（1回の read で推奨される最大バイト数、0 ならデフォルト使用）
    fn lopen(&mut self, fid: u32) -> Result<u32, V9pError> {
        let tag = self.alloc_tag();
        // Tlopen: size[4] type[1] tag[2] fid[4] flags[4]
        let mut off = 0;
        off = put_u32(&mut self.tx_buf, off, 0);
        off = put_u8(&mut self.tx_buf, off, P9_TLOPEN);
        off = put_u16(&mut self.tx_buf, off, tag);
        off = put_u32(&mut self.tx_buf, off, fid);
        off = put_u32(&mut self.tx_buf, off, 0); // flags = O_RDONLY
        put_u32(&mut self.tx_buf, 0, off as u32);

        self.transact(off)?;

        // Rlopen: size[4] type[1] tag[2] qid[13] iounit[4]
        let (resp_type, _) = get_u8(&self.rx_buf, 4);
        if resp_type != P9_RLOPEN {
            return Err(V9pError::ProtocolError("expected Rlopen"));
        }
        let (_qid, roff) = get_qid(&self.rx_buf, 7);
        let (iounit, _) = get_u32(&self.rx_buf, roff);

        Ok(iounit)
    }

    /// Tread / Rread: 開いた fid からデータを読む。
    ///
    /// offset バイト目から最大 buf.len() バイトを読み取る。
    /// 実際に読み取ったバイト数を返す。0 ならファイル終端 (EOF)。
    fn read_data(&mut self, fid: u32, offset: u64, buf: &mut [u8]) -> Result<usize, V9pError> {
        // 1 回の read で要求できるサイズは msize - 11 (Rread のヘッダーオーバーヘッド)
        // Rread: size[4] type[1] tag[2] count[4] data[count]
        let max_count = (self.msize as usize).saturating_sub(11);
        let count = core::cmp::min(buf.len(), max_count) as u32;

        let tag = self.alloc_tag();
        // Tread: size[4] type[1] tag[2] fid[4] offset[8] count[4]
        let mut off = 0;
        off = put_u32(&mut self.tx_buf, off, 0);
        off = put_u8(&mut self.tx_buf, off, P9_TREAD);
        off = put_u16(&mut self.tx_buf, off, tag);
        off = put_u32(&mut self.tx_buf, off, fid);
        off = put_u64(&mut self.tx_buf, off, offset);
        off = put_u32(&mut self.tx_buf, off, count);
        put_u32(&mut self.tx_buf, 0, off as u32);

        self.transact(off)?;

        // Rread: size[4] type[1] tag[2] count[4] data[count]
        let (resp_type, _) = get_u8(&self.rx_buf, 4);
        if resp_type != P9_RREAD {
            return Err(V9pError::ProtocolError("expected Rread"));
        }
        let (data_count, _) = get_u32(&self.rx_buf, 7);
        let data_count = data_count as usize;

        // レスポンスからデータをコピー
        if data_count > 0 {
            buf[..data_count].copy_from_slice(&self.rx_buf[11..11 + data_count]);
        }

        Ok(data_count)
    }

    /// Treaddir / Rreaddir: ディレクトリエントリを読む。
    ///
    /// 開いた fid のディレクトリからエントリを読む。
    /// offset は前回の readdir で最後のエントリが返した offset 値を使う（初回は 0）。
    /// 戻り値: (エントリのベクタ, 次回の offset)。エントリが空なら EOF。
    fn readdir_once(
        &mut self,
        fid: u32,
        offset: u64,
    ) -> Result<(Vec<DirEntry9p>, u64), V9pError> {
        let max_count = (self.msize as usize).saturating_sub(11);

        let tag = self.alloc_tag();
        // Treaddir: size[4] type[1] tag[2] fid[4] offset[8] count[4]
        let mut off = 0;
        off = put_u32(&mut self.tx_buf, off, 0);
        off = put_u8(&mut self.tx_buf, off, P9_TREADDIR);
        off = put_u16(&mut self.tx_buf, off, tag);
        off = put_u32(&mut self.tx_buf, off, fid);
        off = put_u64(&mut self.tx_buf, off, offset);
        off = put_u32(&mut self.tx_buf, off, max_count as u32);
        put_u32(&mut self.tx_buf, 0, off as u32);

        self.transact(off)?;

        // Rreaddir: size[4] type[1] tag[2] count[4] data[count]
        let (resp_type, _) = get_u8(&self.rx_buf, 4);
        if resp_type != P9_RREADDIR {
            return Err(V9pError::ProtocolError("expected Rreaddir"));
        }
        let (data_count, _) = get_u32(&self.rx_buf, 7);
        let data_count = data_count as usize;

        if data_count == 0 {
            return Ok((Vec::new(), 0));
        }

        // ディレクトリエントリをパース
        // 各エントリ: qid[13] offset[8] type[1] name[s]
        let mut entries = Vec::new();
        let mut roff = 11; // Rreaddir ヘッダーの後
        let data_end = 11 + data_count;
        let mut last_offset = 0u64;

        while roff < data_end {
            let (qid, new_roff) = get_qid(&self.rx_buf, roff);
            roff = new_roff;
            let (entry_offset, new_roff) = get_u64(&self.rx_buf, roff);
            roff = new_roff;
            let (dtype, new_roff) = get_u8(&self.rx_buf, roff);
            roff = new_roff;
            let (name, new_roff) = get_str(&self.rx_buf, roff);
            roff = new_roff;

            last_offset = entry_offset;

            // "." と ".." はスキップ（VFS のエントリとしては不要）
            if name == "." || name == ".." {
                continue;
            }

            entries.push(DirEntry9p {
                qid,
                offset: entry_offset,
                dtype,
                name,
            });
        }

        Ok((entries, last_offset))
    }

    /// Tgetattr / Rgetattr: ファイル属性を取得する。
    ///
    /// fid に対応するファイル/ディレクトリの属性（モード、サイズ等）を取得する。
    /// fid は walk 済みで未 open でも OK。
    fn getattr(&mut self, fid: u32) -> Result<Stat9p, V9pError> {
        let tag = self.alloc_tag();
        // Tgetattr: size[4] type[1] tag[2] fid[4] request_mask[8]
        let mut off = 0;
        off = put_u32(&mut self.tx_buf, off, 0);
        off = put_u8(&mut self.tx_buf, off, P9_TGETATTR);
        off = put_u16(&mut self.tx_buf, off, tag);
        off = put_u32(&mut self.tx_buf, off, fid);
        off = put_u64(&mut self.tx_buf, off, P9_GETATTR_BASIC);
        put_u32(&mut self.tx_buf, 0, off as u32);

        self.transact(off)?;

        // Rgetattr: size[4] type[1] tag[2]
        //   valid[8] qid[13] mode[4] uid[4] gid[4] nlink[8] rdev[8] size[8] ...
        let (resp_type, _) = get_u8(&self.rx_buf, 4);
        if resp_type != P9_RGETATTR {
            return Err(V9pError::ProtocolError("expected Rgetattr"));
        }

        // 順番にフィールドをパース
        let mut roff = 7; // ヘッダー（size + type + tag）の後
        let (_valid, new_roff) = get_u64(&self.rx_buf, roff);
        roff = new_roff;
        let (_qid, new_roff) = get_qid(&self.rx_buf, roff);
        roff = new_roff;
        let (mode, new_roff) = get_u32(&self.rx_buf, roff);
        roff = new_roff;
        let (_uid, new_roff) = get_u32(&self.rx_buf, roff);
        roff = new_roff;
        let (_gid, new_roff) = get_u32(&self.rx_buf, roff);
        roff = new_roff;
        let (_nlink, new_roff) = get_u64(&self.rx_buf, roff);
        roff = new_roff;
        let (_rdev, new_roff) = get_u64(&self.rx_buf, roff);
        roff = new_roff;
        let (size, _) = get_u64(&self.rx_buf, roff);

        Ok(Stat9p { mode, size })
    }

    /// Tclunk / Rclunk: fid を解放する。
    ///
    /// ファイルディスクリプタの close に相当。使い終わった fid は必ず clunk する。
    fn clunk(&mut self, fid: u32) -> Result<(), V9pError> {
        let tag = self.alloc_tag();
        // Tclunk: size[4] type[1] tag[2] fid[4]
        let mut off = 0;
        off = put_u32(&mut self.tx_buf, off, 0);
        off = put_u8(&mut self.tx_buf, off, P9_TCLUNK);
        off = put_u16(&mut self.tx_buf, off, tag);
        off = put_u32(&mut self.tx_buf, off, fid);
        put_u32(&mut self.tx_buf, 0, off as u32);

        self.transact(off)?;
        // Rclunk は空ペイロード — 正常ならここに到達

        Ok(())
    }

    // ============================================================
    // 高レベル API（VFS から呼ばれる）
    // ============================================================

    /// ディレクトリ内のエントリ一覧を取得する。
    ///
    /// パスが空文字列の場合はルートディレクトリ。
    /// walk → lopen → readdir ループ → clunk の手順で行う。
    pub fn list_directory(&mut self, path: &str) -> Result<Vec<DirEntry9p>, V9pError> {
        // walk でディレクトリの fid を取得（空パスなら root をクローン）
        let (dir_fid, _qid) = self.walk(path)?;

        // fid を開く
        if let Err(e) = self.lopen(dir_fid) {
            let _ = self.clunk(dir_fid);
            return Err(e);
        }

        // readdir ループですべてのエントリを取得
        let mut all_entries = Vec::new();
        let mut offset: u64 = 0;
        loop {
            let (entries, last_offset) = match self.readdir_once(dir_fid, offset) {
                Ok(result) => result,
                Err(e) => {
                    let _ = self.clunk(dir_fid);
                    return Err(e);
                }
            };
            if entries.is_empty() {
                break;
            }
            offset = last_offset;
            all_entries.extend(entries);
        }

        // fid を解放
        let _ = self.clunk(dir_fid);

        Ok(all_entries)
    }

    /// ファイルの全内容を読み取る。
    ///
    /// walk → getattr(size) → lopen → read ループ → clunk の手順で行う。
    /// ファイル全体をメモリに読み込んで返す。
    pub fn read_file_data(&mut self, path: &str) -> Result<(Vec<u8>, Stat9p), V9pError> {
        // walk でファイルの fid を取得
        let (file_fid, _qid) = self.walk(path)?;

        // getattr でファイルサイズを取得
        let stat = match self.getattr(file_fid) {
            Ok(s) => s,
            Err(e) => {
                let _ = self.clunk(file_fid);
                return Err(e);
            }
        };

        // fid を開く
        if let Err(e) = self.lopen(file_fid) {
            let _ = self.clunk(file_fid);
            return Err(e);
        }

        // read ループでファイル全体を読む
        let file_size = stat.size as usize;
        let mut data = Vec::with_capacity(file_size);
        let mut file_offset: u64 = 0;
        let read_chunk_size = (self.msize as usize).saturating_sub(11);
        let mut chunk_buf = vec![0u8; read_chunk_size];

        loop {
            let n = match self.read_data(file_fid, file_offset, &mut chunk_buf) {
                Ok(n) => n,
                Err(e) => {
                    let _ = self.clunk(file_fid);
                    return Err(e);
                }
            };
            if n == 0 {
                break;
            }
            data.extend_from_slice(&chunk_buf[..n]);
            file_offset += n as u64;
        }

        // fid を解放
        let _ = self.clunk(file_fid);

        Ok((data, stat))
    }
}

// ============================================================
// VFS 統合
// ============================================================

/// 9P ファイルシステム（VFS FileSystem トレイト実装）
///
/// /9p にマウントされ、ホスト共有ディレクトリへの読み取り専用アクセスを提供する。
pub struct V9pFs;

impl V9pFs {
    pub fn new() -> Self {
        V9pFs
    }
}

impl FileSystem for V9pFs {
    fn name(&self) -> &str {
        "9p"
    }

    fn open(&self, path: &str) -> Result<Box<dyn VfsNode>, VfsError> {
        let mut drv = VIRTIO_9P.lock();
        let drv = drv.as_mut().ok_or(VfsError::IoError)?;

        let (data, stat) = drv.read_file_data(path).map_err(v9p_to_vfs)?;

        // ディレクトリを open しようとした場合はエラー
        if stat.mode & 0o040000 != 0 {
            return Err(VfsError::NotAFile);
        }

        Ok(Box::new(V9pFile {
            data,
            size: stat.size as usize,
        }))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<VfsDirEntry>, VfsError> {
        let mut drv = VIRTIO_9P.lock();
        let drv = drv.as_mut().ok_or(VfsError::IoError)?;

        let entries = drv.list_directory(path).map_err(v9p_to_vfs)?;

        // DirEntry9p → VfsDirEntry に変換
        let vfs_entries = entries
            .into_iter()
            .map(|e| {
                let kind = if e.dtype == 4 {
                    // DT_DIR = 4
                    VfsNodeKind::Directory
                } else {
                    VfsNodeKind::File
                };
                VfsDirEntry {
                    name: e.name,
                    kind,
                    size: 0, // readdir ではサイズ不明（getattr が必要）
                }
            })
            .collect();

        Ok(vfs_entries)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let mut drv = VIRTIO_9P.lock();
        let drv = drv.as_mut().ok_or(VfsError::IoError)?;

        let (data, _stat) = drv.read_file_data(path).map_err(v9p_to_vfs)?;
        Ok(data)
    }
}

/// 9P ファイルノード（VFS VfsNode トレイト実装）
///
/// open 時にファイル全体をメモリに読み込み、以降はメモリ上のデータから読み取る。
/// FAT32 と同じパターン。大きなファイルには非効率だが、VFS 設計と一致しシンプル。
struct V9pFile {
    /// ファイルデータ全体
    data: Vec<u8>,
    /// ファイルサイズ
    size: usize,
}

impl VfsNode for V9pFile {
    fn kind(&self) -> VfsNodeKind {
        VfsNodeKind::File
    }

    fn size(&self) -> usize {
        self.size
    }

    fn read(&self, offset: usize, buf: &mut [u8]) -> Result<usize, VfsError> {
        if offset >= self.data.len() {
            return Ok(0); // EOF
        }
        let available = self.data.len() - offset;
        let to_read = core::cmp::min(buf.len(), available);
        buf[..to_read].copy_from_slice(&self.data[offset..offset + to_read]);
        Ok(to_read)
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, VfsError> {
        // Phase 1 は読み取り専用
        Err(VfsError::ReadOnly)
    }
}

/// V9pError → VfsError への変換ヘルパー
fn v9p_to_vfs(e: V9pError) -> VfsError {
    match e {
        V9pError::ServerError(errno) => {
            // Linux errno → VfsError のマッピング
            match errno {
                2 => VfsError::NotFound,     // ENOENT
                13 => VfsError::PermissionDenied, // EACCES
                20 => VfsError::NotADirectory, // ENOTDIR
                21 => VfsError::NotAFile,      // EISDIR
                _ => VfsError::IoError,
            }
        }
        V9pError::ProtocolError(_) => VfsError::IoError,
        V9pError::Timeout => VfsError::IoError,
    }
}

// ============================================================
// ユーティリティ
// ============================================================

/// 値を alignment の倍数に切り上げる
fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}
