// interrupts.rs — IDT (Interrupt Descriptor Table) と例外ハンドラ
//
// IDT は CPU に「割り込みや例外が起きたらどの関数を呼ぶか」を教えるテーブル。
// x86_64 では 256 個のエントリがあり、0〜31 番が CPU 例外、32〜255 番が
// ハードウェア割り込みやソフトウェア割り込みに使われる。
//
// 例外ハンドラが設定されていないと、例外 → ダブルフォルト → トリプルフォルト
// → CPU リセット（無言の再起動）という連鎖が起きる。
// ハンドラを設定しておけば「何が起きたか」を画面に表示して安全に停止できる。

use lazy_static::lazy_static;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::gdt;

lazy_static! {
    /// IDT (Interrupt Descriptor Table)
    /// CPU の各例外に対応するハンドラ関数を登録する。
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();

        // --- CPU 例外ハンドラの登録 ---

        // #DE: 除算エラー（ゼロ除算など）
        idt.divide_error.set_handler_fn(divide_error_handler);

        // #DB: デバッグ例外
        idt.debug.set_handler_fn(debug_handler);

        // #BP: ブレークポイント（int3 命令）
        // デバッグ用。意図的に発生させてテストできる。
        idt.breakpoint.set_handler_fn(breakpoint_handler);

        // #UD: 不正オペコード（CPU が理解できない命令を実行しようとした）
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);

        // #GP: 一般保護違反（特権違反、不正なメモリアクセス等）
        idt.general_protection_fault.set_handler_fn(general_protection_fault_handler);

        // #PF: ページフォルト（マッピングされていないメモリへのアクセス等）
        idt.page_fault.set_handler_fn(page_fault_handler);

        // #DF: ダブルフォルト（例外ハンドラ実行中に別の例外が起きた場合）
        // これが最後の砦。ここでも失敗するとトリプルフォルト → CPU リセット。
        // IST（専用スタック）を使うことで、スタック破壊時でも安全に動く。
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }

        idt
    };
}

/// IDT を初期化して CPU にロードする。
/// GDT の初期化後に呼ぶこと（IST を使うため TSS が必要）。
pub fn init() {
    IDT.load();
}

// =================================================================
// 例外ハンドラの実装
// =================================================================
//
// 各ハンドラは x86_64 の割り込み呼び出し規約 (x86-interrupt) に従う。
// 第1引数の InterruptStackFrame には例外発生時の RIP, RSP, RFLAGS 等が入っている。
// エラーコード付きの例外（GPF, PF, DF等）は第2引数にエラーコードが渡される。
//
// 注意: ここでは画面への直接描画はまだ行わず、hlt ループで停止する。
// フレームバッファへの出力は将来的に panic ハンドラ経由で行う。

extern "x86-interrupt" fn divide_error_handler(stack_frame: InterruptStackFrame) {
    panic!("CPU EXCEPTION: DIVIDE ERROR (#DE)\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn debug_handler(stack_frame: InterruptStackFrame) {
    panic!("CPU EXCEPTION: DEBUG (#DB)\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    // ブレークポイントは致命的ではないので panic しない。
    // ただし今はシリアルもフレームバッファもハンドラから使えないので、
    // とりあえず何もせず戻る。テストで使う。
    let _ = stack_frame;
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    panic!("CPU EXCEPTION: INVALID OPCODE (#UD)\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    panic!(
        "CPU EXCEPTION: GENERAL PROTECTION FAULT (#GP)\nError code: {}\n{:#?}",
        error_code, stack_frame
    );
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // CR2 レジスタにはページフォルトを起こしたアドレスが入っている。
    use x86_64::registers::control::Cr2;
    panic!(
        "CPU EXCEPTION: PAGE FAULT (#PF)\nAccessed address: {:?}\nError code: {:?}\n{:#?}",
        Cr2::read(),
        error_code,
        stack_frame
    );
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    // ダブルフォルトは回復不能。情報を表示して停止する。
    // IST で専用スタックに切り替わっているので、ここまでは来れるはず。
    panic!(
        "CPU EXCEPTION: DOUBLE FAULT (#DF)\nError code: {}\n{:#?}",
        error_code, stack_frame
    );
}
