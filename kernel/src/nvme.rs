// nvme.rs — NVMe (Non-Volatile Memory Express) ドライバ
//
// NVMe は PCIe ネイティブのストレージインターフェース。
// AHCI (SATA) と比べて低レイテンシ・高スループットが特長。
// 最近の PC では NVMe SSD が主流であり、実機対応には必須。
//
// ## NVMe の基本アーキテクチャ
//
// NVMe コントローラは PCI デバイスとして存在し、
// BAR0 に MMIO レジスタがマップされる（AHCI の BAR5 とは異なる）。
//
// 通信はキューベース:
//   - Submission Queue (SQ): ホスト → コントローラにコマンドを投入（64 バイト/エントリ）
//   - Completion Queue (CQ): コントローラ → ホストに結果を返す（16 バイト/エントリ）
//   - Doorbell レジスタ: SQ/CQ のポインタ更新をコントローラに通知する MMIO レジスタ
//
// キューは 2 種類:
//   - Admin Queue (AQ): コントローラ管理用（Identify, Create I/O Queue 等）
//   - I/O Queue: データ転送用（Read, Write）
//
// ## コマンド発行の流れ
//
// 1. SQ にコマンド (SQE, 64 bytes) を書き込む
// 2. SQ Tail Doorbell に新しい Tail 値を書き込む → コントローラがコマンドを処理開始
// 3. CQ をポーリング（Phase Tag ビットで新しい CQE を検出）
// 4. CQ Head Doorbell に新しい Head 値を書き込む → コントローラに消費を通知
//
// ## 現在の実装
//
// - ポーリング方式（割り込みは使わない）
// - Admin Queue + I/O Queue 各 1 組
// - Read/Write は 1 セクタ (512 バイト) 単位
// - PRP (Physical Region Page) は PRP1 のみ使用（4KB 以内の転送）

use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{fence, Ordering};
use spin::Mutex;
use crate::pci;
use crate::serial_println;

// ============================================================
// NVMe コントローラレジスタ定義 (BAR0)
// ============================================================
// NVMe 1.0 仕様 Section 3: Controller Registers

// CAP レジスタのフィールド
/// CAP.MQES — Maximum Queue Entries Supported (0-based, bits [15:0])。
/// コントローラがサポートする最大キューエントリ数 - 1。
/// 例: MQES=63 なら最大 64 エントリ。
const CAP_MQES_MASK: u64 = 0xFFFF;

/// CAP.DSTRD — Doorbell Stride (bits [35:32])。
/// Doorbell レジスタ間のストライド。実際のバイト数は 4 × (2^DSTRD)。
/// DSTRD=0 なら 4 バイト間隔（最小）。
const CAP_DSTRD_SHIFT: u64 = 32;
const CAP_DSTRD_MASK: u64 = 0xF;

// CC (Controller Configuration) レジスタのフィールド
/// CC.EN — Enable (bit 0)。1 でコントローラを有効化する。
const CC_EN: u32 = 1 << 0;
/// CC.CSS — Command Set Selected (bits [6:4])。0 = NVM Command Set。
const CC_CSS_NVM: u32 = 0 << 4;
/// CC.MPS — Memory Page Size (bits [10:7])。0 = 4KB (2^(12+0))。
const CC_MPS_4K: u32 = 0 << 7;
/// CC.IOSQES — I/O Submission Queue Entry Size (bits [19:16])。
/// 6 = 2^6 = 64 バイト（NVMe 仕様の固定値）。
const CC_IOSQES_64: u32 = 6 << 16;
/// CC.IOCQES — I/O Completion Queue Entry Size (bits [23:20])。
/// 4 = 2^4 = 16 バイト（NVMe 仕様の固定値）。
const CC_IOCQES_16: u32 = 4 << 20;

// CSTS (Controller Status) レジスタのフィールド
/// CSTS.RDY — Ready (bit 0)。コントローラが動作可能になると 1 になる。
const CSTS_RDY: u32 = 1 << 0;
/// CSTS.CFS — Controller Fatal Status (bit 1)。致命的エラーが発生すると 1 になる。
const CSTS_CFS: u32 = 1 << 1;

// Admin コマンド opcodes
/// Identify コマンド (Admin opcode 0x06)。
/// コントローラ情報やネームスペース情報を取得する。
const ADMIN_IDENTIFY: u8 = 0x06;
/// Create I/O Completion Queue (Admin opcode 0x05)。
const ADMIN_CREATE_IO_CQ: u8 = 0x05;
/// Create I/O Submission Queue (Admin opcode 0x01)。
const ADMIN_CREATE_IO_SQ: u8 = 0x01;

// I/O コマンド opcodes
/// NVM Read コマンド (I/O opcode 0x02)。
const IO_CMD_READ: u8 = 0x02;
/// NVM Write コマンド (I/O opcode 0x01)。
const IO_CMD_WRITE: u8 = 0x01;

