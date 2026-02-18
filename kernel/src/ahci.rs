// ahci.rs — AHCI (Advanced Host Controller Interface) SATA ドライバ
//
// AHCI は SATA デバイスを制御するための標準インターフェース。
// Intel ICH/PCH シリーズのオンボード SATA コントローラや、
// QEMU の `-device ahci` でエミュレートされるコントローラが該当する。
//
// ## AHCI の基本アーキテクチャ
//
// AHCI HBA (Host Bus Adapter) は PCI デバイスとして存在し、
// BAR5 (ABAR) に MMIO レジスタがマップされる。
//
// HBA には最大 32 個のポートがあり、各ポートに SATA デバイスが接続される。
// ポートの有効/無効は PI (Ports Implemented) レジスタのビットマスクで判定する。
//
// ## コマンド発行の流れ
//
// 1. Command List にコマンドヘッダーを設定（物理アドレス、FIS 長、PRDT 数など）
// 2. Command Table に ATA コマンド FIS と PRDT (Physical Region Descriptor Table) を設定
// 3. ポートの CI (Command Issue) レジスタにビットをセットしてコマンドを発行
// 4. CI ビットがクリアされるまでポーリングして完了を待つ
//
// ## 現在の実装
//
// - PIO/ポーリング方式（割り込みは使わない）
// - 後から DMA/割り込み対応に拡張可能な構造
// - 各ポートに Command List 1 スロット分 + Command Table 1 つを確保
//   （シングルスレッドでポーリング待ちするため、同時に 1 コマンドしか発行しない）

use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{fence, Ordering};
use spin::Mutex;
use crate::pci;
use crate::serial_println;

// ============================================================
// AHCI HBA レジスタ定義
// ============================================================
// AHCI 仕様 Section 3.1: Generic Host Control

/// HBA メモリレジスタ（BAR5 = ABAR にマップされる）。
/// AHCI spec Section 3.1 に基づくレジスタレイアウト。
///
/// このコントローラ全体のグローバルレジスタと、
/// 最大 32 ポート分のポートレジスタ配列で構成される。
#[repr(C)]
struct HbaMemory {
    /// 0x00: Host Capabilities — HBA がサポートする機能のビットマスク。
    /// bit [4:0] = ポート数 - 1、bit [8] = External SATA サポート、
    /// bit [30] = 64-bit DMA サポート、bit [31] = NCQ サポートなど。
    cap: u32,
    /// 0x04: Global Host Control — HBA 全体の制御レジスタ。
    /// bit [0] = HR (HBA Reset)、bit [1] = IE (Interrupt Enable)、
    /// bit [31] = AE (AHCI Enable)。
    ghc: u32,
    /// 0x08: Interrupt Status — 各ポートの割り込みステータス（ビットマスク）。
    /// ポート N の割り込みが発生すると bit N がセットされる。
    is: u32,
    /// 0x0C: Ports Implemented — 実装済みポートのビットマスク。
    /// bit N が 1 ならポート N が利用可能。
    pi: u32,
    /// 0x10: Version — AHCI バージョン（例: 0x00010301 = 1.3.1）。
    vs: u32,
    /// 0x14: Command Completion Coalescing Control
    _ccc_ctl: u32,
    /// 0x18: Command Completion Coalescing Ports
    _ccc_ports: u32,
    /// 0x1C: Enclosure Management Location
    _em_loc: u32,
    /// 0x20: Enclosure Management Control
    _em_ctl: u32,
    /// 0x24: Host Capabilities Extended
    _cap2: u32,
    /// 0x28: BIOS/OS Handoff Control and Status
    _bohc: u32,
    /// 0x2C〜0x9F: 予約
    _reserved: [u8; 0xA0 - 0x2C],
    /// 0xA0〜0xFF: ベンダー固有
    _vendor: [u8; 0x100 - 0xA0],
    /// 0x100〜: ポートレジスタ（各 0x80 バイト、最大 32 ポート）。
    /// ports[N] がポート N のレジスタに対応する。
    ports: [HbaPort; 32],
}

