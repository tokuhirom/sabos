// interrupts.rs — IDT (Interrupt Descriptor Table) と割り込みハンドラ
//
// IDT は CPU に「割り込みや例外が起きたらどの関数を呼ぶか」を教えるテーブル。
// x86_64 では 256 個のエントリがあり、0〜31 番が CPU 例外、32〜255 番が
// ハードウェア割り込みやソフトウェア割り込みに使われる。
//
// 例外ハンドラが設定されていないと、例外 → ダブルフォルト → トリプルフォルト
// → CPU リセット（無言の再起動）という連鎖が起きる。
// ハンドラを設定しておけば「何が起きたか」を画面に表示して安全に停止できる。
//
// ハードウェア割り込みは PIC (8259) 経由で CPU に届く。
// PIC が IRQ 0〜15 を IDT の 32〜47 番にマッピングする。

use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::gdt;

/// タイマー割り込みが発火した回数。
/// プリエンプティブスケジューリングの動作確認や、システムの稼働時間の目安に使える。
pub static TIMER_TICK_COUNT: AtomicU64 = AtomicU64::new(0);

// =================================================================
// キー入力キュー
// =================================================================
//
// キーボード割り込みハンドラが受け取った文字をキューに溜めて、
// メインループ（シェル）がそこから読み取る。
// 割り込みハンドラから直接画面に書くのではなく、キューを介することで
// 表示ロジック（エコーバック、行編集等）をメインコード側に置ける。

lazy_static! {
    /// キー入力キュー。キーボードハンドラが push、シェルが pop する。
    static ref KEY_QUEUE: spin::Mutex<VecDeque<char>> =
        spin::Mutex::new(VecDeque::new());
}

/// キューから1文字取り出す。キューが空なら None を返す。
/// 割り込みを無効化してからロックを取ることで、
/// キーボードハンドラとのデッドロックを防ぐ。
pub fn get_key() -> Option<char> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        KEY_QUEUE.lock().pop_front()
    })
}

// =================================================================
// PIC (Programmable Interrupt Controller) の設定
// =================================================================
//
// PC には 8259 PIC が 2 つカスケード接続されている:
//   マスタ PIC: IRQ 0〜7（タイマー、キーボード等）
//   スレーブ PIC: IRQ 8〜15（マウス、ディスク等）
//
// BIOS/UEFI のデフォルトでは IRQ 0〜15 が IDT の 0〜15 番にマッピングされており、
// CPU 例外（0〜31 番）と衝突する。そこで PIC を再プログラムして
// IRQ 0〜15 を IDT の 32〜47 番にずらす（リマップ）。

/// マスタ PIC の割り込みオフセット。IRQ 0 → IDT 32 番。
pub const PIC_1_OFFSET: u8 = 32;
/// スレーブ PIC の割り込みオフセット。IRQ 8 → IDT 40 番。
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

/// PIC のグローバルインスタンス。
/// ChainedPics はマスタとスレーブの 2 つの PIC をまとめて管理する。
/// spin::Mutex で排他制御（割り込みハンドラから EOI を送る必要があるため）。
pub static PICS: spin::Mutex<ChainedPics> =
    spin::Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

/// ハードウェア割り込みの番号。
/// PIC_1_OFFSET (32) を基準に、IRQ 番号分だけ足した値が IDT のエントリ番号になる。
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    /// IRQ 0: タイマー (PIT: Programmable Interval Timer)
    /// 約 18.2 Hz でデフォルト発火する。OS のハートビート。
    Timer = PIC_1_OFFSET,
    /// IRQ 1: キーボード (PS/2)
    /// キーが押された/離されたときに発火する。
    Keyboard,
    /// IRQ 12: マウス (PS/2)
    /// マウスのパケット受信時に発火する。
    Mouse = PIC_2_OFFSET + 4,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