// Identify CNS (Controller or Namespace Structure) 値
/// CNS=1: Identify Controller — コントローラ情報を取得。
const IDENTIFY_CNS_CONTROLLER: u32 = 1;
/// CNS=0: Identify Namespace — ネームスペース情報を取得。
const IDENTIFY_CNS_NAMESPACE: u32 = 0;

/// キューのエントリ数。Admin Queue と I/O Queue で共通。
/// 小さめにして物理メモリの消費を抑える。
const QUEUE_DEPTH: u16 = 64;

/// リトライ回数の上限。
/// 実機では NVMe コントローラの一時的なエラーが起きうるため、
/// 3 回までリトライすることで一時的エラーを吸収する。
const IO_RETRY_COUNT: u32 = 3;

/// リトライ間のスピンウェイト回数（約 1ms 相当）。
const IO_RETRY_SPIN_WAIT: u32 = 100_000;

// ============================================================
// Submission Queue Entry (SQE, 64 bytes)
// ============================================================
// NVMe 1.0 仕様 Figure 11: Command Format

/// NVMe Submission Queue Entry (64 バイト)。
/// すべての NVMe コマンドはこの形式で SQ に投入する。
#[repr(C)]
#[derive(Clone, Copy)]
struct NvmeSqe {
    /// Opcode (8bit) — コマンド種別。Admin/I/O で異なる体系。
    opcode: u8,
    /// Flags (8bit) — bit [1:0] = Fused Operation, bit [7:6] = PRP or SGL。
    /// 通常は 0 (PRP 使用、非 Fused)。
    flags: u8,
    /// Command ID (16bit) — コマンドの識別子。CQE の cid と対応する。
    cid: u16,
    /// Namespace ID (32bit) — 対象のネームスペース。
    /// Admin コマンドでは 0 またはネームスペース番号、I/O コマンドでは通常 1。
    nsid: u32,
    /// 予約フィールド (64bit × 2)。
    _rsvd: [u64; 2],
    /// PRP Entry 1 — データバッファの物理アドレス（先頭 4KB 分）。
    prp1: u64,
    /// PRP Entry 2 — 4KB を超えるデータの場合、次のページの物理アドレス or PRP List。
    /// 512 バイト転送では使わない。
    prp2: u64,
    /// Command Dword 10 — コマンド固有パラメータ。
    cdw10: u32,
    /// Command Dword 11 — コマンド固有パラメータ。
    cdw11: u32,
    /// Command Dword 12 — コマンド固有パラメータ。
    cdw12: u32,
    /// Command Dword 13 — コマンド固有パラメータ。
    cdw13: u32,
    /// Command Dword 14 — コマンド固有パラメータ。
    cdw14: u32,
    /// Command Dword 15 — コマンド固有パラメータ。
    cdw15: u32,
}

// SQE のゼロ初期化値
impl NvmeSqe {
    fn zeroed() -> Self {
        Self {
            opcode: 0, flags: 0, cid: 0, nsid: 0,
            _rsvd: [0; 2],
            prp1: 0, prp2: 0,
            cdw10: 0, cdw11: 0, cdw12: 0, cdw13: 0, cdw14: 0, cdw15: 0,
        }
    }
}

// ============================================================
// Completion Queue Entry (CQE, 16 bytes)
// ============================================================
// NVMe 1.0 仕様 Figure 33: Completion Queue Entry

/// NVMe Completion Queue Entry (16 バイト)。
/// コントローラがコマンド完了時に CQ に書き込む。
#[repr(C)]
#[derive(Clone, Copy)]
struct NvmeCqe {
    /// DW0 — コマンド固有の結果値。
    dw0: u32,
    /// DW1 — 予約。
    _rsvd: u32,
    /// SQ Head Pointer — コントローラが処理した SQ の Head 位置。
    sq_head: u16,
    /// SQ Identifier — どの SQ のコマンドに対する応答か。
    sq_id: u16,
    /// Command ID — 対応する SQE の cid。
    cid: u16,
    /// Status — bit [0] = Phase Tag, bits [15:1] = Status Code。
    /// Phase Tag はキューの巡回を検出するためのビット。
    /// Status Code が 0 なら成功。
    status: u16,
}

// ============================================================
// NVMe キュー管理構造体
// ============================================================