/// ポートレジスタ（各ポートに 1 つ、0x80 バイト）。
/// AHCI spec Section 3.3 に基づくレイアウト。
///
/// 各ポートは独立した SATA リンクを持ち、
/// Command List と FIS Receive Area を介してデバイスと通信する。
#[repr(C)]
struct HbaPort {
    /// 0x00: Command List Base Address (下位 32 ビット)。
    /// Command List の物理アドレス。1KB アラインが必要。
    clb: u32,
    /// 0x04: Command List Base Address (上位 32 ビット)。
    /// 64-bit DMA の場合に使用。
    clbu: u32,
    /// 0x08: FIS Base Address (下位 32 ビット)。
    /// FIS Receive Area の物理アドレス。256 バイトアラインが必要。
    fb: u32,
    /// 0x0C: FIS Base Address (上位 32 ビット)。
    fbu: u32,
    /// 0x10: Interrupt Status — このポートの割り込み要因。
    is: u32,
    /// 0x14: Interrupt Enable — 割り込み有効化マスク。
    ie: u32,
    /// 0x18: Command and Status — ポートの動作制御レジスタ。
    /// bit [0] = ST (Start)、bit [4] = FRE (FIS Receive Enable)、
    /// bit [14] = FR (FIS Receive Running)、bit [15] = CR (Command List Running)。
    cmd: u32,
    /// 0x1C: 予約
    _reserved0: u32,
    /// 0x20: Task File Data — 最新の D2H Register FIS の Status/Error。
    /// bit [7:0] = Status、bit [15:8] = Error。
    /// Status の bit [7] = BSY (Busy)、bit [3] = DRQ (Data Request)。
    tfd: u32,
    /// 0x24: Signature — デバイス種別を示す署名。
    /// IDENTIFY コマンドの応答から設定される。
    /// 0x00000101 = SATA ディスク、0xEB140101 = SATAPI デバイス。
    sig: u32,
    /// 0x28: SATA Status (SCR0: SStatus) — SATA リンクの状態。
    /// bit [3:0] = DET (Device Detection)：
    ///   0 = デバイスなし、1 = 検出中、3 = デバイス検出済み＋Phy通信確立。
    /// bit [7:4] = SPD (Speed)：1 = Gen1、2 = Gen2、3 = Gen3。
    ssts: u32,
    /// 0x2C: SATA Control (SCR2: SControl) — SATA リンク制御。
    sctl: u32,
    /// 0x30: SATA Error (SCR1: SError) — SATA エラーステータス。
    serr: u32,
    /// 0x34: SATA Active (SCR3: SActive) — NCQ 用アクティブタグ。
    sact: u32,
    /// 0x38: Command Issue — コマンド発行レジスタ。
    /// bit N を 1 にセットすると、Command List のスロット N が HBA に発行される。
    /// HBA がコマンドを完了すると bit N をクリアする。
    ci: u32,
    /// 0x3C: SATA Notification
    _sntf: u32,
    /// 0x40: FIS-based Switching Control
    _fbs: u32,
    /// 0x44: Device Sleep
    _devslp: u32,
    /// 0x48〜0x6F: 予約
    _reserved1: [u8; 0x70 - 0x48],
    /// 0x70〜0x7F: ベンダー固有
    _vendor: [u8; 0x80 - 0x70],
}

// ============================================================
// コマンド構造体定義
// ============================================================

/// Command List Header（コマンドリストの各スロット、32 バイト）。
/// AHCI spec Section 4.2.2。
///
/// HBA はこのヘッダーを読んで Command Table のアドレスや
/// 転送パラメータを取得する。
#[repr(C)]
#[derive(Clone, Copy)]
struct HbaCmdHeader {
    /// bit [4:0] = CFL (Command FIS Length in DWORDs)
    /// bit [5] = A (ATAPI)
    /// bit [6] = W (Write: 1=H2D データ転送、0=D2H)
    /// bit [7] = P (Prefetchable)
    /// bit [8] = R (Reset)
    /// bit [9] = B (BIST)
    /// bit [10] = C (Clear Busy upon R_OK)
    /// bit [15:12] = PMP (Port Multiplier Port)
    flags: u16,
    /// PRDT (Physical Region Descriptor Table) のエントリ数。
    /// Command Table 内の PRDT 配列の要素数を指定する。
    prdtl: u16,
    /// PRD Byte Count — HBA が転送完了後に書き込む実際の転送バイト数。
    /// コマンド発行前は 0 に初期化しておく。
    prdbc: u32,
    /// Command Table Base Address (下位 32 ビット)。
    /// 128 バイトアラインが必要。
    ctba: u32,
    /// Command Table Base Address (上位 32 ビット)。
    ctbau: u32,
    /// 予約（0 に初期化すること）
    _rsv: [u32; 4],
}

/// Command Table（コマンド FIS + ATAPI コマンド + PRDT）。
/// AHCI spec Section 4.2.3。
///
/// 実際の ATA コマンド FIS と、データ転送先の物理メモリ記述子（PRDT）を格納する。
/// PRDT エントリ数は最低 1 つ必要（読み書きコマンドの場合）。
/// 128 バイトアラインで配置する必要がある。
#[repr(C)]
struct HbaCmdTable {
    /// Command FIS (Frame Information Structure)。
    /// Register H2D FIS を格納する領域（最大 64 バイト）。
    cfis: [u8; 64],
    /// ATAPI Command（SCSI コマンドパケット）。
    /// ATAPI デバイスの場合に使用。SATA ディスクでは未使用。
    acmd: [u8; 16],
    /// 予約
    _rsv: [u8; 48],
    /// PRDT エントリ配列（最低 1 つ）。
    /// 各エントリがデータバッファの物理アドレスとサイズを記述する。
    prdt: [HbaPrdtEntry; 1],
}

