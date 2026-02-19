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
//
// Phase 3-2: FADT (Fixed ACPI Description Table) から電源管理情報を取得し、
// ACPI S5 シャットダウン（電源OFF）とシステムリブートを実装する。
// - S5 スリープタイプは DSDT のバイト列を直接スキャンして `_S5_` オブジェクトから取得
//   （軽量実装、AML インタープリタ不要）
// - リブートは FADT reset_register → 8042 キーボードコントローラ → トリプルフォルトの
//   3 段フォールバック

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

/// FADT から取得した電源管理情報。
/// ACPI S5 シャットダウンとシステムリブートに必要な情報を保持する。
pub struct AcpiFadtInfo {
    /// PM1a Control Block の I/O ポートアドレス。
    /// ACPI S5 シャットダウン時に SLP_TYP と SLP_EN を書き込む先。
    pub pm1a_cnt_blk: u16,
    /// リセットレジスタのアドレス（I/O ポートまたは MMIO）。
    /// FADT reset_reg フィールドから取得。
    pub reset_reg_addr: u64,
    /// リセットレジスタのアドレス空間。
    /// true = SystemIo（I/O ポート）、false = SystemMemory（MMIO）。
    pub reset_reg_is_io: bool,
    /// リセットレジスタに書き込む値。
    /// この値を reset_reg_addr に書き込むとシステムがリセットされる。
    pub reset_value: u8,
    /// FADT の flags フィールドで reset がサポートされているか。
    /// bit 10 (RESET_REG_SUP) が立っていれば true。
    pub supports_reset: bool,
    /// S5 スリープタイプ（DSDT の `_S5_` パッケージから取得）。
    /// PM1a_CNT に書き込む SLP_TYPa の値。
    /// None の場合は DSDT スキャンで `_S5_` が見つからなかったことを意味する。
    pub slp_typa_s5: Option<u8>,
}

/// FADT 情報のグローバルストレージ。
static ACPI_FADT_INFO: Once<AcpiFadtInfo> = Once::new();

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

    // FADT (Fixed ACPI Description Table) から電源管理情報を取得する。
    // FADT には PM1a Control Block（シャットダウン用）とリセットレジスタ（リブート用）が含まれる。
    match tables.find_table::<acpi::fadt::Fadt>() {
        Ok(fadt) => {
            // PM1a Control Block: S5 シャットダウン時に SLP_TYP と SLP_EN を書き込む I/O ポート
            let pm1a = fadt.pm1a_control_block().ok()
                .map(|g| g.address as u16).unwrap_or(0);

            // リセットレジスタ: システムリブート時に reset_value を書き込む先
            let (reset_addr, reset_is_io) = fadt.reset_register().ok()
                .map(|g| (g.address, g.address_space == acpi::address::AddressSpace::SystemIo))
                .unwrap_or((0, false));
            // FADT は packed struct なのでフィールドを直接参照するとアライメント違反になる。
            // ローカル変数にコピーしてからメソッドを呼ぶ。
            let flags = { fadt.flags };
            let supports_reset = flags.supports_system_reset_via_fadt();
            let reset_val = { fadt.reset_value };

            // DSDT (Differentiated System Description Table) から S5 スリープタイプを取得。
            // DSDT には AML (ACPI Machine Language) バイトコードが含まれており、
            // `_S5_` という名前のオブジェクトに電源OFF 用のスリープタイプが格納されている。
            let slp_typa_s5 = fadt.dsdt_address().ok().and_then(|dsdt_addr| {
                scan_dsdt_for_s5(dsdt_addr)
            });

            crate::kprintln!("ACPI: FADT PM1a_CNT={:#x}, reset_reg={:#x} ({}), reset_val={:#x}",
                pm1a, reset_addr,
                if reset_is_io { "I/O" } else { "MMIO" },
                reset_val);
            if let Some(slp) = slp_typa_s5 {
                crate::kprintln!("ACPI: S5 sleep type = {:#x} (from DSDT _S5_ scan)", slp);
            } else {
                crate::kprintln!("ACPI: WARNING: _S5_ not found in DSDT, shutdown may not work");
            }

            ACPI_FADT_INFO.call_once(|| AcpiFadtInfo {
                pm1a_cnt_blk: pm1a,
                reset_reg_addr: reset_addr,
                reset_reg_is_io: reset_is_io,
                reset_value: reset_val,
                supports_reset,
                slp_typa_s5,
            });
        }
        Err(e) => {
            crate::kprintln!("ACPI: FADT not found: {:?}", e);
        }
    }
}

