// gdt.rs — GDT (Global Descriptor Table) と TSS (Task State Segment) のセットアップ
//
// GDT は CPU に「メモリセグメントのルール」を教えるテーブル。
// x86_64 のロングモードではセグメンテーションはほぼ無効化されているが、
// 最低限「カーネルのコードセグメント」「データセグメント」の定義と
// TSS の登録が必要。
//
// TSS はもともと「タスク切り替え用の構造体」だったが、
// x86_64 では主に「例外発生時に使う別スタックの指定」に使われる。
// 特にダブルフォルト時に現在のスタックが壊れている可能性があるため、
// 専用の安全なスタック（IST: Interrupt Stack Table）を用意する。

use lazy_static::lazy_static;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// ダブルフォルトハンドラ用の IST インデックス。
/// IDT でダブルフォルトエントリに set_stack_index(0) で指定する。
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// ダブルフォルトハンドラ用スタックのサイズ（20KiB = 5ページ）。
/// 通常のカーネルスタックは数十KiB〜数百KiBだが、例外ハンドラ用はこれで十分。
const DOUBLE_FAULT_STACK_SIZE: usize = 4096 * 5;

/// ダブルフォルトハンドラ用の専用スタック。
/// 通常のスタックが壊れていても安全に動けるよう、別のメモリ領域を確保する。
static mut DOUBLE_FAULT_STACK: [u8; DOUBLE_FAULT_STACK_SIZE] = [0; DOUBLE_FAULT_STACK_SIZE];

lazy_static! {
    /// TSS (Task State Segment)
    /// x86_64 では主に IST（割り込みスタックテーブル）のために使う。
    /// ダブルフォルト発生時に自動的に切り替わる専用スタックを登録する。
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        // IST の 0 番目にダブルフォルト用スタックのトップアドレスを設定。
        // スタックは上位アドレスから下位に向かって伸びるので、配列の末尾がトップ。
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            // Rust 2024 では static mut への共有参照が禁止されたので &raw const を使う。
            let stack_start = &raw const DOUBLE_FAULT_STACK as u64;
            let stack_end = stack_start + DOUBLE_FAULT_STACK_SIZE as u64;
            VirtAddr::new(stack_end)
        };
        tss
    };

    /// GDT とセグメントセレクタ。
    /// カーネルのコードセグメント・データセグメント・TSS セグメントを登録する。
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        // カーネルモードのコードセグメント（Ring 0, 64-bit）
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        // カーネルモードのデータセグメント（Ring 0）
        let data_selector = gdt.append(Descriptor::kernel_data_segment());
        // TSS セグメント（CPU に TSS の場所を教える）
        let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));
        (gdt, Selectors { code_selector, data_selector, tss_selector })
    };
}

/// GDT に登録したセグメントのセレクタ（インデックス）を保持する。
/// GDT をロードした後、CPU のセグメントレジスタをこれらの値に設定する。
struct Selectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

/// GDT と TSS を初期化して CPU にロードする。
///
/// これを呼ぶと:
/// 1. GDT を CPU の GDTR レジスタに設定（lgdt 命令）
/// 2. コードセグメント (CS) とデータセグメント (SS) を新しいセレクタに切り替え
/// 3. TSS をロード（ltr 命令）して IST が有効になる
pub fn init() {
    use x86_64::instructions::segmentation::{CS, DS, ES, Segment, SS};
    use x86_64::instructions::tables::load_tss;

    GDT.0.load();
    unsafe {
        // CS (Code Segment) の切り替えは特殊で、far return を使って行われる。
        // x86_64 crate が内部でよろしくやってくれる。
        CS::set_reg(GDT.1.code_selector);
        // SS, DS, ES はゼロか同じデータセレクタを設定。
        SS::set_reg(GDT.1.data_selector);
        DS::set_reg(GDT.1.data_selector);
        ES::set_reg(SegmentSelector(0));
        // TSS をロード。これで IST（ダブルフォルト用スタック）が有効になる。
        load_tss(GDT.1.tss_selector);
    }
}