/// PRDT エントリ（Physical Region Descriptor Table Entry、16 バイト）。
/// AHCI spec Section 4.2.3.3。
///
/// DMA 転送先/元の物理メモリ領域を記述する。
/// 1 エントリで最大 4MB (dbc = 0x3FFFFF) の転送が可能。
#[repr(C)]
#[derive(Clone, Copy)]
struct HbaPrdtEntry {
    /// Data Base Address (下位 32 ビット)。
    /// 転送先/元バッファの物理アドレス。2 バイトアラインが必要。
    dba: u32,
    /// Data Base Address (上位 32 ビット)。
    dbau: u32,
    /// 予約
    _rsv: u32,
    /// Data Byte Count — 転送バイト数 - 1。
    /// bit [21:0] = バイト数 - 1（最大 4MB）。
    /// bit [31] = I (Interrupt on Completion)。
    dbc: u32,
}

/// Register H2D FIS（ホスト→デバイス コマンド送信用、20 バイト）。
/// ATA/ATAPI-8 spec および AHCI spec Section 3.3.7。
///
/// ATA コマンド（IDENTIFY, READ DMA EXT, WRITE DMA EXT 等）を
/// デバイスに送信するための FIS 構造体。
#[repr(C)]
struct FisRegH2D {
    /// FIS Type — 0x27 = Register H2D FIS。
    fis_type: u8,
    /// bit [7] = C (Command/Control): 1 = コマンドレジスタ更新、0 = コントロールレジスタ更新。
    /// bit [3:0] = Port Multiplier Port。
    flags: u8,
    /// ATA コマンドコード。
    /// 0xEC = IDENTIFY DEVICE、0x25 = READ DMA EXT、0x35 = WRITE DMA EXT。
    command: u8,
    /// Feature レジスタ（下位 8 ビット）。コマンドによって用途が異なる。
    feature_lo: u8,

    /// LBA (Logical Block Address) の下位 24 ビット。
    /// lba0 = LBA [7:0], lba1 = LBA [15:8], lba2 = LBA [23:16]。
    lba0: u8,
    lba1: u8,
    lba2: u8,
    /// Device レジスタ。bit [6] = LBA モード（1 にセット）。
    device: u8,

    /// LBA の上位 24 ビット（48-bit LBA の場合）。
    /// lba3 = LBA [31:24], lba4 = LBA [39:32], lba5 = LBA [47:40]。
    lba3: u8,
    lba4: u8,
    lba5: u8,
    /// Feature レジスタ（上位 8 ビット）。
    feature_hi: u8,

    /// Sector Count（下位 8 ビット）。
    count_lo: u8,
    /// Sector Count（上位 8 ビット、48-bit LBA の場合）。
    count_hi: u8,
    /// ICC (Isochronous Command Completion)。通常は 0。
    _icc: u8,
    /// Control レジスタ。通常は 0。
    _control: u8,

    /// 予約（0 パディング）
    _rsv: [u8; 4],
}

// ============================================================
// ATA コマンド定数
// ============================================================

/// IDENTIFY DEVICE コマンド — デバイスの識別情報（モデル名、容量等）を取得する。
/// 512 バイトの応答データを返す。
const ATA_CMD_IDENTIFY: u8 = 0xEC;

/// READ DMA EXT コマンド — 48-bit LBA で DMA 読み取り。
/// セクタ数は Count レジスタで指定（0 = 65536 セクタ）。
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;

/// WRITE DMA EXT コマンド — 48-bit LBA で DMA 書き込み。
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;

/// Register H2D FIS の FIS Type 値。
const FIS_TYPE_REG_H2D: u8 = 0x27;

// ============================================================
// HBA ポートコマンド / ステータスビット
// ============================================================

/// CMD.ST — Start (コマンドリスト処理を開始する)。
const HBA_PORT_CMD_ST: u32 = 1 << 0;
/// CMD.FRE — FIS Receive Enable (FIS 受信を有効化する)。
const HBA_PORT_CMD_FRE: u32 = 1 << 4;
/// CMD.FR — FIS Receive Running (FIS 受信が動作中)。
const HBA_PORT_CMD_FR: u32 = 1 << 14;
/// CMD.CR — Command List Running (コマンドリスト処理が動作中)。
const HBA_PORT_CMD_CR: u32 = 1 << 15;

/// TFD.BSY — デバイスがビジー状態。
const HBA_PORT_TFD_BSY: u32 = 1 << 7;
/// TFD.DRQ — デバイスがデータ転送を要求中。
const HBA_PORT_TFD_DRQ: u32 = 1 << 3;

/// GHC.AE — AHCI Enable (AHCI モードを有効化する)。
const HBA_GHC_AE: u32 = 1 << 31;
/// GHC.HR — HBA Reset (ソフトリセットを発行する)。
/// 現在はリセットを省略しているが、実機対応時に必要になる可能性があるため残す。
#[allow(dead_code)]
const HBA_GHC_HR: u32 = 1 << 0;

/// SATA ディスクの署名値（Signature）。
/// IDENTIFY コマンド後にポートの SIG レジスタに設定される。
const SATA_SIG_DISK: u32 = 0x00000101;

// ============================================================
// グローバル状態
// ============================================================

/// グローバルな AHCI デバイスリスト。
/// init() で検出・初期化された AHCI ポート（SATA ディスク）が格納される。
/// 各要素は 1 つの SATA ディスクに対応する。
pub static AHCI_DEVICES: Mutex<Vec<AhciDevice>> = Mutex::new(Vec::new());