lazy_static! {
    /// IDT (Interrupt Descriptor Table)
    /// CPU 例外とハードウェア割り込みのハンドラ関数を登録する。
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();

        // --- CPU 例外ハンドラの登録 (0〜31番) ---

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

        // --- ハードウェア割り込みハンドラの登録 (32番〜) ---

        // IRQ 0: タイマー割り込み
        idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);

        // IRQ 1: キーボード割り込み
        idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
        // IRQ 12: マウス割り込み
        idt[InterruptIndex::Mouse.as_u8()].set_handler_fn(mouse_interrupt_handler);

        // --- ソフトウェア割り込み: システムコール (int 0x80) ---
        //
        // int 0x80 はユーザーモード (Ring 3) からカーネル (Ring 0) への
        // システムコール呼び出しに使うソフトウェア割り込み。
        // Linux の伝統的な 32-bit システムコールインターフェースと同じ番号。
        //
        // set_handler_addr() で生のアドレスを設定する（x86-interrupt ABI ではなく
        // 独自のアセンブリハンドラを使うため）。
        // set_privilege_level(Ring3) で DPL=3 にする。
        // DPL=3 にしないと、Ring 3 から int 0x80 を実行したとき
        // General Protection Fault (#GP) が発生する。
        unsafe {
            idt[0x80].set_handler_addr(x86_64::VirtAddr::new(
                crate::syscall::syscall_handler_asm as *const () as u64
            ))
            .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
        }

        idt
    };
}

/// IDT を初期化して CPU にロードする。
/// GDT の初期化後に呼ぶこと（IST を使うため TSS が必要）。
pub fn init() {
    IDT.load();

    // PIC を初期化する。
    // IRQ 0〜15 が IDT の 32〜47 番にマッピングされるようにリマップする。
    unsafe {
        let mut pics = PICS.lock();
        pics.initialize();

        // PIC のマスクを明示的に設定する。
        // pic8259 の initialize() は初期化前のマスクを復元するが、
        // UEFI が IRQ をマスクしている場合がある（特に IRQ 0 タイマー）。
        // ここで明示的にタイマー (IRQ 0) とキーボード (IRQ 1) をアンマスクする。
        //
        // マスクは各ビットが 1 = マスク（無効）、0 = アンマスク（有効）。
        // マスタ PIC: bit 0 = IRQ 0 (タイマー), bit 1 = IRQ 1 (キーボード),
        //             bit 2 = IRQ 2 (スレーブ連携)
        //   0b11111000 → IRQ 0, 1, 2 のみ有効
        // スレーブ PIC: bit 4 = IRQ 12 (マウス)
        //   0b11101111 → IRQ 12 のみ有効
        pics.write_masks(0b11111000, 0b11101111);
    }
}

// =================================================================
// CPU 例外ハンドラの実装 (0〜31番)
// =================================================================
//
// 各ハンドラは x86_64 の割り込み呼び出し規約 (x86-interrupt) に従う。
// 第1引数の InterruptStackFrame には例外発生時の RIP, RSP, RFLAGS 等が入っている。
// エラーコード付きの例外（GPF, PF, DF等）は第2引数にエラーコードが渡される。

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

    // Ring 3（ユーザーモード）からの不正アクセスかどうかを判定する。
    // PageFaultErrorCode の USER_MODE ビットが立っていれば Ring 3 からのアクセス。
    // この場合はカーネルパニックではなく、ユーザープログラムを強制終了して
    // カーネル（シェル）に安全に戻る。
    if error_code.contains(PageFaultErrorCode::USER_MODE) {
        crate::kprintln!();
        crate::kprintln!("Page fault in user mode!");
        crate::kprintln!("  Accessed address: {:?}", Cr2::read());
        crate::kprintln!("  Error code: {:?}", error_code);
        crate::scheduler::with_current_task(|task| {
            crate::kprintln!("  Task: {} ({})", task.id, task.name);
        });
        crate::kprintln!("  RIP: {:?}", stack_frame.instruction_pointer);
        crate::kprintln!("  RSP: {:?}", stack_frame.stack_pointer);
        crate::kprintln!("  Terminating user program...");
        // ユーザーモード例外は現在のユーザータスクを終了させて
        // 他のタスクに切り替える。割り込みハンドラ内なので
        // exit_usermode() の longjmp は使わない。
        crate::scheduler::abort_current_user_task_from_exception();
    }

    // Ring 0（カーネル）からのページフォルトは回復不能なので panic する。
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