/// NVMe のキューペア（Submission Queue + Completion Queue）を管理する構造体。
/// Admin Queue と I/O Queue の両方をこの構造体で表す。
struct NvmeQueue {
    /// Submission Queue のベースアドレス（物理 = 仮想、アイデンティティマッピング）。
    sq_base: *mut NvmeSqe,
    /// Completion Queue のベースアドレス。
    cq_base: *mut NvmeCqe,
    /// キューの深さ（エントリ数）。
    depth: u16,
    /// SQ の Tail インデックス（次にコマンドを書き込む位置）。
    sq_tail: u16,
    /// CQ の Head インデックス（次に読み取る位置）。
    cq_head: u16,
    /// Phase Tag — CQE の status bit [0] と比較して新しいエントリかどうか判定する。
    /// キューが一巡するたびに反転する（0 → 1 → 0 → ...）。
    phase: bool,
    /// SQ Tail Doorbell の MMIO アドレス。
    sq_doorbell: *mut u32,
    /// CQ Head Doorbell の MMIO アドレス。
    cq_doorbell: *mut u32,
    /// 次に割り当てる Command ID。
    next_cid: u16,
}

/// NvmeQueue は raw pointer を含むが、Mutex で保護されるため Send/Sync は安全
unsafe impl Send for NvmeQueue {}
unsafe impl Sync for NvmeQueue {}

impl NvmeQueue {
    /// コマンドを SQ に投入する。
    ///
    /// SQE を SQ の Tail 位置に書き込み、Tail Doorbell を更新する。
    /// 返り値は割り当てた Command ID（CQE との照合に使う）。
    fn submit(&mut self, mut sqe: NvmeSqe) -> u16 {
        let cid = self.next_cid;
        sqe.cid = cid;
        self.next_cid = self.next_cid.wrapping_add(1);

        // SQ の Tail 位置に SQE を書き込む
        unsafe {
            core::ptr::write_volatile(self.sq_base.add(self.sq_tail as usize), sqe);
        }

        // Tail を進める（巡回）
        self.sq_tail = (self.sq_tail + 1) % self.depth;

        // メモリバリア: SQE の書き込みが Doorbell 更新より先に完了することを保証
        fence(Ordering::SeqCst);

        // SQ Tail Doorbell を更新してコントローラに新しいコマンドを通知
        unsafe {
            core::ptr::write_volatile(self.sq_doorbell, self.sq_tail as u32);
        }

        cid
    }

    /// CQ をポーリングして完了を待つ。
    ///
    /// Phase Tag ビットで新しい CQE を検出する。
    /// NVMe の CQ には Phase Tag という仕組みがあり、
    /// キューが一巡するたびに Phase が反転する。
    /// CQE の status bit [0] が現在の期待 Phase と一致すれば新しいエントリ。
    ///
    /// タイムアウト付きポーリング。成功時は CQE を返す。
    fn poll_completion(&mut self) -> Result<NvmeCqe, &'static str> {
        for _ in 0..100_000_000u64 {
            fence(Ordering::SeqCst);
            let cqe = unsafe {
                core::ptr::read_volatile(self.cq_base.add(self.cq_head as usize))
            };

            // Phase Tag チェック: CQE の status bit [0] と現在の期待 Phase を比較
            let phase_bit = (cqe.status & 1) != 0;
            if phase_bit != self.phase {
                // まだ新しい CQE が書き込まれていない → 待ち続ける
                core::hint::spin_loop();
                continue;
            }

            // 新しい CQE を検出!

            // CQ Head を進める
            self.cq_head = (self.cq_head + 1) % self.depth;
            // キューが一巡したら Phase を反転
            if self.cq_head == 0 {
                self.phase = !self.phase;
            }

            // CQ Head Doorbell を更新してコントローラに消費を通知
            fence(Ordering::SeqCst);
            unsafe {
                core::ptr::write_volatile(self.cq_doorbell, self.cq_head as u32);
            }

            // Status Code をチェック（bits [15:1]）
            let status_code = (cqe.status >> 1) & 0x7FF;
            if status_code != 0 {
                serial_println!("NVMe: command error: status_code={:#x}, cid={}", status_code, cqe.cid);
                return Err("NVMe: command error");
            }

            return Ok(cqe);
        }

        Err("NVMe: command timeout")
    }
}

// ============================================================
// NVMe デバイス構造体
// ============================================================

/// NVMe デバイス（1 つの NVMe コントローラに対応）。
pub struct NvmeDevice {
    /// BAR0 の MMIO ベースアドレス。
    bar0: u64,
    /// Doorbell Stride (バイト数)。各 Doorbell レジスタ間のバイト間隔。
    /// 計算式: 4 × 2^DSTRD。DSTRD=0 なら 4 バイト。
    doorbell_stride: u32,
    /// Admin Queue。コントローラ管理コマンド用。
    admin_queue: NvmeQueue,
    /// I/O Queue。データ転送用。None なら未作成。
    io_queue: Option<NvmeQueue>,
    /// ネームスペース 1 のサイズ（論理ブロック数）。
    ns_size: u64,
    /// ネームスペース 1 の論理ブロックサイズ（バイト、通常 512 or 4096）。
    block_size: u32,
}

/// NvmeDevice は raw pointer を含むが、Mutex で保護されるため Send/Sync は安全
unsafe impl Send for NvmeDevice {}
unsafe impl Sync for NvmeDevice {}