/// AHCI デバイス（1 つの SATA ディスクポートに対応）。
///
/// HBA の MMIO ベースアドレスとポート番号、
/// DMA 用のコマンド構造体メモリ、デバイス容量を保持する。
pub struct AhciDevice {
    /// HBA メモリレジスタの仮想アドレス（= 物理アドレス、アイデンティティマッピング）。
    hba: *mut HbaMemory,
    /// この SATA デバイスが接続されている HBA ポートのインデックス（0〜31）。
    port_index: u8,
    /// Command List の仮想アドレス。
    /// 32 スロット × 32 バイト = 1KB。1KB アラインで確保。
    cmd_list: *mut HbaCmdHeader,
    /// Command Table の仮想アドレス。
    /// 各スロットに 1 つの Command Table（256 バイト）。128 バイトアラインで確保。
    cmd_table: *mut HbaCmdTable,
    /// FIS Receive Area の仮想アドレス。
    /// 256 バイト。256 バイトアラインで確保。
    /// 現在はポーリング方式なので直接参照しないが、
    /// HBA がレスポンス FIS を書き込むため確保しておく必要がある。
    #[allow(dead_code)]
    fis_base: *mut u8,
    /// デバイスの総セクタ数（IDENTIFY DEVICE で取得、48-bit LBA）。
    capacity: u64,
}

// AhciDevice は raw pointer を含むが、Mutex で保護されるため Send/Sync は安全
unsafe impl Send for AhciDevice {}
unsafe impl Sync for AhciDevice {}

// ============================================================
// 初期化
// ============================================================

/// AHCI ドライバを初期化する。
///
/// PCI バスから AHCI コントローラを探し、見つかった各コントローラのポートを初期化する。
/// SATA ディスクが検出されたポートには IDENTIFY DEVICE コマンドを発行して
/// 容量とモデル名を取得する。
pub fn init() {
    let controllers = pci::find_ahci_controllers();
    if controllers.is_empty() {
        serial_println!("AHCI: no controllers found");
        return;
    }

    let mut devices = Vec::new();

    for ctrl in controllers {
        serial_println!(
            "AHCI: controller found at PCI {:02x}:{:02x}.{}",
            ctrl.bus, ctrl.device, ctrl.function
        );

        // PCI Command レジスタで Bus Master + Memory Space を有効化する。
        // Bus Master: HBA が DMA でメインメモリにアクセスするために必要。
        // Memory Space: BAR5 の MMIO 領域にアクセスするために必要。
        let cmd = pci::pci_config_read16(ctrl.bus, ctrl.device, ctrl.function, 0x04);
        // bit [1] = Memory Space Enable、bit [2] = Bus Master Enable
        pci::pci_config_write16(ctrl.bus, ctrl.device, ctrl.function, 0x04, cmd | 0x06);

        // BAR5 (ABAR = AHCI Base Address Register) を読み取る。
        // AHCI 仕様では BAR5 が HBA メモリレジスタの MMIO ベースアドレスを格納する。
        // BAR5 は 64-bit の場合もあるが、QEMU のエミュレーションでは通常 32-bit 範囲内。
        let bar5_raw = pci::read_bar(ctrl.bus, ctrl.device, ctrl.function, 5);
        if bar5_raw & 1 != 0 {
            // I/O ポートマップド（AHCI は MMIO であるべき）
            serial_println!("AHCI: BAR5 is I/O port, expected MMIO — skipping");
            continue;
        }
        // BAR のタイプビット ([2:1]) をチェック
        let bar_type = (bar5_raw >> 1) & 0x3;
        let abar = if bar_type == 0x02 {
            // 64-bit BAR: BAR5 と BAR6 (存在しないが read_bar64 で読む) を結合
            // 実際には BAR5 のインデックスは 5 なので BAR5+BAR6 を読む
            // 注意: BAR は 0-indexed で BAR5 = offset 0x24
            // read_bar64 は bar_index と bar_index+1 を読む
            pci::read_bar64(ctrl.bus, ctrl.device, ctrl.function, 5)
        } else {
            // 32-bit BAR: 下位 4 ビットをマスクしてベースアドレスを取得
            (bar5_raw & !0xF) as u64
        };

        if abar == 0 {
            serial_println!("AHCI: BAR5 is zero — skipping");
            continue;
        }

        serial_println!("AHCI: ABAR = {:#x}", abar);

        // ABAR を HbaMemory として使用する。
        // SABOS はアイデンティティマッピングなので物理アドレス = 仮想アドレス。
        let hba = abar as *mut HbaMemory;

        // HBA の初期化シーケンス（AHCI spec Section 10.1.2）:
        // 1. GHC.AE をセットして AHCI モードを有効化
        // 2. 必要に応じて HBA Reset
        // 3. PI を読んでアクティブポートを列挙

        unsafe {
            // 1. AHCI Enable
            let ghc = core::ptr::read_volatile(&(*hba).ghc);
            core::ptr::write_volatile(&mut (*hba).ghc, ghc | HBA_GHC_AE);

            // HBA Reset は省略する。
            // QEMU では初期状態で正常に動作する。
            // 実機でも UEFI が既に初期化済みの場合が多い。
            // リセットが必要な場合は以下のコードを有効化する:
            //   core::ptr::write_volatile(&mut (*hba).ghc, HBA_GHC_HR);
            //   while core::ptr::read_volatile(&(*hba).ghc) & HBA_GHC_HR != 0 { spin_loop(); }
            //   core::ptr::write_volatile(&mut (*hba).ghc, HBA_GHC_AE);

            // バージョンを表示
            let version = core::ptr::read_volatile(&(*hba).vs);
            serial_println!(
                "AHCI: version {}.{}{}",
                (version >> 16) & 0xFFFF,
                (version >> 8) & 0xFF,
                if version & 0xFF != 0 {
                    // マイナーバージョンがあれば表示
                    alloc::format!(".{}", version & 0xFF)
                } else {
                    alloc::string::String::new()
                }
            );

            // PI (Ports Implemented) を読んでアクティブポートを列挙
            let pi = core::ptr::read_volatile(&(*hba).pi);
            serial_println!("AHCI: ports implemented = {:#010x}", pi);

            // 各ポートをチェック
            for port_idx in 0..32u8 {
                if pi & (1 << port_idx) == 0 {
                    // このポートは実装されていない
                    continue;
                }

                // SATA Status (SSTS) を確認
                let ssts = core::ptr::read_volatile(&(*hba).ports[port_idx as usize].ssts);
                let det = ssts & 0xF; // Device Detection
                if det != 3 {
                    // DET=3: デバイス検出済み＋Phy通信確立
                    // それ以外はデバイスが接続されていないか、まだ初期化中
                    continue;
                }

                // Signature を確認
                let sig = core::ptr::read_volatile(&(*hba).ports[port_idx as usize].sig);
                if sig != SATA_SIG_DISK {
                    serial_println!(
                        "AHCI: port {}: non-disk device (sig={:#010x}), skipping",
                        port_idx, sig
                    );
                    continue;
                }

                serial_println!("AHCI: port {}: SATA disk detected", port_idx);

                // ポートを初期化
                if let Some(dev) = init_port(hba, port_idx) {
                    devices.push(dev);
                }
            }
        }
    }

    if devices.is_empty() {
        serial_println!("AHCI: no SATA disks found");
    } else {
        serial_println!("AHCI: {} SATA disk(s) initialized", devices.len());
    }

    *AHCI_DEVICES.lock() = devices;
}

