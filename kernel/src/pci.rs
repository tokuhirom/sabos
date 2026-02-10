// pci.rs — PCI バス列挙 (PCI Configuration Space アクセス)
//
// PCI (Peripheral Component Interconnect) は PC のデバイスを接続するバス規格。
// CPU から各デバイスの設定情報（ベンダー ID、デバイス ID、BAR 等）を読み書きできる。
//
// PCI Type 1 アクセス方式:
//   I/O ポート 0xCF8 (CONFIG_ADDRESS) にアドレスを書き込み、
//   I/O ポート 0xCFC (CONFIG_DATA) からデータを読み書きする。
//
// CONFIG_ADDRESS のビット構造:
//   [31]    = Enable bit (1 で有効)
//   [23:16] = バス番号 (0〜255)
//   [15:11] = デバイス番号 (0〜31)
//   [10:8]  = ファンクション番号 (0〜7)
//   [7:2]   = レジスタ番号 (4バイトアライン)
//   [1:0]   = 常に 0
//
// Configuration Space の主要レジスタ:
//   0x00: ベンダー ID (16bit) + デバイス ID (16bit)
//   0x08: リビジョン ID (8bit) + クラスコード (24bit)
//   0x0C: キャッシュラインサイズ等
//   0x10〜0x24: BAR0〜BAR5 (Base Address Register)
//   0x2C: サブシステムベンダー ID + サブシステム ID
//   0x3C: 割り込みライン + 割り込みピン

use alloc::vec::Vec;
use x86_64::instructions::port::Port;

/// PCI Configuration Space のアドレスポート（書き込み専用）
const CONFIG_ADDRESS: u16 = 0xCF8;
/// PCI Configuration Space のデータポート（読み書き）
const CONFIG_DATA: u16 = 0xCFC;

/// PCI デバイスの情報を保持する構造体。
/// enumerate_bus() で列挙されたデバイスの基本情報をまとめる。
#[derive(Debug, Clone)]
pub struct PciDevice {
    /// バス番号 (0〜255)
    pub bus: u8,
    /// デバイス番号 (0〜31)
    pub device: u8,
    /// ファンクション番号 (0〜7)
    pub function: u8,
    /// ベンダー ID（デバイスの製造元を識別）
    /// 例: 0x8086 = Intel, 0x1AF4 = Red Hat (virtio)
    pub vendor_id: u16,
    /// デバイス ID（デバイスの種類を識別）
    pub device_id: u16,
    /// クラスコード（デバイスの大分類）
    /// 例: 0x01 = マスストレージ, 0x02 = ネットワーク, 0x06 = ブリッジ
    pub class_code: u8,
    /// サブクラスコード（クラス内の細分類）
    pub subclass: u8,
    /// プログラミングインターフェース（さらに細かい分類）
    pub prog_if: u8,
}

/// PCI Configuration Space から 32 ビット値を読み取る。
///
/// CONFIG_ADDRESS にアドレスを書き込み、CONFIG_DATA から 32 ビット読む。
/// offset は 4 バイトアラインされている必要がある（下位 2 ビットは 0）。
pub fn pci_config_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    // CONFIG_ADDRESS の値を構築する
    // [31] = 1 (Enable), [23:16] = bus, [15:11] = device, [10:8] = function, [7:0] = offset
    let address: u32 = (1 << 31)                    // Enable bit
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC);                 // 下位 2 ビットをマスク（4バイトアライン）

    unsafe {
        let mut addr_port = Port::<u32>::new(CONFIG_ADDRESS);
        let mut data_port = Port::<u32>::new(CONFIG_DATA);
        addr_port.write(address);
        data_port.read()
    }
}

/// PCI Configuration Space から 16 ビット値を読み取る。
///
/// 32 ビット読み取りを行い、offset のアライメントに応じて上位/下位 16 ビットを返す。
pub fn pci_config_read16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let val32 = pci_config_read32(bus, device, function, offset & 0xFC);
    // offset の bit 1 で上位/下位 16 ビットを選択
    // offset が 4n+0 なら下位 16 ビット、4n+2 なら上位 16 ビット
    ((val32 >> ((offset & 2) * 8)) & 0xFFFF) as u16
}

/// BAR (Base Address Register) の値を読み取る。
///
/// BAR は PCI デバイスが使用するメモリ領域または I/O ポート領域のベースアドレスを格納する。
/// BAR のオフセットは 0x10 + bar_index * 4 (BAR0=0x10, BAR1=0x14, ..., BAR5=0x24)。
///
/// BAR の最下位ビット (bit 0) が:
///   0 = メモリマップド I/O (MMIO)
///   1 = I/O ポートマップド
///
/// I/O ポートマップドの場合、bit [31:2] がポートベースアドレス。
pub fn read_bar(bus: u8, device: u8, function: u8, bar_index: u8) -> u32 {
    let offset = 0x10 + bar_index * 4;
    pci_config_read32(bus, device, function, offset)
}

