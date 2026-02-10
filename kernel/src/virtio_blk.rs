// virtio_blk.rs — virtio-blk ドライバ (Legacy インターフェース)
//
// virtio は仮想化環境（QEMU, KVM 等）でホストとゲスト間の効率的な I/O を実現するための
// 標準インターフェース仕様。物理ハードウェアをエミュレートするよりもオーバーヘッドが小さい。
//
// virtio-blk はブロックデバイス（ディスク）用の virtio デバイス。
// QEMU の `-drive if=virtio` オプションで使われる。
//
// 今回実装するのは virtio legacy (v0.9.5) インターフェース。
// QEMU のデフォルト (-drive if=virtio) は legacy デバイス (device_id=0x1001) を使う。
//
// ## PCI Transport (Legacy)
//
// virtio デバイスは PCI デバイスとして見え、BAR0 が I/O ポート空間にマップされる。
// BAR0 のオフセットにデバイスレジスタが配置される:
//
//   Offset  Size  Name
//   0x00    4     Device Features       (ホスト→ゲスト: デバイスが対応する機能)
//   0x04    4     Guest Features        (ゲスト→ホスト: ゲストが使いたい機能)
//   0x08    4     Queue Address         (Virtqueue の物理アドレス ÷ 4096)
//   0x0C    2     Queue Size            (Virtqueue のエントリ数、デバイスが決定)
//   0x0E    2     Queue Select          (操作対象の Virtqueue 番号)
//   0x10    2     Queue Notify          (この Virtqueue に新しいリクエストがある通知)
//   0x12    1     Device Status         (初期化ステータス)
//   0x13    1     ISR Status            (割り込みステータス)
//   0x14+   ?     Device-Specific Config (デバイス固有の設定、block は capacity 等)
//
// ## Virtqueue (Split Virtqueue)
//
// virtio の I/O は Virtqueue（仮想キュー）を通じて行われる。
// Virtqueue は 3 つのリングバッファで構成される:
//
// 1. Descriptor Table: I/O バッファの物理アドレス・長さ・フラグの配列
//    - addr: バッファの物理アドレス
//    - len: バッファの長さ
//    - flags: NEXT (チェーン継続), WRITE (デバイス書き込み用)
//    - next: 次のディスクリプタのインデックス
//
// 2. Available Ring: ゲスト→デバイスへの「新しいリクエストがある」通知
//    - flags: 割り込み抑制フラグ
//    - idx: 次に書き込む位置（単調増加）
//    - ring[]: ディスクリプタチェーンの先頭インデックスの配列
//
// 3. Used Ring: デバイス→ゲストへの「リクエスト完了」通知
//    - flags: 通知抑制フラグ
//    - idx: 次に書き込む位置（単調増加）
//    - ring[]: { id: ディスクリプタインデックス, len: 書き込まれたバイト数 }
//
// ## virtio-blk リクエストフォーマット
//
// ブロック読み書きは 3 つのディスクリプタをチェーンして行う:
//   [0] VirtioBlkReqHeader { type, reserved, sector }  ← ゲスト読み取り専用
//   [1] データバッファ (512 バイト × セクタ数)            ← type に応じて読み/書き
//   [2] ステータスバイト (1 バイト)                       ← デバイス書き込み
//
// ステータス: 0 = OK, 1 = IOERR, 2 = UNSUPPORTED

use alloc::vec::Vec;
use crate::pci;
use crate::serial_println;
use core::alloc::Layout;
use core::sync::atomic::{fence, Ordering};
use spin::Mutex;
use x86_64::instructions::port::Port;

/// グローバルな virtio-blk ドライバインスタンスのリスト。
/// init() で初期化される。QEMU で複数の `-drive if=virtio` を指定すると
/// 複数のデバイスがここに格納される。
/// インデックス 0 が最初に発見されたデバイス（通常は disk.img）。
pub static VIRTIO_BLKS: Mutex<Vec<VirtioBlk>> = Mutex::new(Vec::new());