/// 検出された AHCI デバイスの数を返す。
pub fn device_count() -> usize {
    AHCI_DEVICES.lock().len()
}

/// 個別のポートを初期化する。
///
/// 1. ポートをアイドル状態にする（ST, FRE をクリアして CR, FR の停止を待つ）
/// 2. Command List, FIS Receive Area, Command Table のメモリを確保
/// 3. CLB, FB レジスタにアドレスを設定
/// 4. FRE, ST を有効化
/// 5. IDENTIFY DEVICE コマンドを発行してデバイス情報を取得
///
/// # Safety
/// `hba` は有効な HbaMemory MMIO 領域を指していること。
/// `port_idx` は PI レジスタで有効なポートであること。
fn init_port(hba: *mut HbaMemory, port_idx: u8) -> Option<AhciDevice> {
    unsafe {
        let port = &mut (*hba).ports[port_idx as usize];

        // --- ポートをアイドル状態にする ---
        // AHCI spec Section 10.1.2: ポートの初期化前に ST と FRE をクリアし、
        // CR (Command List Running) と FR (FIS Receive Running) が 0 になるのを待つ。
        stop_port(port);

        // --- DMA 用メモリの確保 ---
        // Command List: 32 スロット × 32 バイト = 1024 バイト。1KB アライン。
        let cmd_list_layout = Layout::from_size_align(1024, 1024).unwrap();
        let cmd_list_ptr = alloc::alloc::alloc_zeroed(cmd_list_layout);
        if cmd_list_ptr.is_null() {
            serial_println!("AHCI: port {}: failed to allocate Command List", port_idx);
            return None;
        }

        // FIS Receive Area: 256 バイト。256 バイトアライン。
        let fis_layout = Layout::from_size_align(256, 256).unwrap();
        let fis_ptr = alloc::alloc::alloc_zeroed(fis_layout);
        if fis_ptr.is_null() {
            serial_println!("AHCI: port {}: failed to allocate FIS area", port_idx);
            alloc::alloc::dealloc(cmd_list_ptr, cmd_list_layout);
            return None;
        }

        // Command Table: 128 バイト (CFIS+ACMD+RSV) + 16 バイト (PRDT × 1) = 144 バイト。
        // 余裕を見て 256 バイト確保。128 バイトアライン。
        let cmd_table_layout = Layout::from_size_align(256, 128).unwrap();
        let cmd_table_ptr = alloc::alloc::alloc_zeroed(cmd_table_layout);
        if cmd_table_ptr.is_null() {
            serial_println!("AHCI: port {}: failed to allocate Command Table", port_idx);
            alloc::alloc::dealloc(cmd_list_ptr, cmd_list_layout);
            alloc::alloc::dealloc(fis_ptr, fis_layout);
            return None;
        }

        // --- CLB, FB レジスタにアドレスを設定 ---
        // アイデンティティマッピングなので仮想アドレス = 物理アドレス。
        let cmd_list_phys = cmd_list_ptr as u64;
        let fis_phys = fis_ptr as u64;

        core::ptr::write_volatile(&mut port.clb, cmd_list_phys as u32);
        core::ptr::write_volatile(&mut port.clbu, (cmd_list_phys >> 32) as u32);
        core::ptr::write_volatile(&mut port.fb, fis_phys as u32);
        core::ptr::write_volatile(&mut port.fbu, (fis_phys >> 32) as u32);

        // SERR (SATA Error) レジスタをクリア（全ビット Write-1-to-Clear）
        core::ptr::write_volatile(&mut port.serr, 0xFFFFFFFF);
        // IS (Interrupt Status) をクリア
        core::ptr::write_volatile(&mut port.is, 0xFFFFFFFF);

        // --- FRE を有効化して FIS 受信を開始 ---
        let cmd = core::ptr::read_volatile(&port.cmd);
        core::ptr::write_volatile(&mut port.cmd, cmd | HBA_PORT_CMD_FRE);

        // --- ST を有効化してコマンドリスト処理を開始 ---
        let cmd = core::ptr::read_volatile(&port.cmd);
        core::ptr::write_volatile(&mut port.cmd, cmd | HBA_PORT_CMD_ST);

        // --- Command List のスロット 0 に Command Table アドレスを設定 ---
        let cmd_header = cmd_list_ptr as *mut HbaCmdHeader;
        let cmd_table_phys = cmd_table_ptr as u64;
        (*cmd_header).ctba = cmd_table_phys as u32;
        (*cmd_header).ctbau = (cmd_table_phys >> 32) as u32;

        let mut dev = AhciDevice {
            hba,
            port_index: port_idx,
            cmd_list: cmd_list_ptr as *mut HbaCmdHeader,
            cmd_table: cmd_table_ptr as *mut HbaCmdTable,
            fis_base: fis_ptr,
            capacity: 0,
        };

        // --- IDENTIFY DEVICE コマンドを発行 ---
        match dev.identify() {
            Ok((capacity, model)) => {
                dev.capacity = capacity;
                let size_mib = capacity * 512 / 1024 / 1024;
                serial_println!(
                    "AHCI: port {}: {}, {} sectors ({} MiB)",
                    port_idx, model, capacity, size_mib
                );
                crate::kprintln!(
                    "  [AHCI port {}] {} ({} MiB)",
                    port_idx, model, size_mib
                );
                Some(dev)
            }
            Err(e) => {
                serial_println!("AHCI: port {}: IDENTIFY failed: {}", port_idx, e);
                None
            }
        }
    }
}

