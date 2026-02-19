// e1000e.rs — Intel e1000e (82574L 互換) NIC ドライバ
//
// Intel e1000e は広く普及した Gigabit Ethernet NIC。
// QEMU でも `-device e1000e` でエミュレーション可能。
// 実機では Intel 82574L や同系統の NIC で動作する。
//
// ## 動作モード
//
// ポーリングモードで実装（割り込みは使わない）。
// net_poller カーネルタスクが HLT ループで定期的にパケットを受信する。
//
// ## ディスクリプタ
//
// Legacy descriptor format（16 バイト）を使用。
// RX/TX ともにリングバッファ構造で、ハードウェアが Head を進め、
// ソフトウェアが Tail を進めることでバッファを管理する。
//
// ## 参考資料
//
// Intel 82574L GbE Controller Family Datasheet
// https://www.intel.com/content/dam/doc/datasheet/82574l-gbe-controller-datasheet.pdf

use alloc::vec::Vec;
use core::alloc::Layout;
use spin::Mutex;

use crate::pci;
use crate::serial_println;

/// グローバルな e1000e ドライバインスタンス。
/// PCI で検出・初期化された e1000e デバイスを保持する。
/// virtio-net と同様に Option<E1000e> で管理し、None なら未検出。
pub static E1000E: Mutex<Option<E1000e>> = Mutex::new(None);

// ============================================================
// レジスタオフセット定数
// ============================================================

/// e1000e の MMIO レジスタオフセット。
/// Intel 82574L データシートの Table 13-2 (Register Summary) に基づく。
mod regs {
    /// Device Control — デバイス全体の制御
    /// RST (bit 26) でリセット、SLU (bit 6) で Link Up を強制
    pub const CTRL: u64 = 0x0000;

    /// Device Status — デバイスの現在の状態
    /// LU (bit 1) でリンクアップ状態を確認
    pub const STATUS: u64 = 0x0008;

    /// Interrupt Cause Read — 割り込み原因（read-to-clear）
    /// ポーリングモードでも、読み取ることでイベントフラグをクリアする
    pub const ICR: u64 = 0x00C0;

    /// Interrupt Mask Set — 割り込みマスクのセット
    #[allow(dead_code)]
    pub const IMS: u64 = 0x00D0;

    /// Interrupt Mask Clear — 割り込みマスクのクリア（全ビット書き込みで全割り込み無効化）
    pub const IMC: u64 = 0x00D8;

    /// Receive Control — 受信動作の制御
    /// EN (bit 1) で受信有効化、BAM (bit 15) でブロードキャスト受信
    pub const RCTL: u64 = 0x0100;

    /// Transmit Control — 送信動作の制御
    /// EN (bit 1) で送信有効化、PSP (bit 3) で短いパケットのパディング
    pub const TCTL: u64 = 0x0400;

    /// RX Descriptor Base Address Low — RX リングバッファの物理アドレス（下位 32 ビット）
    pub const RDBAL: u64 = 0x2800;
    /// RX Descriptor Base Address High — RX リングバッファの物理アドレス（上位 32 ビット）
    pub const RDBAH: u64 = 0x2804;
    /// RX Descriptor Length — RX リングバッファのサイズ（バイト単位、128 の倍数）
    pub const RDLEN: u64 = 0x2808;
    /// RX Descriptor Head — ハードウェアが次に書き込む位置（読み取り専用）
    pub const RDH: u64 = 0x2810;
    /// RX Descriptor Tail — ソフトウェアが最後に準備した位置
    pub const RDT: u64 = 0x2818;

    /// TX Descriptor Base Address Low — TX リングバッファの物理アドレス（下位 32 ビット）
    pub const TDBAL: u64 = 0x3800;
    /// TX Descriptor Base Address High — TX リングバッファの物理アドレス（上位 32 ビット）
    pub const TDBAH: u64 = 0x3804;
    /// TX Descriptor Length — TX リングバッファのサイズ（バイト単位、128 の倍数）
    pub const TDLEN: u64 = 0x3808;
    /// TX Descriptor Head — ハードウェアが次に送信する位置（読み取り専用）
    pub const TDH: u64 = 0x3810;
    /// TX Descriptor Tail — ソフトウェアが最後に書き込んだ位置
    pub const TDT: u64 = 0x3818;