/// virtio-blk ドライバを初期化する。
/// PCI バスから全ての virtio-blk デバイスを探して初期化する。
pub fn init() {
    let pci_devices = pci::find_all_virtio_blk();
    let mut drivers = Vec::new();
    for dev in pci_devices {
        if let Some(driver) = VirtioBlk::from_pci_device(dev) {
            drivers.push(driver);
        }
    }
    if drivers.is_empty() {
        serial_println!("virtio-blk device not found or initialization failed");
    } else {
        serial_println!("virtio-blk: {} device(s) initialized", drivers.len());
    }
    *VIRTIO_BLKS.lock() = drivers;
}

/// 検出された virtio-blk デバイスの数を返す。
/// Step 2（ホストディレクトリの VFS マウント）で使用予定。
#[allow(dead_code)]
pub fn device_count() -> usize {
    VIRTIO_BLKS.lock().len()
}

// ============================================================
// virtio デバイスステータスフラグ
// ============================================================

/// ゲスト OS がデバイスを認識した
const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
/// ゲスト OS がデバイスのドライバを持っている
const VIRTIO_STATUS_DRIVER: u8 = 2;
/// ドライバの初期化が完了し、デバイスを使用可能
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
/// 何か致命的なエラーが発生した
const _VIRTIO_STATUS_FAILED: u8 = 128;

// ============================================================
// Virtqueue ディスクリプタのフラグ
// ============================================================

/// このディスクリプタには次のディスクリプタがチェーンされている
const VIRTQ_DESC_F_NEXT: u16 = 1;
/// このバッファはデバイスが書き込む用（読み取りではなく書き込み先）
const VIRTQ_DESC_F_WRITE: u16 = 2;

// ============================================================
// virtio-blk リクエストタイプ
// ============================================================

/// セクタの読み取り
const VIRTIO_BLK_T_IN: u32 = 0;
/// セクタの書き込み
const VIRTIO_BLK_T_OUT: u32 = 1;

// ============================================================
// virtio-blk リクエストステータス
// ============================================================

const VIRTIO_BLK_S_OK: u8 = 0;
const _VIRTIO_BLK_S_IOERR: u8 = 1;

// ============================================================
// データ構造体
// ============================================================

/// virtio-blk のリクエストヘッダー。
/// 各ブロック I/O リクエストの先頭に置く。
#[repr(C)]
struct VirtioBlkReqHeader {
    /// リクエストタイプ (VIRTIO_BLK_T_IN = 読み取り, VIRTIO_BLK_T_OUT = 書き込み)
    request_type: u32,
    /// 予約フィールド（常に 0）
    reserved: u32,
    /// 読み書き対象のセクタ番号（0始まり、1セクタ = 512 バイト）
    sector: u64,
}

/// virtio-blk ドライバのメイン構造体。
/// デバイスの I/O ポートベースアドレスと Virtqueue を管理する。
pub struct VirtioBlk {
    /// BAR0 から取得した I/O ポートのベースアドレス
    io_base: u16,
    /// Virtqueue のサイズ（エントリ数、デバイスが決定）
    queue_size: u16,
    /// Virtqueue メモリの先頭ポインタ（ページアラインで確保済み）
    /// UEFI 環境ではアイデンティティマッピングなので仮想アドレス = 物理アドレス
    vq_ptr: *mut u8,
    /// 次に使うディスクリプタのインデックス
    next_desc: u16,
    /// 前回 Used Ring から読んだ idx（新しい完了を検出するために使う）
    last_used_idx: u16,
    /// デバイスのブロック数（容量）
    capacity: u64,
}

// VirtioBlk は raw pointer を含むが、Mutex で保護されるため Send/Sync は安全
unsafe impl Send for VirtioBlk {}
unsafe impl Sync for VirtioBlk {}