/// ACPI から取得した APIC 情報を返す。
/// ACPI が利用不可または APIC 情報がない場合は None。
pub fn get_apic_info() -> Option<&'static AcpiApicInfo> {
    ACPI_INFO.get()
}

/// FADT から取得した電源管理情報を返す。
/// FADT が利用不可の場合は None。
pub fn get_fadt_info() -> Option<&'static AcpiFadtInfo> {
    ACPI_FADT_INFO.get()
}

/// DSDT のバイト列をスキャンして `_S5_` スリープタイプを取得する。
///
/// AML インタープリタを使わない軽量実装。DSDT のバイナリデータから
/// `_S5_` という名前のパッケージオブジェクトを検索し、その最初の要素
/// （SLP_TYPa の値）を返す。
///
/// AML バイトコードの構造:
/// - NameOp (0x08) + "_S5_" (0x5F 0x53 0x35 0x5F) + PackageOp (0x12) + PkgLength + NumElements + 要素...
/// - 要素は BytePrefix (0x0A) + 値、または ZeroOp (0x00) / OneOp (0x01) の即値
///
/// `dsdt_phys`: DSDT テーブルの物理アドレス（= 仮想アドレス、アイデンティティマッピング）
fn scan_dsdt_for_s5(dsdt_phys: usize) -> Option<u8> {
    // DSDT はヘッダ（36 バイト）+ AML バイトコードで構成される。
    // ヘッダの length フィールドからテーブル全体のサイズを取得する。

    // DSDT ヘッダを読んでテーブル長を取得（ヘッダは acpi crate の SdtHeader と同じレイアウト）
    // offset 4 に u32 LE で length が入っている
    let length = unsafe {
        let len_ptr = (dsdt_phys + 4) as *const u32;
        core::ptr::read_unaligned(len_ptr) as usize
    };

    // 安全のためサイズを制限（壊れた DSDT テーブルへの対策）
    if length < 36 || length > 4 * 1024 * 1024 {
        crate::kprintln!("ACPI: DSDT length {} looks invalid", length);
        return None;
    }

    let data = unsafe { core::slice::from_raw_parts(dsdt_phys as *const u8, length) };

    // "_S5_" のバイトパターンを DSDT 全体から検索
    let needle = b"_S5_";
    for i in 0..data.len().saturating_sub(needle.len() + 5) {
        if &data[i..i + 4] == needle {
            // _S5_ の後に PackageOp (0x12) があるはず
            let pkg_start = i + 4;
            if pkg_start >= data.len() || data[pkg_start] != 0x12 {
                continue;
            }

            // PkgLength のエンコーディング:
            // - 下位 2 ビットが 0 なら 1 バイト長
            // - 下位 2 ビットが n (1-3) なら n+1 バイト長
            // ここでは PkgLength の先頭バイトの上位 2 ビット (bit 7:6) でバイト数を判定
            let pkg_len_byte0 = data[pkg_start + 1];
            let pkg_len_bytes = if pkg_len_byte0 & 0xC0 == 0 {
                1usize // 1 バイト PkgLength（6 ビット長）
            } else {
                ((pkg_len_byte0 >> 6) & 0x03) as usize + 1
            };

            // NumElements（パッケージ内の要素数）の位置
            let num_elements_offset = pkg_start + 1 + pkg_len_bytes;
            if num_elements_offset >= data.len() {
                continue;
            }

            // 最初の要素（SLP_TYPa）の位置
            let val_offset = num_elements_offset + 1;
            if val_offset >= data.len() {
                continue;
            }

            // 要素の値を読む:
            // - BytePrefix (0x0A) + 1バイト値: 2バイト以上の即値
            // - ZeroOp (0x00): 値 0
            // - OneOp (0x01): 値 1
            // - それ以外: 直接バイト値として解釈
            if data[val_offset] == 0x0A && val_offset + 1 < data.len() {
                // BytePrefix 形式: 0x0A の次のバイトが値
                return Some(data[val_offset + 1]);
            } else {
                // 即値（ZeroOp, OneOp, その他）
                return Some(data[val_offset]);
            }
        }
    }

    None
}