/// ポートをアイドル状態にする（ST, FRE をクリアし、CR, FR の停止を待つ）。
///
/// AHCI spec Section 10.1.2:
/// ST をクリア → CR が 0 になるまで待つ（最大 500ms）。
/// FRE をクリア → FR が 0 になるまで待つ（最大 500ms）。
/// # Safety
/// `port` は有効な HbaPort MMIO 領域を指していること。
fn stop_port(port: &mut HbaPort) {
    unsafe {
        let mut cmd = core::ptr::read_volatile(&port.cmd);

        // ST (Start) をクリア
        if cmd & HBA_PORT_CMD_ST != 0 {
            cmd &= !HBA_PORT_CMD_ST;
            core::ptr::write_volatile(&mut port.cmd, cmd);
        }

        // CR (Command List Running) が 0 になるまで待つ
        for _ in 0..1_000_000 {
            if core::ptr::read_volatile(&port.cmd) & HBA_PORT_CMD_CR == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // FRE (FIS Receive Enable) をクリア
        if cmd & HBA_PORT_CMD_FRE != 0 {
            cmd = core::ptr::read_volatile(&port.cmd);
            cmd &= !HBA_PORT_CMD_FRE;
            core::ptr::write_volatile(&mut port.cmd, cmd);
        }

        // FR (FIS Receive Running) が 0 になるまで待つ
        for _ in 0..1_000_000 {
            if core::ptr::read_volatile(&port.cmd) & HBA_PORT_CMD_FR == 0 {
                break;
            }
            core::hint::spin_loop();
        }
    }
}

// ============================================================
// コマンド発行
// ============================================================

impl AhciDevice {
    /// IDENTIFY DEVICE コマンドを発行し、デバイスの容量とモデル名を取得する。
    ///
    /// ATA コマンド 0xEC を使い、512 バイトの識別情報データを読み取る。
    /// 返り値: (セクタ数, モデル名文字列)
    fn identify(&mut self) -> Result<(u64, alloc::string::String), &'static str> {
        // IDENTIFY レスポンス用の 512 バイトバッファ
        let mut identify_buf = [0u8; 512];

        // Command Header を設定
        let cmd_header = unsafe { &mut *self.cmd_list };
        // CFL = 5 DWORDs (Register H2D FIS は 5 DWORD = 20 バイト)
        // W = 0 (D2H データ転送 = デバイスからホストへ)
        cmd_header.flags = 5; // CFL=5, W=0
        cmd_header.prdtl = 1; // PRDT エントリ 1 つ
        cmd_header.prdbc = 0; // 転送バイト数は HBA が書き込む

        // Command Table をクリアして設定
        let cmd_table = unsafe { &mut *self.cmd_table };
        // CFIS, ACMD, RSV をゼロクリア
        cmd_table.cfis.fill(0);
        cmd_table.acmd.fill(0);
        cmd_table._rsv.fill(0);

        // Register H2D FIS を構築
        let fis = cmd_table.cfis.as_mut_ptr() as *mut FisRegH2D;
        unsafe {
            (*fis).fis_type = FIS_TYPE_REG_H2D;
            (*fis).flags = 0x80; // bit [7] = C (Command レジスタ更新)
            (*fis).command = ATA_CMD_IDENTIFY;
            (*fis).device = 0; // IDENTIFY では device = 0
        }

        // PRDT エントリを設定（identify_buf の物理アドレスを指定）
        let buf_phys = identify_buf.as_mut_ptr() as u64;
        cmd_table.prdt[0].dba = buf_phys as u32;
        cmd_table.prdt[0].dbau = (buf_phys >> 32) as u32;
        cmd_table.prdt[0]._rsv = 0;
        // dbc = バイト数 - 1 = 511
        cmd_table.prdt[0].dbc = 511;

        // コマンドを発行して完了を待つ
        self.issue_command_slot0()?;

        // IDENTIFY レスポンスから情報を取得
        // Word 100-103: 48-bit LBA アドレス可能なセクタ数（リトルエンディアン）
        let capacity = u64::from_le_bytes([
            identify_buf[200], identify_buf[201], identify_buf[202], identify_buf[203],
            identify_buf[204], identify_buf[205], identify_buf[206], identify_buf[207],
        ]);

        // Word 60-61 のフォールバック（48-bit LBA 未対応の古いドライブ用）
        let capacity = if capacity == 0 {
            let lo = u16::from_le_bytes([identify_buf[120], identify_buf[121]]) as u64;
            let hi = u16::from_le_bytes([identify_buf[122], identify_buf[123]]) as u64;
            (hi << 16) | lo
        } else {
            capacity
        };

        // Word 27-46: モデル名 (40 文字、ATA バイトスワップ: 各 Word 内で上位/下位バイトが逆)
        let mut model_bytes = [0u8; 40];
        for i in 0..20 {
            let word_offset = (27 + i) * 2; // Word 27 は byte offset 54
            // ATA ではモデル名の各 Word 内でバイトが入れ替わっている
            model_bytes[i * 2] = identify_buf[word_offset + 1];
            model_bytes[i * 2 + 1] = identify_buf[word_offset];
        }
        // ASCII 文字列として解釈し、末尾のスペースを除去
        let model = core::str::from_utf8(&model_bytes)
            .unwrap_or("(unknown)")
            .trim()
            .into();

        Ok((capacity, model))
    }

    /// 指定セクタからデータを読み取る（READ DMA EXT）。
    ///
    /// ATA コマンド 0x25 を使い、1 セクタ (512 バイト) を読み取る。
    /// buf は 512 バイト以上であること。
    pub fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        if sector >= self.capacity {
            return Err("AHCI: sector out of range");
        }
        if buf.len() < 512 {
            return Err("AHCI: buffer too small");
        }

        // Command Header を設定
        let cmd_header = unsafe { &mut *self.cmd_list };
        cmd_header.flags = 5; // CFL=5, W=0 (読み取り)
        cmd_header.prdtl = 1;
        cmd_header.prdbc = 0;

        // Command Table をクリアして FIS を構築
        let cmd_table = unsafe { &mut *self.cmd_table };
        cmd_table.cfis.fill(0);

        let fis = cmd_table.cfis.as_mut_ptr() as *mut FisRegH2D;
        unsafe {
            (*fis).fis_type = FIS_TYPE_REG_H2D;
            (*fis).flags = 0x80; // C = 1 (コマンド)
            (*fis).command = ATA_CMD_READ_DMA_EXT;
            (*fis).device = 0x40; // LBA モード

            // 48-bit LBA を設定
            (*fis).lba0 = (sector & 0xFF) as u8;
            (*fis).lba1 = ((sector >> 8) & 0xFF) as u8;
            (*fis).lba2 = ((sector >> 16) & 0xFF) as u8;
            (*fis).lba3 = ((sector >> 24) & 0xFF) as u8;
            (*fis).lba4 = ((sector >> 32) & 0xFF) as u8;
            (*fis).lba5 = ((sector >> 40) & 0xFF) as u8;

            // セクタ数 = 1
            (*fis).count_lo = 1;
            (*fis).count_hi = 0;
        }

        // PRDT エントリを設定
        let buf_phys = buf.as_mut_ptr() as u64;
        cmd_table.prdt[0].dba = buf_phys as u32;
        cmd_table.prdt[0].dbau = (buf_phys >> 32) as u32;
        cmd_table.prdt[0]._rsv = 0;
        cmd_table.prdt[0].dbc = 511; // 512 - 1

        self.issue_command_slot0()
    }

    /// 指定セクタにデータを書き込む（WRITE DMA EXT）。
    ///
    /// ATA コマンド 0x35 を使い、1 セクタ (512 バイト) を書き込む。
    /// buf は 512 バイト以上であること。
    pub fn write_sector(&mut self, sector: u64, buf: &[u8]) -> Result<(), &'static str> {
        if sector >= self.capacity {
            return Err("AHCI: sector out of range");
        }
        if buf.len() < 512 {
            return Err("AHCI: buffer too small");
        }

        // Command Header を設定
        let cmd_header = unsafe { &mut *self.cmd_list };
        // CFL=5, W=1 (書き込み: ホスト→デバイス方向)
        cmd_header.flags = 5 | (1 << 6); // bit [6] = W
        cmd_header.prdtl = 1;
        cmd_header.prdbc = 0;

        // Command Table をクリアして FIS を構築
        let cmd_table = unsafe { &mut *self.cmd_table };
        cmd_table.cfis.fill(0);

        let fis = cmd_table.cfis.as_mut_ptr() as *mut FisRegH2D;
        unsafe {
            (*fis).fis_type = FIS_TYPE_REG_H2D;
            (*fis).flags = 0x80; // C = 1 (コマンド)
            (*fis).command = ATA_CMD_WRITE_DMA_EXT;
            (*fis).device = 0x40; // LBA モード

            // 48-bit LBA を設定
            (*fis).lba0 = (sector & 0xFF) as u8;
            (*fis).lba1 = ((sector >> 8) & 0xFF) as u8;
            (*fis).lba2 = ((sector >> 16) & 0xFF) as u8;
            (*fis).lba3 = ((sector >> 24) & 0xFF) as u8;
            (*fis).lba4 = ((sector >> 32) & 0xFF) as u8;
            (*fis).lba5 = ((sector >> 40) & 0xFF) as u8;

            // セクタ数 = 1
            (*fis).count_lo = 1;
            (*fis).count_hi = 0;
        }

        // PRDT エントリを設定
        let buf_phys = buf.as_ptr() as u64;
        cmd_table.prdt[0].dba = buf_phys as u32;
        cmd_table.prdt[0].dbau = (buf_phys >> 32) as u32;
        cmd_table.prdt[0]._rsv = 0;
        cmd_table.prdt[0].dbc = 511; // 512 - 1

        self.issue_command_slot0()
    }

    /// デバイスの容量（セクタ数）を返す。
    #[allow(dead_code)]
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Command List のスロット 0 のコマンドを発行し、完了をポーリングで待つ。
    ///
    /// 事前条件: cmd_list[0] と cmd_table が正しく設定されていること。
    /// ポートの TFD (Task File Data) が BSY/DRQ でないことを確認してから CI をセットする。
    fn issue_command_slot0(&mut self) -> Result<(), &'static str> {
        let port = unsafe { &mut (*self.hba).ports[self.port_index as usize] };

        // デバイスがビジーでないことを確認する。
        // BSY または DRQ がセットされている場合は前のコマンドが完了していない。
        let mut spin = 0u64;
        loop {
            let tfd = unsafe { core::ptr::read_volatile(&port.tfd) };
            if tfd & (HBA_PORT_TFD_BSY | HBA_PORT_TFD_DRQ) == 0 {
                break;
            }
            spin += 1;
            if spin > 100_000_000 {
                return Err("AHCI: device busy timeout");
            }
            core::hint::spin_loop();
        }

        // メモリバリア: コマンド構造体の書き込みが CI セットより先に完了することを保証
        fence(Ordering::SeqCst);

        // IS (Interrupt Status) をクリア
        unsafe {
            core::ptr::write_volatile(&mut port.is, 0xFFFFFFFF);
        }

        // CI (Command Issue) のビット 0 をセットしてコマンドを発行
        unsafe {
            core::ptr::write_volatile(&mut port.ci, 1);
        }

        // メモリバリア: CI の書き込みが確実に行われることを保証
        fence(Ordering::SeqCst);

        // CI ビット 0 がクリアされるまでポーリング（HBA がコマンドを完了するとクリアする）
        let mut spin = 0u64;
        loop {
            fence(Ordering::SeqCst);
            let ci = unsafe { core::ptr::read_volatile(&port.ci) };
            if ci & 1 == 0 {
                break;
            }

            // TFD のエラーチェック
            let tfd = unsafe { core::ptr::read_volatile(&port.tfd) };
            if tfd & 1 != 0 {
                // TFD.STS.ERR (bit 0) がセットされている → コマンドエラー
                return Err("AHCI: command error (TFD.ERR)");
            }

            spin += 1;
            if spin > 100_000_000 {
                return Err("AHCI: command timeout");
            }
            core::hint::spin_loop();
        }

        // 最終的な TFD エラーチェック
        let tfd = unsafe { core::ptr::read_volatile(&port.tfd) };
        if tfd & 1 != 0 {
            return Err("AHCI: command completed with error");
        }

        Ok(())
    }
}
