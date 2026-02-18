// acpi.rs — ACPI テーブルパース
//
// ACPI (Advanced Configuration and Power Interface) はハードウェア構成情報を
// OS に伝えるためのファームウェアインターフェース。
// RSDP → RSDT/XSDT → MADT (APIC 情報) という階層構造のテーブルを持つ。
//
// `acpi` crate (no_std 対応) を使ってテーブルをパースし、
// APIC (割り込みコントローラ) の情報を取得する。
// SABOS ではアイデンティティマッピング（物理アドレス=仮想アドレス）なので、
// ACPI テーブルのメモリアクセスは map/unmap が不要（no-op）。

use alloc::vec::Vec;
use core::ptr::NonNull;
use acpi::{AcpiHandler, AcpiTables, PhysicalMapping};
use spin::Once;

/// ACPI から取得した APIC 情報を保持する構造体。
#[derive(Debug)]
pub struct AcpiApicInfo {
    /// Local APIC のベース物理アドレス（通常 0xFEE00000）
    pub local_apic_address: u64,
    /// I/O APIC のリスト（アドレスと GSI ベース）
    pub io_apics: Vec<IoApicInfo>,
    /// レガシー PIC (8259) が存在するかどうか
    /// （APIC 初期化時に PIC のマスク処理の判断に使う可能性がある）
    #[allow(dead_code)]
    pub has_legacy_pic: bool,
}

/// I/O APIC の情報。
#[derive(Debug, Clone)]
pub struct IoApicInfo {
    /// I/O APIC の ID
    pub id: u8,
    /// I/O APIC のベース物理アドレス
    pub address: u32,
    /// この I/O APIC が担当する GSI (Global System Interrupt) の開始番号
    pub global_system_interrupt_base: u32,
}

/// ACPI 情報のグローバルストレージ。
/// Once で一度だけ初期化される。
static ACPI_INFO: Once<AcpiApicInfo> = Once::new();

/// SABOS のアイデンティティマッピング用 ACPI ハンドラ。
///
/// `acpi` crate は ACPI テーブルのメモリにアクセスするために
/// AcpiHandler トレイトの実装を要求する。
/// SABOS では物理アドレス = 仮想アドレス のアイデンティティマッピングなので、
/// map_physical_region() は物理アドレスをそのままポインタに変換するだけ（no-op）。
/// unmap_physical_region() も何もしない。
#[derive(Clone)]
struct IdentityMappedAcpiHandler;

impl AcpiHandler for IdentityMappedAcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        // アイデンティティマッピングなので物理アドレスをそのまま仮想アドレスとして使う
        let ptr = NonNull::new(physical_address as *mut T)
            .expect("ACPI: null pointer in map_physical_region");
        unsafe {
            PhysicalMapping::new(
                physical_address,
                ptr,
                size,
                size,
                Self,
            )
        }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {
        // アイデンティティマッピングなので unmap は不要（no-op）
    }
}

/// ACPI テーブルをパースして APIC 情報を取得・保存する。
///
/// `rsdp_phys` は UEFI Configuration Table から取得した RSDP の物理アドレス。
/// ヒープが必要なため、allocator::init() の後に呼ぶこと。
pub fn init(rsdp_phys: u64) {
    if rsdp_phys == 0 {
        crate::kprintln!("ACPI: RSDP not available, skipping ACPI init");
        return;
    }

    // RSDP からACPI テーブル群をパースする
    let tables = match unsafe {
        AcpiTables::from_rsdp(IdentityMappedAcpiHandler, rsdp_phys as usize)
    } {
        Ok(tables) => tables,
        Err(e) => {
            crate::kprintln!("ACPI: Failed to parse tables: {:?}", e);
            return;
        }
    };

    // PlatformInfo を取得して割り込みモデルを確認する
    let platform_info = match tables.platform_info() {
        Ok(info) => info,
        Err(e) => {
            crate::kprintln!("ACPI: Failed to get platform info: {:?}", e);
            return;
        }
    };

    // 割り込みモデルから APIC 情報を抽出する
    match platform_info.interrupt_model {
        acpi::InterruptModel::Apic(apic_model) => {
            let io_apics: Vec<IoApicInfo> = apic_model.io_apics.iter().map(|io| {
                IoApicInfo {
                    id: io.id,
                    address: io.address,
                    global_system_interrupt_base: io.global_system_interrupt_base,
                }
            }).collect();

            crate::kprintln!("ACPI: Local APIC at {:#x}", apic_model.local_apic_address);
            for io in &io_apics {
                crate::kprintln!("ACPI: I/O APIC #{} at {:#x} (GSI base {})",
                    io.id, io.address, io.global_system_interrupt_base);
            }

            let has_legacy_pic = apic_model.also_has_legacy_pics;
            crate::kprintln!("ACPI: Legacy PIC: {}", if has_legacy_pic { "yes" } else { "no" });

            ACPI_INFO.call_once(|| AcpiApicInfo {
                local_apic_address: apic_model.local_apic_address,
                io_apics,
                has_legacy_pic,
            });
        }
        acpi::InterruptModel::Unknown => {
            crate::kprintln!("ACPI: Unknown interrupt model");
        }
        _ => {
            crate::kprintln!("ACPI: Unsupported interrupt model");
        }
    }
}

/// ACPI から取得した APIC 情報を返す。
/// ACPI が利用不可または APIC 情報がない場合は None。
pub fn get_apic_info() -> Option<&'static AcpiApicInfo> {
    ACPI_INFO.get()
}
