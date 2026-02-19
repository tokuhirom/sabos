// virtio_net.rs — virtio-net ドライバ (Legacy インターフェース)
//
// virtio-net は virtio フレームワークを使ったネットワークデバイス。
// QEMU の `-device virtio-net-pci` オプションで使われる。
//
// ## virtio-blk との違い
//
// - 2 つの Virtqueue を使用: receiveq (queue 0) と transmitq (queue 1)
// - パケットの前に virtio-net ヘッダーが付く
// - MAC アドレスを device config から読み取る
//
// ## virtio-net ヘッダー (Legacy)
//
// 各パケットの前に 10 バイト (または 12 バイト) のヘッダーが付く:
//   - flags (1 byte): チェックサム等のフラグ
//   - gso_type (1 byte): GSO (Generic Segmentation Offload) タイプ
//   - hdr_len (2 bytes): ヘッダー長
//   - gso_size (2 bytes): GSO セグメントサイズ
//   - csum_start (2 bytes): チェックサム計算開始位置
//   - csum_offset (2 bytes): チェックサム書き込み位置
//
// 今回は GSO やチェックサムオフロードは使わないので、すべて 0 で良い。

use crate::pci;
use crate::serial_println;
use alloc::vec;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{fence, Ordering};
use spin::Mutex;
use x86_64::instructions::port::Port;

/// グローバルな virtio-net ドライバインスタンス。
pub static VIRTIO_NET: Mutex<Option<VirtioNet>> = Mutex::new(None);

/// virtio-net ドライバを初期化する。
pub fn init() {
    let driver = VirtioNet::new();
    if driver.is_some() {
        serial_println!("virtio-net driver initialized successfully");
    } else {
        serial_println!("virtio-net device not found or initialization failed");
    }
    *VIRTIO_NET.lock() = driver;
}

// ============================================================
// virtio デバイスステータスフラグ
// ============================================================

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const _VIRTIO_STATUS_FEATURES_OK: u8 = 8;

// ============================================================
// Virtqueue ディスクリプタのフラグ
// ============================================================

const VIRTQ_DESC_F_WRITE: u16 = 2;

// ============================================================
// virtio-net feature bits
// ============================================================

/// MAC アドレスが device config に存在する
const VIRTIO_NET_F_MAC: u64 = 1 << 5;

// ============================================================
// virtio-net ヘッダー
// ============================================================

/// virtio-net パケットヘッダー (Legacy, 10 バイト)
/// 各送受信パケットの先頭に付く
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct VirtioNetHeader {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
}

impl VirtioNetHeader {
    /// 空のヘッダー (GSO/チェックサムオフロードなし)
    pub fn empty() -> Self {
        VirtioNetHeader {
            flags: 0,
            gso_type: 0,
            hdr_len: 0,
            gso_size: 0,
            csum_start: 0,
            csum_offset: 0,
        }
    }
}

// ============================================================
// VirtioNet 構造体
// ============================================================

/// 受信バッファのサイズ (MTU 1500 + Ethernet ヘッダー + 余裕)
const RX_BUFFER_SIZE: usize = 2048;
/// 受信バッファの数
const RX_BUFFER_COUNT: usize = 16;

/// virtio-net ドライバ
pub struct VirtioNet {
    /// I/O ポートベースアドレス
    io_base: u16,
    /// Virtqueue サイズ
    queue_size: u16,
    /// receiveq (queue 0) の Virtqueue メモリ
    rx_vq_ptr: *mut u8,
    /// transmitq (queue 1) の Virtqueue メモリ
    tx_vq_ptr: *mut u8,
    /// 受信バッファ群 (RX_BUFFER_COUNT 個)
    rx_buffers: *mut u8,
    /// receiveq の次のディスクリプタインデックス
    rx_next_desc: u16,
    /// receiveq の last_used_idx
    rx_last_used_idx: u16,
    /// transmitq の次のディスクリプタインデックス
    tx_next_desc: u16,
    /// transmitq の last_used_idx
    tx_last_used_idx: u16,
    /// MAC アドレス
    pub mac_address: [u8; 6],
}

unsafe impl Send for VirtioNet {}
unsafe impl Sync for VirtioNet {}