// ============================================================
// グローバル状態
// ============================================================

/// グローバルな NVMe デバイスリスト。
/// init() で検出・初期化された NVMe コントローラが格納される。
pub static NVME_DEVICES: Mutex<Vec<NvmeDevice>> = Mutex::new(Vec::new());

// ============================================================
// Doorbell レジスタのアドレス計算
// ============================================================

/// Doorbell レジスタのアドレスを計算する。
///
/// NVMe 仕様 Section 3.1.15: Doorbell Register は BAR0 + 0x1000 以降に配置される。
/// SQ y Tail Doorbell = BAR0 + 0x1000 + (2y)     × doorbell_stride
/// CQ y Head Doorbell = BAR0 + 0x1000 + (2y + 1)  × doorbell_stride
///
/// doorbell_stride = 4 × 2^DSTRD バイト。
fn sq_tail_doorbell(bar0: u64, queue_id: u16, stride: u32) -> *mut u32 {
    (bar0 + 0x1000 + (2 * queue_id as u64) * stride as u64) as *mut u32
}

fn cq_head_doorbell(bar0: u64, queue_id: u16, stride: u32) -> *mut u32 {
    (bar0 + 0x1000 + (2 * queue_id as u64 + 1) * stride as u64) as *mut u32
}

// ============================================================
// コントローラレジスタアクセス（BAR0 オフセット）
// ============================================================

/// NVMe コントローラレジスタのオフセット定数。
/// BAR0 + offset で各レジスタにアクセスする。
mod regs {
    /// 0x00: Controller Capabilities (64-bit, RO)
    pub const CAP: u64 = 0x00;
    /// 0x08: Version (32-bit, RO)
    pub const VS: u64 = 0x08;
    /// 0x14: Controller Configuration (32-bit, R/W)
    pub const CC: u64 = 0x14;
    /// 0x1C: Controller Status (32-bit, RO)
    pub const CSTS: u64 = 0x1C;
    /// 0x24: Admin Queue Attributes (32-bit, R/W)
    pub const AQA: u64 = 0x24;
    /// 0x28: Admin Submission Queue Base Address (64-bit, R/W)
    pub const ASQ: u64 = 0x28;
    /// 0x30: Admin Completion Queue Base Address (64-bit, R/W)
    pub const ACQ: u64 = 0x30;
}

/// MMIO レジスタの読み書きヘルパー。
/// アイデンティティマッピングなので物理アドレス = 仮想アドレス。
fn mmio_read32(bar0: u64, offset: u64) -> u32 {
    unsafe { core::ptr::read_volatile((bar0 + offset) as *const u32) }
}

fn mmio_write32(bar0: u64, offset: u64, value: u32) {
    unsafe { core::ptr::write_volatile((bar0 + offset) as *mut u32, value) }
}

fn mmio_read64(bar0: u64, offset: u64) -> u64 {
    // 64-bit レジスタは 2 回の 32-bit 読み取りで取得する。
    // ハードウェアが 64-bit アトミック読み取りをサポートしない場合があるため。
    let lo = mmio_read32(bar0, offset) as u64;
    let hi = mmio_read32(bar0, offset + 4) as u64;
    (hi << 32) | lo
}

fn mmio_write64(bar0: u64, offset: u64, value: u64) {
    mmio_write32(bar0, offset, value as u32);
    mmio_write32(bar0, offset + 4, (value >> 32) as u32);
}

// ============================================================
// 初期化
// ============================================================

/// NVMe ドライバを初期化する。
///
/// PCI バスから NVMe コントローラを探し、見つかった各コントローラを初期化する。
/// 初期化手順:
/// 1. PCI デバイス検出 → Bus Master + Memory Space 有効化
/// 2. BAR0 読み取り → MMIO ベースアドレス取得
/// 3. コントローラを無効化 → Admin Queue 設定 → コントローラを有効化
/// 4. Identify Controller/Namespace → 容量取得
/// 5. I/O Queue 作成 → Read/Write 可能な状態にする
pub fn init() {
    let controllers = pci::find_nvme_controllers();
    if controllers.is_empty() {
        serial_println!("NVMe: no controllers found");
        return;
    }

    let mut devices = Vec::new();

    for ctrl in controllers {
        serial_println!(
            "NVMe: controller found at PCI {:02x}:{:02x}.{}",
            ctrl.bus, ctrl.device, ctrl.function
        );

        match init_controller(&ctrl) {
            Ok(dev) => {
                devices.push(dev);
            }
            Err(e) => {
                serial_println!("NVMe: initialization failed: {}", e);
            }
        }
    }

    if devices.is_empty() {
        serial_println!("NVMe: no devices initialized");
    } else {
        serial_println!("NVMe: {} device(s) initialized", devices.len());
    }

    *NVME_DEVICES.lock() = devices;
}