// =================================================================
// ハードウェア割り込みハンドラの実装 (32番〜)
// =================================================================
//
// ハードウェア割り込みは CPU 例外と違って、処理後に PIC に
// EOI (End Of Interrupt) を送る必要がある。
// EOI を送らないと PIC は「まだ処理中」と判断して、
// 同じ優先度以下の割り込みをブロックし続ける。

/// IRQ 0: タイマー割り込みハンドラ。
/// PIT (Programmable Interval Timer) から約 18.2 Hz で発火する。
///
/// プリエンプティブマルチタスクの心臓部:
///   1. EOI を先に送る（context_switch 後も他タスクがタイマー割り込みを受け取れるように）
///   2. scheduler::preempt() で次の Ready タスクに強制切り替え
///
/// EOI を context_switch の前に送る理由:
///   context_switch で別タスクに切り替わると、このハンドラの残りのコードは
///   「このタスクが再スケジュールされるまで」実行されない。
///   EOI を送らずに切り替えると、PIC がタイマー割り込みをブロックし続け、
///   切り替え先タスクがタイマー割り込みを受け取れなくなる。
extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    TIMER_TICK_COUNT.fetch_add(1, Ordering::Relaxed);

    // EOI を先に送る（プリエンプション前に PIC をクリアする）
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }

    // プリエンプティブスケジューリング:
    // 現在のタスクを中断して、次の Ready タスクに切り替える。
    // try_lock() を使うので、SCHEDULER がロック中なら何もせずスキップする。
    crate::scheduler::preempt();
}

/// IRQ 1: キーボード割り込みハンドラ。
/// PS/2 キーボードからスキャンコードが I/O ポート 0x60 に届く。
/// スキャンコードを読み取って文字に変換し、入力バッファに追加する。
///
/// 入力は2箇所に通知される:
/// 1. KEY_QUEUE — カーネルシェル用（後方互換性）
/// 2. console::push_input_char() — ユーザー空間 SYS_READ 用
extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use pc_keyboard::{layouts, DecodedKey, HandleControl, Keyboard, ScancodeSet1};
    use x86_64::instructions::port::Port;

    // キーボードの状態をグローバルに保持する。
    // Keyboard 構造体がスキャンコードのステートマシンを管理する
    // （例: Shift が押されているか、マルチバイトシーケンスの途中か等）。
    lazy_static! {
        static ref KEYBOARD: spin::Mutex<Keyboard<layouts::Us104Key, ScancodeSet1>> =
            spin::Mutex::new(Keyboard::new(
                ScancodeSet1::new(),
                layouts::Us104Key,
                HandleControl::Ignore,
            ));
    }

    let mut keyboard = KEYBOARD.lock();

    // I/O ポート 0x60 からスキャンコードを読み取る。
    // PS/2 キーボードコントローラはこのポートにスキャンコードを置く。
    // 読み取らないと次の割り込みが来なくなる。
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };

    // pc-keyboard crate でスキャンコードをキーイベントに変換する。
    // add_byte() でスキャンコードを投入し、process_keyevent() で
    // キーの押下/解放を処理する。
    if let Ok(Some(key_event)) = keyboard.add_byte(scancode) {
        if let Some(key) = keyboard.process_keyevent(key_event) {
            match key {
                DecodedKey::Unicode(character) => {
                    // 文字をキー入力キューに追加する（カーネルシェル用）
                    KEY_QUEUE.lock().push_back(character);
                    // コンソール入力バッファにも追加（ユーザー空間 SYS_READ 用）
                    crate::console::push_input_char(character);
                }
                DecodedKey::RawKey(key) => {
                    // 特殊キー（矢印キー、F1-F12等）は今は無視。
                    // 将来的にはシェルのカーソル移動等に使う。
                    let _ = key;
                }
            }
        }
    }

    // PIC に EOI を送る。
    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}

/// IRQ 12: マウス割り込みハンドラ。
/// PS/2 マウスからの 1 バイトを読み取り、パケット組み立てへ渡す。
extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use x86_64::instructions::port::Port;

    let byte = unsafe { Port::<u8>::new(0x60).read() };
    crate::mouse::handle_irq_byte(byte);

    unsafe {
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Mouse.as_u8());
    }
}