impl VirtioNet {
    /// virtio-net デバイスを初期化する
    pub fn new() -> Option<Self> {
        let dev = pci::find_virtio_net()?;
        serial_println!(
            "virtio-net found at PCI {:02x}:{:02x}.{}",
            dev.bus, dev.device, dev.function
        );

        // BAR0 を読み取る
        let bar0 = pci::read_bar(dev.bus, dev.device, dev.function, 0);
        if bar0 & 1 == 0 {
            serial_println!("virtio-net BAR0 is MMIO, not I/O port — unsupported");
            return None;
        }
        let io_base = (bar0 & 0xFFFC) as u16;
        serial_println!("virtio-net I/O base: {:#x}", io_base);

        // デバイスリセット
        unsafe {
            Port::<u8>::new(io_base + 0x12).write(0);
        }

        // ACKNOWLEDGE
        unsafe {
            Port::<u8>::new(io_base + 0x12).write(VIRTIO_STATUS_ACKNOWLEDGE);
        }

        // DRIVER
        unsafe {
            Port::<u8>::new(io_base + 0x12)
                .write(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);
        }

        // Feature negotiation
        let device_features = unsafe { Port::<u32>::new(io_base + 0x00).read() };
        serial_println!("virtio-net device features: {:#010x}", device_features);

        // MAC アドレスがあるか確認
        let has_mac = (device_features as u64 & VIRTIO_NET_F_MAC) != 0;
        serial_println!("virtio-net has MAC address: {}", has_mac);

        // ゲストの機能ビット (VIRTIO_NET_F_MAC のみ)
        let guest_features = if has_mac { VIRTIO_NET_F_MAC as u32 } else { 0 };
        unsafe {
            Port::<u32>::new(io_base + 0x04).write(guest_features);
        }

        // ---- Virtqueue 0 (receiveq) のセットアップ ----
        unsafe {
            Port::<u16>::new(io_base + 0x0E).write(0);
        }
        let queue_size = unsafe { Port::<u16>::new(io_base + 0x0C).read() };
        serial_println!("virtio-net receiveq size: {}", queue_size);

        if queue_size == 0 {
            serial_println!("virtio-net receiveq size is 0");
            return None;
        }

        let rx_vq_ptr = Self::allocate_virtqueue(queue_size)?;
        let rx_vq_phys = rx_vq_ptr as u64;
        serial_println!("virtio-net receiveq at phys {:#x}", rx_vq_phys);

        unsafe {
            Port::<u32>::new(io_base + 0x08).write((rx_vq_phys / 4096) as u32);
        }

        // ---- Virtqueue 1 (transmitq) のセットアップ ----
        unsafe {
            Port::<u16>::new(io_base + 0x0E).write(1);
        }
        let tx_queue_size = unsafe { Port::<u16>::new(io_base + 0x0C).read() };
        serial_println!("virtio-net transmitq size: {}", tx_queue_size);

        let tx_vq_ptr = Self::allocate_virtqueue(queue_size)?;
        let tx_vq_phys = tx_vq_ptr as u64;
        serial_println!("virtio-net transmitq at phys {:#x}", tx_vq_phys);

        unsafe {
            Port::<u32>::new(io_base + 0x08).write((tx_vq_phys / 4096) as u32);
        }

        // DRIVER_OK
        unsafe {
            Port::<u8>::new(io_base + 0x12).write(
                VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_DRIVER_OK,
            );
        }

        // MAC アドレスを読み取る (device config は offset 0x14 から)
        // virtio-net の device config: MAC アドレスが最初の 6 バイト
        let mut mac_address = [0u8; 6];
        if has_mac {
            for i in 0..6 {
                mac_address[i] = unsafe { Port::<u8>::new(io_base + 0x14 + i as u16).read() };
            }
            serial_println!(
                "virtio-net MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                mac_address[0], mac_address[1], mac_address[2],
                mac_address[3], mac_address[4], mac_address[5]
            );
        } else {
            // MAC アドレスがない場合はデフォルト
            mac_address = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
            serial_println!("virtio-net using default MAC");
        }

        let status = unsafe { Port::<u8>::new(io_base + 0x12).read() };
        serial_println!("virtio-net status after init: {:#x}", status);

        // 受信バッファを確保
        let rx_buffers = Self::allocate_rx_buffers()?;

        let mut driver = VirtioNet {
            io_base,
            queue_size,
            rx_vq_ptr,
            tx_vq_ptr,
            rx_buffers,
            rx_next_desc: 0,
            rx_last_used_idx: 0,
            tx_next_desc: 0,
            tx_last_used_idx: 0,
            mac_address,
        };

        // 受信バッファを receiveq に登録
        driver.fill_rx_queue();

        Some(driver)
    }