/// 検出された NVMe デバイスの数を返す。
pub fn device_count() -> usize {
    NVME_DEVICES.lock().len()
}

/// 個別の NVMe コントローラを初期化する。
fn init_controller(ctrl: &pci::PciDevice) -> Result<NvmeDevice, &'static str> {
    // --- PCI Command レジスタで Bus Master + Memory Space を有効化 ---
    // Bus Master: NVMe コントローラが DMA でメインメモリにアクセスするために必要。
    // Memory Space: BAR0 の MMIO 領域にアクセスするために必要。
    let cmd = pci::pci_config_read16(ctrl.bus, ctrl.device, ctrl.function, 0x04);
    pci::pci_config_write16(ctrl.bus, ctrl.device, ctrl.function, 0x04, cmd | 0x06);

    // --- BAR0 読み取り ---
    // NVMe は BAR0 に MMIO レジスタがマップされる。
    // BAR0 は通常 64-bit BAR（BAR0 + BAR1 で 64-bit アドレスを構成）。
    let bar0_raw = pci::read_bar(ctrl.bus, ctrl.device, ctrl.function, 0);
    if bar0_raw & 1 != 0 {
        return Err("NVMe: BAR0 is I/O port, expected MMIO");
    }
    let bar_type = (bar0_raw >> 1) & 0x3;
    let bar0 = if bar_type == 0x02 {
        // 64-bit BAR
        let full = pci::read_bar64(ctrl.bus, ctrl.device, ctrl.function, 0);
        full & !0xF
    } else {
        // 32-bit BAR
        (bar0_raw & !0xF) as u64
    };

    if bar0 == 0 {
        return Err("NVMe: BAR0 is zero");
    }

    serial_println!("NVMe: BAR0 = {:#x}", bar0);

    // --- CAP レジスタから情報取得 ---
    let cap = mmio_read64(bar0, regs::CAP);
    let mqes = (cap & CAP_MQES_MASK) as u16 + 1; // 0-based → 実際のエントリ数
    let dstrd = ((cap >> CAP_DSTRD_SHIFT) & CAP_DSTRD_MASK) as u32;
    let doorbell_stride = 4 * (1u32 << dstrd); // 4 × 2^DSTRD バイト

    serial_println!("NVMe: CAP: MQES={}, DSTRD={} (stride={} bytes)", mqes, dstrd, doorbell_stride);

    // バージョンを表示
    let vs = mmio_read32(bar0, regs::VS);
    serial_println!("NVMe: version {}.{}.{}", (vs >> 16) & 0xFFFF, (vs >> 8) & 0xFF, vs & 0xFF);

    // キュー深さはコントローラの MQES 以下にする
    let queue_depth = core::cmp::min(QUEUE_DEPTH, mqes);

    // --- コントローラを無効化 ---
    // CC.EN = 0 にして CSTS.RDY = 0 になるのを待つ。
    // これにより Admin Queue を安全に設定できる。
    let cc = mmio_read32(bar0, regs::CC);
    if cc & CC_EN != 0 {
        mmio_write32(bar0, regs::CC, cc & !CC_EN);
        // CSTS.RDY = 0 を待つ
        for _ in 0..100_000_000u64 {
            if mmio_read32(bar0, regs::CSTS) & CSTS_RDY == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if mmio_read32(bar0, regs::CSTS) & CSTS_RDY != 0 {
            return Err("NVMe: controller disable timeout");
        }
    }

    // --- Admin Queue の物理メモリを確保 ---
    // Admin SQ: queue_depth × 64 バイト、ページアライン (4KB)
    // Admin CQ: queue_depth × 16 バイト、ページアライン (4KB)
    let sq_size = queue_depth as usize * core::mem::size_of::<NvmeSqe>();
    let cq_size = queue_depth as usize * core::mem::size_of::<NvmeCqe>();

    let sq_layout = Layout::from_size_align(sq_size, 4096).map_err(|_| "NVMe: SQ layout error")?;
    let cq_layout = Layout::from_size_align(cq_size, 4096).map_err(|_| "NVMe: CQ layout error")?;

    let sq_ptr = unsafe { alloc::alloc::alloc_zeroed(sq_layout) };
    if sq_ptr.is_null() {
        return Err("NVMe: failed to allocate Admin SQ");
    }
    let cq_ptr = unsafe { alloc::alloc::alloc_zeroed(cq_layout) };
    if cq_ptr.is_null() {
        unsafe { alloc::alloc::dealloc(sq_ptr, sq_layout); }
        return Err("NVMe: failed to allocate Admin CQ");
    }

    let sq_phys = sq_ptr as u64;
    let cq_phys = cq_ptr as u64;

    // --- AQA, ASQ, ACQ レジスタを設定 ---
    // AQA: Admin Queue Attributes — SQ と CQ のサイズを設定（0-based）。
    // bits [27:16] = ACQS (Admin CQ Size), bits [11:0] = ASQS (Admin SQ Size)。
    let aqa = ((queue_depth as u32 - 1) << 16) | (queue_depth as u32 - 1);
    mmio_write32(bar0, regs::AQA, aqa);
    // ASQ: Admin SQ の物理アドレス
    mmio_write64(bar0, regs::ASQ, sq_phys);
    // ACQ: Admin CQ の物理アドレス
    mmio_write64(bar0, regs::ACQ, cq_phys);

    // --- CC を設定してコントローラを有効化 ---
    // MPS=0 (4KB page), CSS=0 (NVM), IOSQES=6 (64B), IOCQES=4 (16B), EN=1
    let cc_val = CC_EN | CC_CSS_NVM | CC_MPS_4K | CC_IOSQES_64 | CC_IOCQES_16;
    mmio_write32(bar0, regs::CC, cc_val);

    // CSTS.RDY = 1 を待つ
    for _ in 0..100_000_000u64 {
        let csts = mmio_read32(bar0, regs::CSTS);
        if csts & CSTS_CFS != 0 {
            return Err("NVMe: controller fatal status during enable");
        }
        if csts & CSTS_RDY != 0 {
            break;
        }
        core::hint::spin_loop();
    }
    if mmio_read32(bar0, regs::CSTS) & CSTS_RDY == 0 {
        return Err("NVMe: controller enable timeout");
    }

    serial_println!("NVMe: controller enabled (RDY=1)");

    // --- Admin Queue 構造体を構築 ---
    let admin_queue = NvmeQueue {
        sq_base: sq_ptr as *mut NvmeSqe,
        cq_base: cq_ptr as *mut NvmeCqe,
        depth: queue_depth,
        sq_tail: 0,
        cq_head: 0,
        phase: true, // 初期 Phase は true (1)
        sq_doorbell: sq_tail_doorbell(bar0, 0, doorbell_stride),
        cq_doorbell: cq_head_doorbell(bar0, 0, doorbell_stride),
        next_cid: 0,
    };

    let mut dev = NvmeDevice {
        bar0,
        doorbell_stride,
        admin_queue,
        io_queue: None,
        ns_size: 0,
        block_size: 512,
    };

    // --- Identify Controller ---
    let model = dev.identify_controller()?;
    serial_println!("NVMe: controller: {}", model);

    // --- Identify Namespace 1 ---
    let (ns_size, block_size) = dev.identify_namespace(1)?;
    dev.ns_size = ns_size;
    dev.block_size = block_size;
    let size_mib = ns_size * block_size as u64 / 1024 / 1024;
    serial_println!(
        "NVMe: namespace 1: {} blocks × {} bytes = {} MiB",
        ns_size, block_size, size_mib
    );
    crate::kprintln!(
        "  [NVMe] {} ({} MiB, block={}B)",
        model, size_mib, block_size
    );

    // --- I/O Queue 作成 ---
    dev.create_io_queues(queue_depth)?;
    serial_println!("NVMe: I/O queues created (depth={})", queue_depth);

    Ok(dev)
}

