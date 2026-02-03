// gdt.rs — GDT (Global Descriptor Table) と TSS (Task State Segment) のセットアップ
//
// GDT は CPU に「メモリセグメントのルール」を教えるテーブル。
// x86_64 のロングモードではセグメンテーションはほぼ無効化されているが、
// 最低限「カーネルのコードセグメント」「データセグメント」の定義と
// TSS の登録が必要。
//
// Ring 3（ユーザーモード）をサポートするため、ユーザーコード/データセグメントも
// GDT に登録する。Ring 3 のコードが int 0x80 等でカーネルに制御を移すとき、
// CPU は TSS の privilege_stack_table[0] (rsp0) に自動的にスタックを切り替える。
//
// TSS はもともと「タスク切り替え用の構造体」だったが、
// x86_64 では主に以下の用途で使われる:
//   - IST (Interrupt Stack Table): 例外発生時に使う別スタックの指定
//   - rsp0: Ring 3 → Ring 0 遷移時のカーネルスタックアドレス

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

/// TSS (Task State Segment)
///
/// `static mut` にしている理由:
///   Ring 3（ユーザーモード）で int 0x80 等を呼んで Ring 0 に遷移するとき、
///   CPU は TSS の `privilege_stack_table[0]`（通称 rsp0）のアドレスに
///   自動的にスタックポインタを切り替える。
///   ユーザータスクごとにカーネルスタックを変えたいので、rsp0 を実行時に
///   書き換える必要がある。そのため不変の `lazy_static!` ではなく `static mut` を使う。
static mut TSS: TaskStateSegment = TaskStateSegment::new();

lazy_static! {
    /// GDT とセグメントセレクタ。
    ///
    /// GDT エントリ配置:
    ///   0: Null（CPU が要求する空エントリ）
    ///   1: Kernel Code (Ring 0) — カーネルの実行コード用
    ///   2: Kernel Data (Ring 0) — カーネルのデータ/スタック用
    ///   3: User Data (Ring 3)   — ユーザープログラムのデータ/スタック用
    ///   4: User Code (Ring 3)   — ユーザープログラムの実行コード用
    ///   5-6: TSS (2エントリ分)  — タスク状態セグメント（SystemSegment は 2 エントリ消費）
    ///   合計: 7 / 8 エントリ
    ///
    /// ユーザーデータセグメントをユーザーコードセグメントの前に置くのは、
    /// sysret 命令の規約に合わせるため（将来の最適化に備える）。
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();

        // カーネルモードのコードセグメント（Ring 0, 64-bit）
        let kernel_code_selector = gdt.append(Descriptor::kernel_code_segment());
        // カーネルモードのデータセグメント（Ring 0）
        let kernel_data_selector = gdt.append(Descriptor::kernel_data_segment());
        // ユーザーモードのデータセグメント（Ring 3）
        let _user_data_selector = gdt.append(Descriptor::user_data_segment());
        // ユーザーモードのコードセグメント（Ring 3, 64-bit）
        let user_code_selector = gdt.append(Descriptor::user_code_segment());

        // TSS セグメント（CPU に TSS の場所を教える）
        // static mut な TSS を raw pointer で渡す（tss_segment_unchecked は unsafe）
        let tss_selector = unsafe {
            gdt.append(Descriptor::tss_segment_unchecked(&raw const TSS))
        };

        (gdt, Selectors {
            kernel_code_selector,
            kernel_data_selector,
            user_code_selector,
            tss_selector,
        })
    };
}

/// GDT に登録したセグメントのセレクタ（インデックス）を保持する。
/// GDT をロードした後、CPU のセグメントレジスタをこれらの値に設定する。
struct Selectors {
    kernel_code_selector: SegmentSelector,
    kernel_data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

/// GDT と TSS を初期化して CPU にロードする。
///
/// これを呼ぶと:
/// 1. TSS にダブルフォルト用 IST スタックを設定
/// 2. GDT を CPU の GDTR レジスタに設定（lgdt 命令）
/// 3. コードセグメント (CS) とデータセグメント (SS) を新しいセレクタに切り替え
/// 4. TSS をロード（ltr 命令）して IST が有効になる
pub fn init() {
    use x86_64::instructions::segmentation::{CS, DS, ES, Segment, SS};
    use x86_64::instructions::tables::load_tss;

    // TSS にダブルフォルト用スタックを設定する。
    // static mut へのアクセスなので unsafe。GDT の lazy_static 初期化前に行う。
    unsafe {
        let tss = &raw mut TSS;
        (*tss).interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            let stack_start = &raw const DOUBLE_FAULT_STACK as u64;
            let stack_end = stack_start + DOUBLE_FAULT_STACK_SIZE as u64;
            VirtAddr::new(stack_end)
        };
    }

    GDT.0.load();
    unsafe {
        // CS (Code Segment) の切り替えは特殊で、far return を使って行われる。
        // x86_64 crate が内部でよろしくやってくれる。
        CS::set_reg(GDT.1.kernel_code_selector);
        // SS, DS, ES はゼロか同じデータセレクタを設定。
        SS::set_reg(GDT.1.kernel_data_selector);
        DS::set_reg(GDT.1.kernel_data_selector);
        ES::set_reg(SegmentSelector(0));
        // TSS をロード。これで IST（ダブルフォルト用スタック）が有効になる。
        load_tss(GDT.1.tss_selector);
    }
}

/// ユーザーモードのコードセグメントセレクタを返す。
/// iretq でユーザーモードに遷移する際、CS レジスタにセットする値。
/// RPL=3（Ring 3）が自動的に含まれている（GDT の append が DPL から設定する）。
pub fn user_code_selector() -> SegmentSelector {
    GDT.1.user_code_selector
}

/// TSS の rsp0（privilege_stack_table[0]）を設定する。
///
/// Ring 3 → Ring 0 への遷移時（int 命令、例外など）、CPU は自動的に
/// TSS の rsp0 が指すアドレスにスタックポインタを切り替える。
/// ユーザータスクを実行する前に、そのタスク用のカーネルスタックの
/// トップアドレスをここに設定する必要がある。
///
/// # Safety
/// - rsp0 は有効なスタック領域のトップアドレスでなければならない
/// - 16 バイトアラインされていること
pub unsafe fn set_tss_rsp0(rsp0: VirtAddr) {
    let tss = &raw mut TSS;
    unsafe {
        (*tss).privilege_stack_table[0] = rsp0;
    }
}