    /// Virtqueue 用のページアラインメモリを確保
    fn allocate_virtqueue(queue_size: u16) -> Option<*mut u8> {
        let desc_size = (queue_size as usize) * 16;
        let avail_size = 4 + (queue_size as usize) * 2;
        let used_offset = align_up(desc_size + avail_size, 4096);
        let used_size = 4 + (queue_size as usize) * 8;
        let total_size = align_up(used_offset + used_size, 4096);

        let layout = Layout::from_size_align(total_size, 4096).ok()?;
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            None
        } else {
            Some(ptr)
        }
    }

    /// 受信バッファを確保
    fn allocate_rx_buffers() -> Option<*mut u8> {
        let total_size = RX_BUFFER_SIZE * RX_BUFFER_COUNT;
        let layout = Layout::from_size_align(total_size, 4096).ok()?;
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            None
        } else {
            Some(ptr)
        }
    }

    /// 受信バッファを receiveq に追加
    fn fill_rx_queue(&mut self) {
        for i in 0..RX_BUFFER_COUNT {
            let buf_addr = unsafe { self.rx_buffers.add(i * RX_BUFFER_SIZE) } as u64;
            let desc_idx = self.rx_next_desc;

            // ディスクリプタを設定 (デバイスが書き込む = WRITE フラグ)
            self.write_rx_desc(desc_idx, buf_addr, RX_BUFFER_SIZE as u32, VIRTQ_DESC_F_WRITE, 0);

            // Available Ring に追加
            self.add_to_rx_avail(desc_idx);

            self.rx_next_desc = (self.rx_next_desc + 1) % self.queue_size;
        }

        // デバイスに通知
        self.notify_rx();
    }

    /// receiveq のディスクリプタを書き込む
    fn write_rx_desc(&self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let offset = (idx as usize) * 16;
        let ptr = unsafe { self.rx_vq_ptr.add(offset) };
        unsafe {
            (ptr as *mut u64).write_volatile(addr);
            (ptr.add(8) as *mut u32).write_volatile(len);
            (ptr.add(12) as *mut u16).write_volatile(flags);
            (ptr.add(14) as *mut u16).write_volatile(next);
        }
    }

    /// receiveq の Available Ring に追加
    fn add_to_rx_avail(&self, desc_idx: u16) {
        let avail_offset = (self.queue_size as usize) * 16;
        let avail_ptr = unsafe { self.rx_vq_ptr.add(avail_offset) };

        let avail_idx = unsafe { (avail_ptr.add(2) as *const u16).read_volatile() };
        let ring_entry_offset = 4 + ((avail_idx % self.queue_size) as usize) * 2;
        unsafe {
            (avail_ptr.add(ring_entry_offset) as *mut u16).write_volatile(desc_idx);
        }
        fence(Ordering::SeqCst);
        unsafe {
            (avail_ptr.add(2) as *mut u16).write_volatile(avail_idx.wrapping_add(1));
        }
        fence(Ordering::SeqCst);
    }

    /// receiveq に通知
    fn notify_rx(&self) {
        unsafe {
            Port::<u16>::new(self.io_base + 0x10).write(0);
        }
    }

    /// transmitq に通知
    fn notify_tx(&self) {
        unsafe {
            Port::<u16>::new(self.io_base + 0x10).write(1);
        }
    }

    /// ISR ステータスレジスタを読み取る（割り込みの確認と ACK）
    ///
    /// virtio-legacy の ISR status register は offset 0x13 にある。
    /// 読み取ると割り込みフラグがクリアされる。
    /// この port I/O は QEMU のイベントループをキックする副作用がある。
    pub fn read_isr_status(&self) -> u8 {
        unsafe { Port::<u8>::new(self.io_base + 0x13).read() }
    }

    /// virtio-net のリンク状態を返す。
    /// QEMU 環境では仮想デバイスなので常に link up (true) を返す。
    /// 実機で VIRTIO_NET_F_STATUS がネゴシエートされた場合は
    /// デバイスステータスから読み取る拡張が可能。
    pub fn is_link_up(&self) -> bool {
        // virtio-net はデバイスが存在すれば link up とみなす
        true
    }

    /// パケットを送信する
    ///
    /// data: Ethernet フレーム (ヘッダーなし virtio-net ヘッダー)
    /// virtio-net ヘッダーはこの関数内で付加する
    pub fn send_packet(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.len() > 1514 {
            return Err("Packet too large");
        }

        // virtio-net ヘッダー + データ
        let header = VirtioNetHeader::empty();
        let total_len = core::mem::size_of::<VirtioNetHeader>() + data.len();

        // 一時バッファを確保 (スタック上)
        let mut buf = [0u8; 1600];
        let header_bytes = unsafe {
            core::slice::from_raw_parts(
                &header as *const VirtioNetHeader as *const u8,
                core::mem::size_of::<VirtioNetHeader>(),
            )
        };
        buf[..header_bytes.len()].copy_from_slice(header_bytes);
        buf[header_bytes.len()..header_bytes.len() + data.len()].copy_from_slice(data);

        // ディスクリプタを設定
        let desc_idx = self.tx_next_desc;
        self.write_tx_desc(desc_idx, buf.as_ptr() as u64, total_len as u32, 0, 0);
        self.tx_next_desc = (self.tx_next_desc + 1) % self.queue_size;

        // Available Ring に追加
        let avail_offset = (self.queue_size as usize) * 16;
        let avail_ptr = unsafe { self.tx_vq_ptr.add(avail_offset) };

        let avail_idx = unsafe { (avail_ptr.add(2) as *const u16).read_volatile() };
        let ring_entry_offset = 4 + ((avail_idx % self.queue_size) as usize) * 2;
        unsafe {
            (avail_ptr.add(ring_entry_offset) as *mut u16).write_volatile(desc_idx);
        }
        fence(Ordering::SeqCst);
        unsafe {
            (avail_ptr.add(2) as *mut u16).write_volatile(avail_idx.wrapping_add(1));
        }
        fence(Ordering::SeqCst);

        // デバイスに通知
        self.notify_tx();

        // 完了を待つ (簡易的なポーリング)
        let desc_size = (self.queue_size as usize) * 16;
        let avail_size = 4 + (self.queue_size as usize) * 2;
        let used_offset = align_up(desc_size + avail_size, 4096);
        let used_ptr = unsafe { self.tx_vq_ptr.add(used_offset) };

        let expected_used_idx = self.tx_last_used_idx.wrapping_add(1);
        let mut spin_count = 0u64;
        loop {
            fence(Ordering::SeqCst);
            let used_idx = unsafe { (used_ptr.add(2) as *const u16).read_volatile() };
            if used_idx == expected_used_idx {
                break;
            }
            spin_count += 1;
            if spin_count > 10_000_000 {
                return Err("virtio-net send timeout");
            }
            core::hint::spin_loop();
        }
        self.tx_last_used_idx = expected_used_idx;

        Ok(())
    }

    /// transmitq のディスクリプタを書き込む
    fn write_tx_desc(&self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let offset = (idx as usize) * 16;
        let ptr = unsafe { self.tx_vq_ptr.add(offset) };
        unsafe {
            (ptr as *mut u64).write_volatile(addr);
            (ptr.add(8) as *mut u32).write_volatile(len);
            (ptr.add(12) as *mut u16).write_volatile(flags);
            (ptr.add(14) as *mut u16).write_volatile(next);
        }
    }

    /// 受信パケットがあれば取得する (ノンブロッキング)
    ///
    /// 戻り値: Some((data, len)) または None
    /// data には virtio-net ヘッダー + Ethernet フレームが含まれる
    pub fn receive_packet(&mut self) -> Option<Vec<u8>> {
        let desc_size = (self.queue_size as usize) * 16;
        let avail_size = 4 + (self.queue_size as usize) * 2;
        let used_offset = align_up(desc_size + avail_size, 4096);
        let used_ptr = unsafe { self.rx_vq_ptr.add(used_offset) };

        let used_idx = unsafe { (used_ptr.add(2) as *const u16).read_volatile() };
        if used_idx == self.rx_last_used_idx {
            return None; // 新しいパケットなし
        }
        // Used Ring からエントリを取得
        let ring_entry_idx = (self.rx_last_used_idx % self.queue_size) as usize;
        let used_elem_ptr = unsafe { used_ptr.add(4 + ring_entry_idx * 8) };
        let desc_id = unsafe { (used_elem_ptr as *const u32).read_volatile() } as u16;
        let written_len = unsafe { (used_elem_ptr.add(4) as *const u32).read_volatile() } as usize;

        self.rx_last_used_idx = self.rx_last_used_idx.wrapping_add(1);

        // 対応する受信バッファからデータをコピー
        // desc_id から受信バッファのインデックスを計算
        let buf_idx = desc_id as usize % RX_BUFFER_COUNT;
        let buf_ptr = unsafe { self.rx_buffers.add(buf_idx * RX_BUFFER_SIZE) };

        let mut data = vec![0u8; written_len];
        unsafe {
            core::ptr::copy_nonoverlapping(buf_ptr, data.as_mut_ptr(), written_len);
        }

        // バッファを再度 receiveq に追加
        let buf_addr = buf_ptr as u64;
        self.write_rx_desc(desc_id, buf_addr, RX_BUFFER_SIZE as u32, VIRTQ_DESC_F_WRITE, 0);
        self.add_to_rx_avail(desc_id);
        self.notify_rx();

        // virtio-net ヘッダーをスキップして Ethernet フレームを返す
        let header_size = core::mem::size_of::<VirtioNetHeader>();
        if data.len() > header_size {
            Some(data[header_size..].to_vec())
        } else {
            None
        }
    }
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}