impl VirtioBlk {
    /// 指定された PCI デバイスから virtio-blk ドライバを初期化する。
    ///
    /// 初期化手順（virtio legacy specification に従う）:
    ///   1. デバイスリセット（Status = 0）
    ///   2. ACKNOWLEDGE ステータスをセット
    ///   3. DRIVER ステータスをセット
    ///   4. Feature negotiation（今回は全ビット 0 = 最小構成）
    ///   5. Virtqueue のセットアップ
    ///   6. DRIVER_OK ステータスをセット
    pub fn from_pci_device(dev: pci::PciDevice) -> Option<Self> {
        serial_println!(
            "virtio-blk found at PCI {:02x}:{:02x}.{}",
            dev.bus, dev.device, dev.function
        );

        // BAR0 を読み取って I/O ポートベースアドレスを取得する。
        // virtio legacy デバイスの BAR0 は I/O ポートマップド（bit 0 = 1）。
        let bar0 = pci::read_bar(dev.bus, dev.device, dev.function, 0);
        // BAR の bit 0 が 1 = I/O ポートマップド。ベースアドレスは bit [31:2]。
        if bar0 & 1 == 0 {
            serial_println!("virtio-blk BAR0 is MMIO, not I/O port — unsupported");
            return None;
        }
        let io_base = (bar0 & 0xFFFC) as u16;
        serial_println!("virtio-blk I/O base: {:#x}", io_base);

        // --- デバイス初期化シーケンス ---

        // 1. デバイスリセット: Status レジスタに 0 を書き込む
        unsafe {
            Port::<u8>::new(io_base + 0x12).write(0);
        }

        // 2. ACKNOWLEDGE: デバイスの存在を認識した
        unsafe {
            Port::<u8>::new(io_base + 0x12).write(VIRTIO_STATUS_ACKNOWLEDGE);
        }

        // 3. DRIVER: ドライバが対応できる
        unsafe {
            Port::<u8>::new(io_base + 0x12)
                .write(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);
        }

        // 4. Feature negotiation
        // デバイスの機能ビットを読む（今回は参考程度、何も使わない）
        let _device_features = unsafe { Port::<u32>::new(io_base + 0x00).read() };
        serial_println!("virtio-blk device features: {:#010x}", _device_features);
        // ゲストの機能ビットを書く（最小構成 = 0）
        unsafe {
            Port::<u32>::new(io_base + 0x04).write(0);
        }

        // 5. Virtqueue 0 のセットアップ
        // Queue Select = 0（virtio-blk は requestq が queue 0）
        unsafe {
            Port::<u16>::new(io_base + 0x0E).write(0);
        }

        // Queue Size を読む（デバイスが決定した固定値、通常 256 や 128）
        let queue_size = unsafe { Port::<u16>::new(io_base + 0x0C).read() };
        serial_println!("virtio-blk queue size: {}", queue_size);

        if queue_size == 0 {
            serial_println!("virtio-blk queue size is 0 — no queue available");
            return None;
        }

        // Virtqueue のメモリレイアウト（virtio legacy の規約）:
        //
        // Descriptor Table: queue_size × 16 バイト
        // Available Ring:   4 + queue_size × 2 バイト (header 4 + ring entries)
        // --- ここまでがページアラインの前半 ---
        // Used Ring:        4 + queue_size × 8 バイト (header 4 + used entries)
        //
        // Available Ring の終端をページサイズ (4096) にアライン → Used Ring の開始位置
        let desc_size = (queue_size as usize) * 16;
        let avail_size = 4 + (queue_size as usize) * 2;
        // Available Ring の終端をページアラインして Used Ring の開始位置を決定
        let used_offset = align_up(desc_size + avail_size, 4096);
        let used_size = 4 + (queue_size as usize) * 8;
        let total_size = used_offset + used_size;
        // さらにページアラインして全体サイズを確定
        let total_size_aligned = align_up(total_size, 4096);

        // Virtqueue 用のメモリをページアラインで確保する（ゼロ初期化済み）。
        // legacy virtio はキューのベースアドレスがページアライン (4096) である必要がある。
        // Queue Address レジスタは「物理アドレス ÷ 4096」を受け取るため、
        // 4096 境界に揃っていないと下位ビットが切り捨てられて正しいアドレスにならない。
        //
        // alloc_zeroed + Layout(align=4096) でページアラインのメモリを確保する。
        // UEFI 環境ではアイデンティティマッピングなので仮想アドレス = 物理アドレス。
        let layout = Layout::from_size_align(total_size_aligned, 4096)
            .expect("Invalid layout for virtqueue");
        let vq_ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if vq_ptr.is_null() {
            serial_println!("Failed to allocate page-aligned memory for virtqueue");
            return None;
        }
        let vq_phys = vq_ptr as u64;
        serial_println!(
            "virtio-blk virtqueue at phys {:#x}, size {} bytes (page-aligned: {})",
            vq_phys, total_size_aligned, vq_phys % 4096 == 0
        );

        // Queue Address レジスタにページ番号（物理アドレス ÷ 4096）を書き込む
        // これでデバイスがゲストの Virtqueue メモリにアクセスできるようになる
        unsafe {
            Port::<u32>::new(io_base + 0x08).write((vq_phys / 4096) as u32);
        }

        // 6. DRIVER_OK: 初期化完了、デバイス使用可能
        unsafe {
            Port::<u8>::new(io_base + 0x12).write(
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_DRIVER_OK,
            );
        }

        // デバイス固有設定: capacity (セクタ数) を読む
        // virtio-blk の device config は offset 0x14 から始まる。
        // capacity は 64 ビット値で offset 0x14 (下位 32 ビット) + 0x18 (上位 32 ビット)。
        let capacity_lo = unsafe { Port::<u32>::new(io_base + 0x14).read() } as u64;
        let capacity_hi = unsafe { Port::<u32>::new(io_base + 0x18).read() } as u64;
        let capacity = capacity_lo | (capacity_hi << 32);
        serial_println!("virtio-blk capacity: {} sectors ({} MiB)", capacity, capacity * 512 / 1024 / 1024);

        let status = unsafe { Port::<u8>::new(io_base + 0x12).read() };
        serial_println!("virtio-blk status after init: {:#x}", status);

        Some(VirtioBlk {
            io_base,
            queue_size,
            vq_ptr,
            next_desc: 0,
            last_used_idx: 0,
            capacity,
        })
    }