    /// Receive Address Low — MAC アドレスの下位 32 ビット（QEMU が自動設定）
    pub const RAL: u64 = 0x5400;
    /// Receive Address High — MAC アドレスの上位 16 ビット + AV（Address Valid）ビット
    pub const RAH: u64 = 0x5404;
}

// ============================================================
// CTRL レジスタのビット定数
// ============================================================

/// Set Link Up — リンクを強制的にアップ状態にする
const CTRL_SLU: u32 = 1 << 6;

/// Device Reset — デバイスをリセットする。
/// セットするとハードウェアが自動的にクリアする。
const CTRL_RST: u32 = 1 << 26;

// ============================================================
// RCTL (Receive Control) レジスタのビット定数
// ============================================================

/// Receiver Enable — 受信機能を有効化
const RCTL_EN: u32 = 1 << 1;

/// Broadcast Accept Mode — ブロードキャストフレームを受信
const RCTL_BAM: u32 = 1 << 15;

/// Strip Ethernet CRC — 受信フレームから CRC を除去
/// CRC をストリップすることで上位層に渡すデータがクリーンになる
const RCTL_SECRC: u32 = 1 << 26;

// Buffer Size は RCTL[16:17] = 0b00 で 2048 バイト（デフォルト）

// ============================================================
// TCTL (Transmit Control) レジスタのビット定数
// ============================================================

/// Transmit Enable — 送信機能を有効化
const TCTL_EN: u32 = 1 << 1;

/// Pad Short Packets — 64 バイト未満のフレームを自動パディング
const TCTL_PSP: u32 = 1 << 3;

/// Collision Threshold — 衝突閾値（15 に設定）
/// Full duplex では使われないが、データシートの推奨値に従う
const TCTL_CT: u32 = 0x0F << 4;

/// Collision Distance — 衝突検出距離（63、Full duplex 推奨値）
const TCTL_COLD: u32 = 0x3F << 12;

// ============================================================
// ディスクリプタ構造体
// ============================================================

/// RX ディスクリプタ（Legacy format、16 バイト）
///
/// ハードウェアがパケットを受信すると、addr が指すバッファにデータを書き込み、
/// length に受信バイト数、status に DD (Descriptor Done) ビットをセットする。
#[repr(C)]
#[derive(Clone, Copy)]
struct RxDesc {
    /// バッファの物理アドレス（受信データの書き込み先）
    addr: u64,
    /// 受信したデータの長さ（バイト）
    length: u16,
    /// チェックサムオフロードの結果
    checksum: u16,
    /// ステータスビット — DD (bit 0) がセットされたら受信完了
    status: u8,
    /// エラービット
    errors: u8,
    /// VLAN タグ等の特殊情報
    special: u16,
}

/// TX ディスクリプタ（Legacy format、16 バイト）
///
/// ソフトウェアがパケットを送信するには、addr にデータの物理アドレス、
/// length にデータ長、cmd に EOP (End of Packet) + RS (Report Status) をセットし、
/// TDT レジスタを更新する。ハードウェアが送信完了すると status の DD ビットをセットする。
#[repr(C)]
#[derive(Clone, Copy)]
struct TxDesc {
    /// バッファの物理アドレス（送信データの読み取り元）
    addr: u64,
    /// 送信するデータの長さ（バイト）
    length: u16,
    /// Checksum Offset — チェックサムオフロード用（未使用、0）
    cso: u8,
    /// コマンドビット — EOP (bit 0), RS (bit 3)
    cmd: u8,
    /// ステータスビット — DD (bit 0) がセットされたら送信完了
    status: u8,
    /// Checksum Start — チェックサムオフロード用（未使用、0）
    css: u8,
    /// VLAN タグ等の特殊情報
    special: u16,
}

/// TX コマンド: End of Packet — このディスクリプタがパケットの末尾であることを示す
const TX_CMD_EOP: u8 = 1 << 0;

/// TX コマンド: Report Status — 送信完了時に DD ビットをセットするようハードウェアに要求
const TX_CMD_RS: u8 = 1 << 3;

/// RX/TX ステータス: Descriptor Done — ハードウェアが処理完了したことを示す
const STATUS_DD: u8 = 1 << 0;

