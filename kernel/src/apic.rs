// apic.rs — APIC (Advanced Programmable Interrupt Controller) 初期化
//
// APIC は PIC (8259) の後継となる割り込みコントローラ。
// Local APIC は各 CPU コアに内蔵されており、I/O APIC は外部デバイスからの
// 割り込みを CPU に配送する。
//
// PIC と比較した APIC の利点:
//   - IRQ が 16 本 → 24+ 本に増加（デバイスが増えても競合しにくい）
//   - マルチプロセッサ対応（割り込みを特定の CPU に配送可能）
//   - MSI/MSI-X 対応（PCI デバイスが直接 Local APIC にメッセージを送れる）
//   - プログラマブルタイマー内蔵（PIT 不要）
//
// 初期化手順:
//   1. PIC を全マスク（APIC に移行するため PIC からの割り込みを止める）
//   2. Local APIC を初期化（タイマー、スプリアス、エラーベクタを設定）
//   3. I/O APIC を初期化（キーボード IRQ1、マウス IRQ12 を有効化）

use core::sync::atomic::{AtomicBool, Ordering};
use x2apic::ioapic::IoApic;
use x2apic::lapic::{LocalApicBuilder, TimerDivide, TimerMode};

use crate::acpi;
use crate::interrupts::PICS;

/// APIC が有効化されているかどうかのフラグ。
/// 割り込みハンドラで EOI の送信先（APIC or PIC）を切り替えるのに使う。
static IS_APIC_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Local APIC のインスタンス。
/// EOI 送信で使うが、割り込みハンドラから Mutex を取るとデッドロックの危険があるため、
/// 初期化後は EOI 専用の直接レジスタ書き込みを使う（local_apic_eoi()）。
static LOCAL_APIC_BASE: spin::Once<u64> = spin::Once::new();

/// APIC が有効かどうかを返す。
pub fn is_apic_active() -> bool {
    IS_APIC_ACTIVE.load(Ordering::Relaxed)
}

/// Local APIC に EOI (End Of Interrupt) を送信する。
///
/// 割り込みハンドラから呼ぶ。Mutex を使わずに直接 MMIO レジスタに書き込むことで、
/// デッドロックのリスクを回避する。
/// Local APIC の EOI レジスタは base + 0xB0 にあり、0 を書き込むと EOI が完了する。
pub fn local_apic_eoi() {
    if let Some(&base) = LOCAL_APIC_BASE.get() {
        unsafe {
            let eoi_reg = (base + 0xB0) as *mut u32;
            core::ptr::write_volatile(eoi_reg, 0);
        }
    }
}

/// APIC を初期化する。
///
/// ACPI テーブルから取得した情報を使って Local APIC と I/O APIC を設定する。
/// ACPI 情報がない場合は PIC のまま動作する（フォールバック）。
pub fn init() {
    let apic_info = match acpi::get_apic_info() {
        Some(info) => info,
        None => {
            crate::kprintln!("APIC: No ACPI APIC info available, keeping PIC mode");
            return;
        }
    };

    // 1. PIC を全マスク（APIC に移行するため PIC からの割り込みを無効化）
    // PIC からの割り込みが APIC と競合しないようにする。
    unsafe {
        PICS.lock().write_masks(0xFF, 0xFF);
    }
    crate::kprintln!("APIC: PIC masked");

    // 2. Local APIC の初期化
    // Local APIC はタイマー割り込み、プロセッサ間割り込み (IPI) 等を管理する。
    let lapic_addr = apic_info.local_apic_address;
    LOCAL_APIC_BASE.call_once(|| lapic_addr);

    let mut lapic = LocalApicBuilder::new()
        // タイマー割り込みベクタ: 32（PIC と同じオフセット。既存のタイマーハンドラを再利用）
        .timer_vector(32)
        // エラー割り込みベクタ: 0xFE（APIC 内部エラー通知用）
        .error_vector(0xFE)
        // スプリアス割り込みベクタ: 0xFF（偽の割り込み。通常は無視してよい）
        .spurious_vector(0xFF)
        // タイマーモード: Periodic（一定間隔で繰り返し発火）
        .timer_mode(TimerMode::Periodic)
        // タイマー分周: 64 分周（QEMU 向けの暫定値。実機では PIT キャリブレーション必要）
        .timer_divide(TimerDivide::Div64)
        // タイマー初期カウント: QEMU のバスクロック 1GHz 想定。
        // 割り込み間隔 = initial * divide / bus_clock = 1000000 * 64 / 1e9 = 64ms
        // PIT の約 55ms (18.2Hz) と同程度のスケジューリング頻度。
        .timer_initial(1_000_000)
        // Local APIC のベースアドレス（通常 0xFEE00000、ACPI テーブルから取得）
        .set_xapic_base(lapic_addr)
        .build()
        .expect("APIC: Failed to build LocalApic");

    unsafe {
        lapic.enable();
    }
    crate::kprintln!("APIC: Local APIC enabled at {:#x}", lapic_addr);

    // 3. I/O APIC の初期化
    // I/O APIC は外部デバイス（キーボード、マウス等）からの IRQ を
    // 適切な CPU の Local APIC にルーティングする。
    if let Some(io_apic_info) = apic_info.io_apics.first() {
        let io_apic_addr = io_apic_info.address as u64;
        unsafe {
            let mut io_apic = IoApic::new(io_apic_addr);

            // 全エントリを offset=32 で初期化（IRQ N → ベクタ 32+N）
            io_apic.init(32);

            // IRQ1: キーボード（PS/2）を有効化
            // ベクタ 33 (= 32 + 1) に配送される → 既存の keyboard_interrupt_handler を再利用
            io_apic.enable_irq(1);

            // IRQ12: マウス（PS/2）を有効化
            // ベクタ 44 (= 32 + 12) に配送される → 既存の mouse_interrupt_handler を再利用
            io_apic.enable_irq(12);

            // IRQ0 (PIT タイマー) は無効のまま。
            // タイマーは Local APIC タイマーを使用する（より正確で省電力）。
        }
        crate::kprintln!("APIC: I/O APIC initialized at {:#x} (IRQ1=kbd, IRQ12=mouse)",
            io_apic_addr);
    } else {
        crate::kprintln!("APIC: No I/O APIC found");
    }

    // APIC モードに切り替え完了
    IS_APIC_ACTIVE.store(true, Ordering::Relaxed);
    crate::kprintln!("APIC: Switched from PIC to APIC mode");
}