impl NvmeDevice {
    /// Identify Controller コマンドを発行し、モデル名を取得する。
    ///
    /// Admin opcode 0x06, CNS=1 でコントローラ情報 (4096 バイト) を取得する。
    /// Identify データ構造体の byte [24..63] にモデル名 (40 文字) が格納されている。
    fn identify_controller(&mut self) -> Result<String, &'static str> {
        // Identify レスポンス用の 4096 バイトバッファ（ページアライン）
        let layout = Layout::from_size_align(4096, 4096).map_err(|_| "NVMe: layout error")?;
        let buf = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if buf.is_null() {
            return Err("NVMe: failed to allocate Identify buffer");
        }

        let mut sqe = NvmeSqe::zeroed();
        sqe.opcode = ADMIN_IDENTIFY;
        sqe.nsid = 0;
        sqe.prp1 = buf as u64;
        sqe.cdw10 = IDENTIFY_CNS_CONTROLLER;

        self.admin_queue.submit(sqe);
        self.admin_queue.poll_completion()?;

        // モデル名: byte [24..63] (40 bytes), ASCII
        let data = unsafe { core::slice::from_raw_parts(buf, 4096) };
        let model_bytes = &data[24..64];
        let model = core::str::from_utf8(model_bytes)
            .unwrap_or("(unknown)")
            .trim()
            .into();

        unsafe { alloc::alloc::dealloc(buf, layout); }