/// RX リングバッファのディスクリプタ数
/// 32 個で十分（QEMU 環境ではパケット頻度が低い）
const RX_DESC_COUNT: usize = 32;

/// TX リングバッファのディスクリプタ数
const TX_DESC_COUNT: usize = 32;

/// 各 RX バッファのサイズ（2048 バイト）
/// RCTL の Buffer Size = 00b（デフォルト）に対応
const RX_BUFFER_SIZE: usize = 2048;

// ============================================================
// MMIO ヘルパー関数
// ============================================================

/// MMIO レジスタから 32 ビット値を読み取る。
/// アイデンティティマッピング（物理アドレス = 仮想アドレス）を前提とする。
fn mmio_read32(bar0: u64, offset: u64) -> u32 {
    unsafe { core::ptr::read_volatile((bar0 + offset) as *const u32) }
}

/// MMIO レジスタに 32 ビット値を書き込む。
fn mmio_write32(bar0: u64, offset: u64, value: u32) {
    unsafe { core::ptr::write_volatile((bar0 + offset) as *mut u32, value) }
}

// ============================================================
// e1000e ドライバ構造体
// ============================================================

/// Intel e1000e NIC ドライバ
///
/// RX/TX ディスクリプタリングと受信バッファを管理し、
/// MMIO 経由でハードウェアを制御する。
pub struct E1000e {
    /// BAR0 の MMIO ベースアドレス（物理アドレス = 仮想アドレス）
    bar0: u64,
    /// RX ディスクリプタリングへのポインタ（ページアライン済み）
    rx_descs: *mut RxDesc,
    /// TX ディスクリプタリングへのポインタ（ページアライン済み）
    tx_descs: *mut TxDesc,
    /// 各 RX ディスクリプタに対応する受信バッファへのポインタ
    rx_buffers: [*mut u8; RX_DESC_COUNT],
    /// 現在の RX インデックス（次に確認するディスクリプタ）
    rx_cur: usize,
    /// 現在の TX インデックス（次に使うディスクリプタ）
    tx_cur: usize,
    /// MAC アドレス（RAL/RAH レジスタから読み取った値）
    pub mac_address: [u8; 6],
}

// E1000e は生ポインタを含むが、Mutex で保護されるため Send/Sync は安全
unsafe impl Send for E1000e {}
unsafe impl Sync for E1000e {}

impl E1000e {
    /// e1000e デバイスを初期化する。
    ///
    /// PCI デバイス情報から BAR0 を読み取り、以下の手順で初期化する:
    /// 1. PCI Bus Master + Memory Space を有効化
    /// 2. BAR0（MMIO ベースアドレス）を取得
    /// 3. デバイスリセット
    /// 4. 割り込み無効化
    /// 5. MAC アドレス読み取り
    /// 6. RX/TX リングバッファ確保・設定
    /// 7. 送受信有効化
    fn new(dev: &pci::PciDevice) -> Result<Self, &'static str> {
        serial_println!("e1000e: initializing device at bus={} dev={} func={}",
            dev.bus, dev.device, dev.function);

        // --- PCI Command レジスタで Bus Master + Memory Space を有効化 ---
        // Bus Master: e1000e が DMA でメインメモリにアクセスするために必要。
        // Memory Space: BAR0 の MMIO 領域にアクセスするために必要。
        let cmd = pci::pci_config_read16(dev.bus, dev.device, dev.function, 0x04);
        pci::pci_config_write16(dev.bus, dev.device, dev.function, 0x04, cmd | 0x06);

        // --- BAR0 読み取り ---
        // BAR0 の type bits ([2:1]) を確認して 32-bit / 64-bit を判定する。
        //   00b = 32-bit MMIO
        //   10b = 64-bit MMIO（BAR0 + BAR1 を結合）
        let bar0_low = pci::read_bar(dev.bus, dev.device, dev.function, 0);
        let bar_type = (bar0_low >> 1) & 0x03;
        let bar0 = if bar_type == 0x02 {
            // 64-bit MMIO: BAR0 + BAR1 を結合
            let bar0_raw = pci::read_bar64(dev.bus, dev.device, dev.function, 0);
            bar0_raw & !0xF
        } else {
            // 32-bit MMIO
            (bar0_low & !0xF) as u64
        };
        serial_println!("e1000e: BAR0 = {:#x}", bar0);