    /// 指定セクタからデータを読み取る。
    ///
    /// sector: 読み取り開始セクタ番号（0始まり）
    /// buf: 読み取り先バッファ（512 バイトの倍数であること）
    ///
    /// virtio-blk のリクエスト手順:
    ///   1. リクエストヘッダー + データバッファ + ステータスバイトの 3 つのディスクリプタをセット
    ///   2. Available Ring に追加してデバイスに通知
    ///   3. Used Ring をポーリングして完了を待つ
    pub fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        if sector >= self.capacity {
            return Err("sector out of range");
        }
        if buf.len() < 512 || buf.len() % 512 != 0 {
            return Err("buffer must be multiple of 512 bytes");
        }

        let sector_count = buf.len() / 512;

        // リクエストヘッダーをスタック上に作成
        // （UEFI 環境ではアイデンティティマッピングなのでスタックの仮想アドレス = 物理アドレス）
        let req_header = VirtioBlkReqHeader {
            request_type: VIRTIO_BLK_T_IN,
            reserved: 0,
            sector,
        };
        // ステータスバイト（デバイスが結果を書き込む）
        let mut status_byte: u8 = 0xFF; // 初期値は無効値

        // --- ディスクリプタチェーンの構築 ---
        // 3 つのディスクリプタを使う: header → data → status
        // 各インデックスは queue_size でラップする（境界を超えないように）
        let desc_base = self.next_desc;
        let d0 = desc_base;
        let d1 = (desc_base + 1) % self.queue_size;
        let d2 = (desc_base + 2) % self.queue_size;