/// ACPI S5 シャットダウン（電源OFF）。
///
/// PM1a Control Block に SLP_TYPa と SLP_EN ビットを書き込んで
/// システムを S5 ステート（Soft Off）に遷移させる。
/// QEMU ではこの操作で仮想マシンが終了する。
///
/// S5 シャットダウンに失敗した場合は HLT ループにフォールバックする。
pub fn acpi_shutdown() {
    if let Some(info) = ACPI_FADT_INFO.get() {
        if let Some(slp_typa) = info.slp_typa_s5 {
            if info.pm1a_cnt_blk != 0 {
                // PM1a_CNT レジスタの構造:
                // bit 12-10: SLP_TYPa（スリープタイプ、DSDT _S5_ から取得した値）
                // bit 13:    SLP_EN（スリープイネーブル、1 でスリープ遷移を開始）
                let val = (slp_typa as u16) << 10 | (1 << 13);
                crate::kprintln!("ACPI: Writing {:#06x} to PM1a_CNT ({:#06x})", val, info.pm1a_cnt_blk);
                unsafe {
                    x86_64::instructions::port::Port::new(info.pm1a_cnt_blk).write(val);
                }
                // QEMU ではここで電源が切れるはず。
                // 切れなかった場合は少し待ってからフォールバックする。
                for _ in 0..100_000 {
                    core::hint::spin_loop();
                }
            }
        }
    }
    // フォールバック: HLT ループ（ACPI シャットダウンが効かなかった場合）
    crate::kprintln!("ACPI: Shutdown failed, halting CPU");
    loop {
        x86_64::instructions::interrupts::disable();
        x86_64::instructions::hlt();
    }
}

/// ACPI リブート（システム再起動）。
///
/// 3 段階のフォールバック:
/// 1. FADT reset register に reset_value を書き込む（ACPI 標準）
/// 2. 8042 キーボードコントローラにリセットコマンド (0xFE) を送信
/// 3. トリプルフォルト（IDT を無効化して例外を発生させる最終手段）
pub fn acpi_reboot() -> ! {
    // 方法 1: FADT reset register
    // ACPI 2.0 以降で定義されたリセットメカニズム。
    // FADT の reset_reg で指定されたアドレスに reset_value を書き込む。
    if let Some(info) = ACPI_FADT_INFO.get() {
        if info.supports_reset && info.reset_reg_addr != 0 {
            crate::kprintln!("ACPI: Resetting via FADT reset register ({:#x})", info.reset_reg_addr);
            if info.reset_reg_is_io {
                // SystemIo: I/O ポートに書き込む
                unsafe {
                    x86_64::instructions::port::Port::new(info.reset_reg_addr as u16)
                        .write(info.reset_value);
                }
            } else {
                // SystemMemory: MMIO アドレスに書き込む
                unsafe {
                    core::ptr::write_volatile(info.reset_reg_addr as *mut u8, info.reset_value);
                }
            }
            // 少し待つ（リセットが効くまでの猶予）
            for _ in 0..100_000 {
                core::hint::spin_loop();
            }
        }
    }

    // 方法 2: 8042 キーボードコントローラ リセット
    // レガシーなリセット方法。I/O ポート 0x64 にコマンド 0xFE を送信すると
    // キーボードコントローラが CPU リセットラインをアサートする。
    crate::kprintln!("ACPI: FADT reset failed, trying 8042 keyboard controller reset");
    unsafe {
        x86_64::instructions::port::Port::<u8>::new(0x64).write(0xFE);
    }
    for _ in 0..100_000 {
        core::hint::spin_loop();
    }

    // 方法 3: トリプルフォルト（最終手段）
    // IDT (Interrupt Descriptor Table) を無効な値に設定して
    // 意図的にトリプルフォルトを発生させる。
    // トリプルフォルトが発生すると CPU は強制リセットされる。
    crate::kprintln!("ACPI: 8042 reset failed, triggering triple fault");
    unsafe {
        // 無効な IDT リミット (0) を設定
        let null_idt: [u8; 10] = [0; 10]; // limit=0, base=0
        core::arch::asm!(
            "lidt [{}]",
            "int3",          // ブレークポイント例外を発生 → IDT が無効なのでダブルフォルト → トリプルフォルト
            in(reg) null_idt.as_ptr(),
            options(noreturn)
        );
    }
}