        if bar0 == 0 {
            return Err("e1000e: BAR0 is zero");
        }

        // --- デバイスリセット ---
        // CTRL レジスタの RST ビット (bit 26) をセットしてリセットを開始する。
        // ハードウェアがリセット完了すると RST ビットを自動的にクリアする。
        let ctrl = mmio_read32(bar0, regs::CTRL);
        mmio_write32(bar0, regs::CTRL, ctrl | CTRL_RST);

        // リセット完了を待つ（RST ビットがクリアされるまで）
        // データシートでは最大 1ms とされているが、余裕を持って待つ
        for _ in 0..1000 {
            if mmio_read32(bar0, regs::CTRL) & CTRL_RST == 0 {
                break;
            }
            // 短い遅延（ビジーウェイト）
            for _ in 0..10000 {
                core::hint::spin_loop();
            }
        }
        serial_println!("e1000e: device reset complete");

        // --- 割り込み無効化 ---
        // ポーリングモードで動作するので、すべての割り込みを無効化する。
        // IMC (Interrupt Mask Clear) に全ビットを書き込むことで全割り込みをマスクする。
        mmio_write32(bar0, regs::IMC, 0xFFFFFFFF);
        // ICR を読み取って保留中の割り込みをクリアする
        let _ = mmio_read32(bar0, regs::ICR);

        // --- Set Link Up ---
        // CTRL レジスタの SLU ビット (bit 6) をセットしてリンクを強制アップする。
        let ctrl = mmio_read32(bar0, regs::CTRL);
        mmio_write32(bar0, regs::CTRL, ctrl | CTRL_SLU);

        // --- MAC アドレス読み取り ---
        // RAL (Receive Address Low) と RAH (Receive Address High) レジスタから
        // MAC アドレスを読み取る。QEMU は RAL/RAH を初期化済みなので EEPROM 不要。
        let ral = mmio_read32(bar0, regs::RAL);
        let rah = mmio_read32(bar0, regs::RAH);
        let mac_address = [
            (ral & 0xFF) as u8,
            ((ral >> 8) & 0xFF) as u8,
            ((ral >> 16) & 0xFF) as u8,
            ((ral >> 24) & 0xFF) as u8,
            (rah & 0xFF) as u8,
            ((rah >> 8) & 0xFF) as u8,
        ];
        serial_println!("e1000e: MAC = {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac_address[0], mac_address[1], mac_address[2],
            mac_address[3], mac_address[4], mac_address[5]);

        // MAC アドレスが全てゼロの場合はエラー（デバイスが正しく初期化されていない）
        if mac_address == [0; 6] {
            return Err("e1000e: MAC address is all zeros");
        }

        // --- RX リングバッファの確保 ---
        // RX ディスクリプタリング（16 バイト × RX_DESC_COUNT）をページアライン確保
        let rx_ring_size = RX_DESC_COUNT * core::mem::size_of::<RxDesc>();
        let rx_layout = Layout::from_size_align(rx_ring_size, 4096)
            .map_err(|_| "e1000e: RX ring layout error")?;
        let rx_ring_ptr = unsafe { alloc::alloc::alloc_zeroed(rx_layout) };
        if rx_ring_ptr.is_null() {
            return Err("e1000e: failed to allocate RX ring");
        }
        let rx_descs = rx_ring_ptr as *mut RxDesc;

        // 各 RX ディスクリプタ用のバッファを確保
        let mut rx_buffers = [core::ptr::null_mut(); RX_DESC_COUNT];
        let buf_layout = Layout::from_size_align(RX_BUFFER_SIZE, 16)
            .map_err(|_| "e1000e: RX buffer layout error")?;
        for i in 0..RX_DESC_COUNT {
            let buf = unsafe { alloc::alloc::alloc_zeroed(buf_layout) };
            if buf.is_null() {
                return Err("e1000e: failed to allocate RX buffer");
            }
            rx_buffers[i] = buf;
            // ディスクリプタにバッファの物理アドレスをセット
            // アイデンティティマッピングなので仮想アドレス = 物理アドレス
            unsafe {
                (*rx_descs.add(i)).addr = buf as u64;
                (*rx_descs.add(i)).status = 0;
            }
        }