        Ok(model)
    }

    /// Identify Namespace コマンドを発行し、容量とブロックサイズを取得する。
    ///
    /// Admin opcode 0x06, CNS=0 でネームスペース情報 (4096 バイト) を取得する。
    /// NSZE (byte [0..7]): ネームスペースサイズ（論理ブロック数）
    /// LBAF[0] (byte [128..131]): LBA Format 0 — LBADS フィールドからブロックサイズを計算
    fn identify_namespace(&mut self, nsid: u32) -> Result<(u64, u32), &'static str> {
        let layout = Layout::from_size_align(4096, 4096).map_err(|_| "NVMe: layout error")?;
        let buf = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if buf.is_null() {
            return Err("NVMe: failed to allocate Identify NS buffer");
        }

        let mut sqe = NvmeSqe::zeroed();
        sqe.opcode = ADMIN_IDENTIFY;
        sqe.nsid = nsid;
        sqe.prp1 = buf as u64;
        sqe.cdw10 = IDENTIFY_CNS_NAMESPACE;

        self.admin_queue.submit(sqe);
        self.admin_queue.poll_completion()?;

        let data = unsafe { core::slice::from_raw_parts(buf, 4096) };

        // NSZE: byte [0..7] — ネームスペースサイズ（論理ブロック数、リトルエンディアン 64-bit）
        let nsze = u64::from_le_bytes([
            data[0], data[1], data[2], data[3],
            data[4], data[5], data[6], data[7],
        ]);

        // FLBAS: byte [26] — Formatted LBA Size
        // bits [3:0] = 使用中の LBA Format のインデックス
        let flbas = data[26];
        let lba_format_index = (flbas & 0x0F) as usize;

        // LBAF (LBA Format) テーブル: byte [128..191]、各 4 バイト
        // LBAF[n] の byte [2] の bits [7:0] = LBADS (LBA Data Size, 2^n バイト)
        // 通常 LBADS=9 (512B) or LBADS=12 (4KB)
        let lbaf_offset = 128 + lba_format_index * 4;
        let lbads = data[lbaf_offset + 2];
        let block_size = 1u32 << lbads;

        unsafe { alloc::alloc::dealloc(buf, layout); }

        if nsze == 0 {
            return Err("NVMe: namespace size is 0");
        }

        Ok((nsze, block_size))
    }

    /// I/O Completion Queue と I/O Submission Queue を作成する。
    ///
    /// Admin コマンドで I/O Queue を作成する:
    /// 1. Create I/O Completion Queue (Admin opcode 0x05)
    /// 2. Create I/O Submission Queue (Admin opcode 0x01)
    ///
    /// I/O Queue の ID は 1（Admin Queue は 0）。
    fn create_io_queues(&mut self, depth: u16) -> Result<(), &'static str> {
        // I/O CQ 用メモリ確保
        let cq_size = depth as usize * core::mem::size_of::<NvmeCqe>();
        let cq_layout = Layout::from_size_align(cq_size, 4096).map_err(|_| "NVMe: CQ layout error")?;
        let cq_ptr = unsafe { alloc::alloc::alloc_zeroed(cq_layout) };
        if cq_ptr.is_null() {
            return Err("NVMe: failed to allocate I/O CQ");
        }

        // I/O SQ 用メモリ確保
        let sq_size = depth as usize * core::mem::size_of::<NvmeSqe>();
        let sq_layout = Layout::from_size_align(sq_size, 4096).map_err(|_| "NVMe: SQ layout error")?;
        let sq_ptr = unsafe { alloc::alloc::alloc_zeroed(sq_layout) };
        if sq_ptr.is_null() {
            unsafe { alloc::alloc::dealloc(cq_ptr, cq_layout); }
            return Err("NVMe: failed to allocate I/O SQ");
        }

        // --- Create I/O Completion Queue ---
        // CDW10: bits [31:16] = Queue Size (0-based), bits [15:0] = Queue ID
        // CDW11: bit [0] = PC (Physically Contiguous) = 1
        let mut sqe = NvmeSqe::zeroed();
        sqe.opcode = ADMIN_CREATE_IO_CQ;
        sqe.prp1 = cq_ptr as u64;
        sqe.cdw10 = ((depth as u32 - 1) << 16) | 1; // QID=1, QSIZE=depth-1
        sqe.cdw11 = 1; // PC=1 (Physically Contiguous)

        self.admin_queue.submit(sqe);
        self.admin_queue.poll_completion()?;

        // --- Create I/O Submission Queue ---
        // CDW10: bits [31:16] = Queue Size (0-based), bits [15:0] = Queue ID
        // CDW11: bits [31:16] = CQ ID, bit [0] = PC (Physically Contiguous) = 1
        let mut sqe = NvmeSqe::zeroed();
        sqe.opcode = ADMIN_CREATE_IO_SQ;
        sqe.prp1 = sq_ptr as u64;
        sqe.cdw10 = ((depth as u32 - 1) << 16) | 1; // QID=1, QSIZE=depth-1
        sqe.cdw11 = (1 << 16) | 1; // CQID=1, PC=1

        self.admin_queue.submit(sqe);
        self.admin_queue.poll_completion()?;

        // I/O Queue 構造体を構築
        self.io_queue = Some(NvmeQueue {
            sq_base: sq_ptr as *mut NvmeSqe,
            cq_base: cq_ptr as *mut NvmeCqe,
            depth,
            sq_tail: 0,
            cq_head: 0,
            phase: true,
            sq_doorbell: sq_tail_doorbell(self.bar0, 1, self.doorbell_stride),
            cq_doorbell: cq_head_doorbell(self.bar0, 1, self.doorbell_stride),
            next_cid: 0,
        });

        Ok(())
    }

    /// 指定セクタからデータを読み取る（NVM Read コマンド）。
    ///
    /// I/O opcode 0x02 で 1 セクタを読み取る。
    /// CDW10-11: Starting LBA (64-bit)
    /// CDW12: Number of Logical Blocks (0-based) = 0 (1 ブロック)
    /// 一時的なエラーに対しては最大 IO_RETRY_COUNT 回リトライする。
    pub fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        // バリデーションエラーはリトライしても意味がないので即返す
        if self.io_queue.is_none() {
            return Err("NVMe: I/O queue not created");
        }
        if buf.len() < self.block_size as usize {
            return Err("NVMe: buffer too small");
        }

        for attempt in 0..IO_RETRY_COUNT {
            match self.read_sector_once(sector, buf) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if attempt + 1 < IO_RETRY_COUNT {
                        serial_println!("NVMe: read sector {} failed (attempt {}): {}, retrying...",
                            sector, attempt + 1, e);
                        for _ in 0..IO_RETRY_SPIN_WAIT { core::hint::spin_loop(); }
                    } else {
                        serial_println!("NVMe: read sector {} failed after {} attempts: {}",
                            sector, IO_RETRY_COUNT, e);
                        return Err(e);
                    }
                }
            }
        }
        Err("NVMe: read failed (unreachable)")
    }

    /// 指定セクタからデータを読み取る内部実装（1 回分）。
    fn read_sector_once(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        let io_queue = self.io_queue.as_mut().ok_or("NVMe: I/O queue not created")?;

        let mut sqe = NvmeSqe::zeroed();
        sqe.opcode = IO_CMD_READ;
        sqe.nsid = 1;
        sqe.prp1 = buf.as_mut_ptr() as u64;
        sqe.cdw10 = sector as u32;         // SLBA 下位 32 ビット
        sqe.cdw11 = (sector >> 32) as u32;  // SLBA 上位 32 ビット
        sqe.cdw12 = 0; // NLB = 0 (1 ブロック、0-based)

        io_queue.submit(sqe);
        io_queue.poll_completion()?;

        Ok(())
    }

    /// 指定セクタにデータを書き込む（NVM Write コマンド）。
    ///
    /// I/O opcode 0x01 で 1 セクタを書き込む。
    /// 一時的なエラーに対しては最大 IO_RETRY_COUNT 回リトライする。
    pub fn write_sector(&mut self, sector: u64, buf: &[u8]) -> Result<(), &'static str> {
        // バリデーションエラーはリトライしても意味がないので即返す
        if self.io_queue.is_none() {
            return Err("NVMe: I/O queue not created");
        }
        if buf.len() < self.block_size as usize {
            return Err("NVMe: buffer too small");
        }

        for attempt in 0..IO_RETRY_COUNT {
            match self.write_sector_once(sector, buf) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if attempt + 1 < IO_RETRY_COUNT {
                        serial_println!("NVMe: write sector {} failed (attempt {}): {}, retrying...",
                            sector, attempt + 1, e);
                        for _ in 0..IO_RETRY_SPIN_WAIT { core::hint::spin_loop(); }
                    } else {
                        serial_println!("NVMe: write sector {} failed after {} attempts: {}",
                            sector, IO_RETRY_COUNT, e);
                        return Err(e);
                    }
                }
            }
        }
        Err("NVMe: write failed (unreachable)")
    }

    /// 指定セクタにデータを書き込む内部実装（1 回分）。
    fn write_sector_once(&mut self, sector: u64, buf: &[u8]) -> Result<(), &'static str> {
        let io_queue = self.io_queue.as_mut().ok_or("NVMe: I/O queue not created")?;

        let mut sqe = NvmeSqe::zeroed();
        sqe.opcode = IO_CMD_WRITE;
        sqe.nsid = 1;
        sqe.prp1 = buf.as_ptr() as u64;
        sqe.cdw10 = sector as u32;         // SLBA 下位 32 ビット
        sqe.cdw11 = (sector >> 32) as u32;  // SLBA 上位 32 ビット
        sqe.cdw12 = 0; // NLB = 0 (1 ブロック、0-based)

        io_queue.submit(sqe);
        io_queue.poll_completion()?;

        Ok(())
    }
}