/// PCI バス 0 のすべてのデバイスを列挙する。
///
/// バス 0 のデバイス 0〜31、各デバイスのファンクション 0〜7 をスキャンする。
/// ベンダー ID が 0xFFFF のデバイスは存在しない（空スロット）のでスキップ。
///
/// マルチファンクションデバイスの判定:
///   ファンクション 0 のヘッダータイプ (offset 0x0E) の bit 7 が:
///   1 = マルチファンクション → ファンクション 1〜7 もスキャン
///   0 = シングルファンクション → ファンクション 0 のみ
pub fn enumerate_bus() -> Vec<PciDevice> {
    let mut devices = Vec::new();

    for device_num in 0..32u8 {
        // まずファンクション 0 を確認
        let vendor_id = pci_config_read16(0, device_num, 0, 0x00);
        if vendor_id == 0xFFFF {
            // デバイスが存在しない
            continue;
        }

        // ヘッダータイプを読んでマルチファンクションか判定
        let header_type = pci_config_read16(0, device_num, 0, 0x0E) as u8;
        let is_multi_function = (header_type & 0x80) != 0;

        // スキャンするファンクション数を決定
        let max_func = if is_multi_function { 8 } else { 1 };

        for func in 0..max_func {
            let vid = pci_config_read16(0, device_num, func, 0x00);
            if vid == 0xFFFF {
                continue;
            }

            let did = pci_config_read16(0, device_num, func, 0x02);

            // クラスコード (offset 0x08): [31:24]=class, [23:16]=subclass, [15:8]=prog_if
            let class_reg = pci_config_read32(0, device_num, func, 0x08);
            let class_code = ((class_reg >> 24) & 0xFF) as u8;
            let subclass = ((class_reg >> 16) & 0xFF) as u8;
            let prog_if = ((class_reg >> 8) & 0xFF) as u8;

            devices.push(PciDevice {
                bus: 0,
                device: device_num,
                function: func,
                vendor_id: vid,
                device_id: did,
                class_code,
                subclass,
                prog_if,
            });
        }
    }

    devices
}

/// virtio-blk デバイスを PCI バスから探す。
///
/// virtio デバイスの識別:
///   vendor_id = 0x1AF4 (Red Hat / virtio)
///   device_id = 0x1001 (virtio legacy block device)
///   ※ transitional デバイスの場合 device_id = 0x1042 の場合もあるが、
///     QEMU のデフォルト (-drive if=virtio) は legacy の 0x1001 を使う。
///
/// PCI バスから全ての virtio-blk デバイスを探す。
///
/// QEMU で複数の `-drive if=virtio` を指定すると、
/// 複数の virtio-blk デバイス (device_id=0x1001) が PCI バス上に現れる。
/// 見つかった全デバイスを Vec で返す。
pub fn find_all_virtio_blk() -> alloc::vec::Vec<PciDevice> {
    let devices = enumerate_bus();
    devices
        .into_iter()
        .filter(|dev| dev.vendor_id == 0x1AF4 && dev.device_id == 0x1001)
        .collect()
}

/// PCI Configuration Space に 32 ビット値を書き込む。
///
/// CONFIG_ADDRESS にアドレスを書き込み、CONFIG_DATA に 32 ビット書く。
/// offset は 4 バイトアラインされている必要がある（下位 2 ビットは 0）。
pub fn pci_config_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let address: u32 = (1 << 31)
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC);

    unsafe {
        let mut addr_port = Port::<u32>::new(CONFIG_ADDRESS);
        let mut data_port = Port::<u32>::new(CONFIG_DATA);
        addr_port.write(address);
        data_port.write(value);
    }
}

/// PCI Configuration Space に 16 ビット値を書き込む。
///
/// 32 ビット読み取りを行い、該当する 16 ビットだけ書き換えてから書き戻す。
/// これにより隣接する 16 ビットのレジスタ値を壊さない。
pub fn pci_config_write16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    // まず 32 ビット全体を読む
    let old = pci_config_read32(bus, device, function, offset & 0xFC);
    let shift = (offset & 2) * 8;
    // 該当する 16 ビットだけ書き換える
    let mask = !(0xFFFF_u32 << shift);
    let new_val = (old & mask) | ((value as u32) << shift);
    pci_config_write32(bus, device, function, offset & 0xFC, new_val);
}

/// AC97 オーディオコントローラを PCI バスから探す。
///
/// Intel AC97 コントローラの識別:
///   vendor_id = 0x8086 (Intel)
///   device_id = 0x2415 (82801AA AC97 Audio Controller)
///
/// QEMU の `-device AC97` でエミュレートされるデバイス。
/// 見つかった最初のデバイスを返す。見つからなければ None。
pub fn find_ac97() -> Option<PciDevice> {
    let devices = enumerate_bus();
    for dev in devices {
        if dev.vendor_id == 0x8086 && dev.device_id == 0x2415 {
            return Some(dev);
        }
    }
    None
}

/// virtio-net デバイスを PCI バスから探す。
///
/// virtio デバイスの識別:
///   vendor_id = 0x1AF4 (Red Hat / virtio)
///   device_id = 0x1000 (virtio legacy network device)
///
/// 見つかった最初のデバイスを返す。見つからなければ None。
pub fn find_virtio_net() -> Option<PciDevice> {
    let devices = enumerate_bus();
    for dev in devices {
        // virtio vendor ID = 0x1AF4
        // virtio-net legacy device ID = 0x1000
        if dev.vendor_id == 0x1AF4 && dev.device_id == 0x1000 {
            return Some(dev);
        }
    }
    None
}