        // --- TX リングバッファの確保 ---
        let tx_ring_size = TX_DESC_COUNT * core::mem::size_of::<TxDesc>();
        let tx_layout = Layout::from_size_align(tx_ring_size, 4096)
            .map_err(|_| "e1000e: TX ring layout error")?;
        let tx_ring_ptr = unsafe { alloc::alloc::alloc_zeroed(tx_layout) };
        if tx_ring_ptr.is_null() {
            return Err("e1000e: failed to allocate TX ring");
        }
        let tx_descs = tx_ring_ptr as *mut TxDesc;

        // --- RX レジスタ設定 ---
        // RDBAL/RDBAH: RX ディスクリプタリングの物理アドレス
        let rx_ring_phys = rx_ring_ptr as u64;
        mmio_write32(bar0, regs::RDBAL, rx_ring_phys as u32);
        mmio_write32(bar0, regs::RDBAH, (rx_ring_phys >> 32) as u32);
        // RDLEN: リングサイズ（バイト単位）
        mmio_write32(bar0, regs::RDLEN, rx_ring_size as u32);
        // RDH: Head = 0（最初のディスクリプタから）
        mmio_write32(bar0, regs::RDH, 0);
        // RDT: Tail = RX_DESC_COUNT - 1（全ディスクリプタをハードウェアに渡す）
        mmio_write32(bar0, regs::RDT, (RX_DESC_COUNT - 1) as u32);

        // --- TX レジスタ設定 ---
        let tx_ring_phys = tx_ring_ptr as u64;
        mmio_write32(bar0, regs::TDBAL, tx_ring_phys as u32);
        mmio_write32(bar0, regs::TDBAH, (tx_ring_phys >> 32) as u32);
        mmio_write32(bar0, regs::TDLEN, tx_ring_size as u32);
        mmio_write32(bar0, regs::TDH, 0);
        mmio_write32(bar0, regs::TDT, 0);

        // --- RCTL（受信制御）設定 ---
        // EN: 受信有効化
        // BAM: ブロードキャストフレーム受信
        // SECRC: CRC をストリップ
        // Buffer Size = 2048 (RCTL[16:17] = 00b、デフォルト)
        mmio_write32(bar0, regs::RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);

        // --- TCTL（送信制御）設定 ---
        // EN: 送信有効化
        // PSP: 短いパケットをパディング
        // CT: Collision Threshold = 15
        // COLD: Collision Distance = 63 (Full duplex)
        mmio_write32(bar0, regs::TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);

        let status = mmio_read32(bar0, regs::STATUS);
        serial_println!("e1000e: STATUS = {:#x}, link up = {}", status, (status & 0x02) != 0);

        serial_println!("e1000e: initialization complete");