        // ディスクリプタ 0: リクエストヘッダー（デバイスが読む = ゲスト読み取り専用）
        self.write_desc(
            d0,
            &req_header as *const VirtioBlkReqHeader as u64,
            core::mem::size_of::<VirtioBlkReqHeader>() as u32,
            VIRTQ_DESC_F_NEXT,
            d1,
        );

        // ディスクリプタ 1: データバッファ（デバイスが書き込む = WRITE フラグ）
        self.write_desc(
            d1,
            buf.as_ptr() as u64,
            (sector_count * 512) as u32,
            VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE,
            d2,
        );

        // ディスクリプタ 2: ステータスバイト（デバイスが書き込む = WRITE フラグ、チェーン終端）
        self.write_desc(
            d2,
            &mut status_byte as *mut u8 as u64,
            1,
            VIRTQ_DESC_F_WRITE,
            0,
        );

        // 次回用にディスクリプタインデックスを進める
        self.next_desc = (desc_base + 3) % self.queue_size;

        // --- Available Ring に追加 ---
        // Available Ring のレイアウト:
        //   offset 0: flags (u16)
        //   offset 2: idx (u16)
        //   offset 4: ring[0], ring[1], ... (各 u16)
        let avail_offset = (self.queue_size as usize) * 16; // Descriptor Table の直後
        let avail_ptr = unsafe { self.vq_ptr.add(avail_offset) };

        // 現在の Available idx を読む
        let avail_idx = unsafe { (avail_ptr.add(2) as *const u16).read_volatile() };
        // ring[avail_idx % queue_size] にディスクリプタチェーンの先頭を書く
        let ring_entry_offset = 4 + ((avail_idx % self.queue_size) as usize) * 2;
        unsafe {
            (avail_ptr.add(ring_entry_offset) as *mut u16).write_volatile(d0);
        }

        // メモリバリア: ring エントリの書き込みが idx 更新より先に完了することを保証
        fence(Ordering::SeqCst);

        // idx をインクリメント（デバイスに「新しいエントリがある」と伝える）
        unsafe {
            (avail_ptr.add(2) as *mut u16).write_volatile(avail_idx.wrapping_add(1));
        }

        // メモリバリア: idx 更新が notify より先に完了することを保証
        fence(Ordering::SeqCst);

        // --- デバイスに通知 ---
        // Queue Notify レジスタに queue 番号を書く
        unsafe {
            Port::<u16>::new(self.io_base + 0x10).write(0);
        }

        // --- Used Ring をポーリングして完了を待つ ---
        // Used Ring のレイアウト:
        //   offset 0: flags (u16)
        //   offset 2: idx (u16)
        //   offset 4: VirtqUsedElem[0], VirtqUsedElem[1], ... (各 8 バイト)
        let desc_size = (self.queue_size as usize) * 16;
        let avail_size = 4 + (self.queue_size as usize) * 2;
        let used_offset = align_up(desc_size + avail_size, 4096);
        let used_ptr = unsafe { self.vq_ptr.add(used_offset) };