        Ok(E1000e {
            bar0,
            rx_descs,
            tx_descs,
            rx_buffers,
            rx_cur: 0,
            tx_cur: 0,
            mac_address,
        })
    }

    /// リンク状態を確認する。
    /// STATUS レジスタの bit 1 (LU: Link Up) を読み取る。
    /// true ならリンクアップ（ケーブル接続＋通信可能）。
    pub fn is_link_up(&self) -> bool {
        let status = mmio_read32(self.bar0, regs::STATUS);
        status & (1 << 1) != 0
    }

    /// Ethernet フレームを送信する。
    ///
    /// TX ディスクリプタに送信データをセットし、TDT レジスタを更新してハードウェアに送信を指示する。
    /// DD ビットをポーリングして送信完了を待つ。
    /// リンクダウン時はパケットを送信せずエラーを返す。
    ///
    /// data: Ethernet フレーム全体（MAC ヘッダー含む）
    pub fn send_packet(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if !self.is_link_up() {
            return Err("e1000e: link is down");
        }
        if data.len() > 1514 {
            return Err("e1000e: packet too large");
        }

        let idx = self.tx_cur;
        let desc = unsafe { &mut *self.tx_descs.add(idx) };

        // 送信データをディスクリプタのバッファにコピー
        // TX バッファは動的に確保せず、送信データの物理アドレスを直接セットする
        // アイデンティティマッピングなのでスタック上のデータでも物理アドレスとして使える
        // ただし DMA の安全性のため、ヒープにコピーする
        let buf_layout = Layout::from_size_align(data.len().max(64), 16)
            .map_err(|_| "e1000e: TX buffer layout error")?;
        let tx_buf = unsafe { alloc::alloc::alloc_zeroed(buf_layout) };
        if tx_buf.is_null() {
            return Err("e1000e: failed to allocate TX buffer");
        }
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), tx_buf, data.len());
        }

        desc.addr = tx_buf as u64;
        desc.length = data.len() as u16;
        desc.cmd = TX_CMD_EOP | TX_CMD_RS;
        desc.status = 0;
        desc.cso = 0;
        desc.css = 0;
        desc.special = 0;

        // TDT を更新してハードウェアに送信を指示
        self.tx_cur = (idx + 1) % TX_DESC_COUNT;
        mmio_write32(self.bar0, regs::TDT, self.tx_cur as u32);

        // DD ビットをポーリングして送信完了を待つ（最大 10ms）
        for _ in 0..100000 {
            if desc.status & STATUS_DD != 0 {
                // 送信完了。TX バッファを解放する。
                unsafe { alloc::alloc::dealloc(tx_buf, buf_layout); }
                return Ok(());
            }
            core::hint::spin_loop();
        }

        // タイムアウト。TX バッファを解放してエラーを返す。
        unsafe { alloc::alloc::dealloc(tx_buf, buf_layout); }
        Err("e1000e: TX timeout (DD bit not set)")
    }

    /// Ethernet フレームを受信する（ノンブロッキング）。
    ///
    /// 現在の RX ディスクリプタの DD ビットを確認し、
    /// セットされていればデータを Vec にコピーして返す。
    /// パケットがなければ None を返す。
    pub fn receive_packet(&mut self) -> Option<Vec<u8>> {
        let idx = self.rx_cur;
        let desc = unsafe { &mut *self.rx_descs.add(idx) };

        // DD ビットが立っていなければパケットなし
        if desc.status & STATUS_DD == 0 {
            return None;
        }

        let length = desc.length as usize;
        if length == 0 || length > RX_BUFFER_SIZE {
            // 不正なサイズのパケットはスキップ
            desc.status = 0;
            let old_tail = mmio_read32(self.bar0, regs::RDT);
            mmio_write32(self.bar0, regs::RDT, (old_tail + 1) % RX_DESC_COUNT as u32);
            self.rx_cur = (idx + 1) % RX_DESC_COUNT;
            return None;
        }

        // データを Vec にコピー
        let mut packet = Vec::with_capacity(length);
        unsafe {
            let buf = self.rx_buffers[idx];
            packet.extend_from_slice(core::slice::from_raw_parts(buf, length));
        }

        // ディスクリプタをリセットしてハードウェアに返す
        desc.status = 0;
        desc.length = 0;

        // RDT を更新して、このディスクリプタをハードウェアに再利用可能と伝える
        mmio_write32(self.bar0, regs::RDT, idx as u32);

        self.rx_cur = (idx + 1) % RX_DESC_COUNT;

        Some(packet)
    }

    /// ICR を読み取って保留中の割り込みをクリアする。
    /// ポーリングモードでは、イベントフラグをクリアしないとハードウェアが
    /// 新しいイベントを通知しない場合があるため、定期的に呼び出す。
    pub fn clear_interrupts(&mut self) {
        let _ = mmio_read32(self.bar0, regs::ICR);
    }
}

// ============================================================
// 初期化関数
// ============================================================

/// e1000e ドライバを初期化する。
///
/// PCI バスから Intel e1000e NIC を探し、見つかったら初期化して
/// グローバル変数 E1000E に格納する。
pub fn init() {
    let dev = match pci::find_e1000e() {
        Some(dev) => dev,
        None => {
            serial_println!("e1000e: device not found");
            return;
        }
    };

    match E1000e::new(&dev) {
        Ok(driver) => {
            serial_println!("e1000e: driver initialized successfully");
            *E1000E.lock() = Some(driver);
        }
        Err(e) => {
            serial_println!("e1000e: initialization failed: {}", e);
        }
    }
}