        // Used Ring の idx がインクリメントされるまで待つ（ビジーウェイト）
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
                return Err("virtio-blk read timeout");
            }
            core::hint::spin_loop();
        }
        self.last_used_idx = expected_used_idx;

        // ステータスバイトを確認
        fence(Ordering::SeqCst);
        if status_byte != VIRTIO_BLK_S_OK {
            return Err("virtio-blk read failed (device returned error)");
        }

        Ok(())
    }

    /// 指定セクタにデータを書き込む。
    ///
    /// sector: 書き込み先セクタ番号（0始まり）
    /// buf: 書き込むデータ（512 バイトの倍数であること）
    ///
    /// read_sector と同じ手順だが、リクエストタイプが OUT で、
    /// データバッファは「デバイスが読む」ので WRITE フラグなし。
    pub fn write_sector(&mut self, sector: u64, buf: &[u8]) -> Result<(), &'static str> {
        if sector >= self.capacity {
            return Err("sector out of range");
        }
        if buf.len() < 512 || buf.len() % 512 != 0 {
            return Err("buffer must be multiple of 512 bytes");
        }

        let sector_count = buf.len() / 512;

        // リクエストヘッダー（VIRTIO_BLK_T_OUT = 書き込みリクエスト）
        let req_header = VirtioBlkReqHeader {
            request_type: VIRTIO_BLK_T_OUT,
            reserved: 0,
            sector,
        };
        // ステータスバイト（デバイスが結果を書き込む）
        let mut status_byte: u8 = 0xFF;

        // --- ディスクリプタチェーンの構築 ---
        // 各インデックスは queue_size でラップする（境界を超えないように）
        let desc_base = self.next_desc;
        let d0 = desc_base;
        let d1 = (desc_base + 1) % self.queue_size;
        let d2 = (desc_base + 2) % self.queue_size;

        // ディスクリプタ 0: リクエストヘッダー（デバイスが読む = WRITE フラグなし）
        self.write_desc(
            d0,
            &req_header as *const VirtioBlkReqHeader as u64,
            core::mem::size_of::<VirtioBlkReqHeader>() as u32,
            VIRTQ_DESC_F_NEXT,
            d1,
        );

        // ディスクリプタ 1: データバッファ（デバイスが読む = WRITE フラグなし）
        // read_sector との違い: VIRTQ_DESC_F_WRITE を外す
        self.write_desc(
            d1,
            buf.as_ptr() as u64,
            (sector_count * 512) as u32,
            VIRTQ_DESC_F_NEXT, // WRITE フラグなし
            d2,
        );

        // ディスクリプタ 2: ステータスバイト（デバイスが書き込む = WRITE フラグ）
        self.write_desc(
            d2,
            &mut status_byte as *mut u8 as u64,
            1,
            VIRTQ_DESC_F_WRITE,
            0,
        );

        // 次回用にディスクリプタインデックスを進める
        self.next_desc = (desc_base + 3) % self.queue_size;

        // --- Available Ring に追加 ---
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

        // --- デバイスに通知 ---
        unsafe {
            Port::<u16>::new(self.io_base + 0x10).write(0);
        }

        // --- Used Ring をポーリングして完了を待つ ---
        let desc_size = (self.queue_size as usize) * 16;
        let avail_size = 4 + (self.queue_size as usize) * 2;
        let used_offset = align_up(desc_size + avail_size, 4096);
        let used_ptr = unsafe { self.vq_ptr.add(used_offset) };

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
                return Err("virtio-blk write timeout");
            }
            core::hint::spin_loop();
        }
        self.last_used_idx = expected_used_idx;

        // ステータスバイトを確認
        fence(Ordering::SeqCst);
        if status_byte != VIRTIO_BLK_S_OK {
            return Err("virtio-blk write failed (device returned error)");
        }

        Ok(())
    }

    /// Virtqueue のディスクリプタテーブルにエントリを書き込む。
    ///
    /// idx: ディスクリプタのインデックス
    /// addr: バッファの物理アドレス
    /// len: バッファのサイズ
    /// flags: VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE 等
    /// next: チェーン先のディスクリプタインデックス
    fn write_desc(&mut self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let offset = (idx as usize) * 16;
        let ptr = unsafe { self.vq_ptr.add(offset) };
        unsafe {
            // VirtqDesc のフィールドを直接書き込む（#[repr(C)] のレイアウトに従う）
            (ptr as *mut u64).write_volatile(addr);             // addr (offset +0)
            (ptr.add(8) as *mut u32).write_volatile(len);       // len  (offset +8)
            (ptr.add(12) as *mut u16).write_volatile(flags);    // flags (offset +12)
            (ptr.add(14) as *mut u16).write_volatile(next);     // next  (offset +14)
        }
    }

    /// デバイスの容量（セクタ数）を返す。
    pub fn capacity(&self) -> u64 {
        self.capacity
    }
}

/// 値を alignment の倍数に切り上げる。
/// alignment は 2 の冪であること。
fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}
