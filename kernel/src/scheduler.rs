// scheduler.rs — 協調的マルチタスクスケジューラ
//
// カーネルタスク（軽量スレッド）を管理する。
// 各タスクは独自のスタックとコンテキスト（レジスタ保存領域）を持ち、
// yield_now() で自発的に CPU を次のタスクに譲る（協調的マルチタスク）。
//
// コンテキストスイッチはアセンブリで実装:
//   1. 現在のタスクの callee-saved レジスタをスタックに push
//   2. スタックポインタ (rsp) を切り替え
//   3. 新しいタスクの callee-saved レジスタをスタックから pop
//   4. ret で新しいタスクの実行を再開
//
// x86_64-unknown-uefi ターゲットは Microsoft x64 ABI を使う。
// callee-saved レジスタ: rbx, rbp, rdi, rsi, r12, r13, r14, r15

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::structures::paging::{PhysFrame, Size4KiB};
use x86_64::VirtAddr;

/// preempt() が呼ばれた回数（タイマー割り込みごとに 1 回）。
static PREEMPT_CALL_COUNT: AtomicU64 = AtomicU64::new(0);
/// preempt() で実際にコンテキストスイッチした回数。
static PREEMPT_SWITCH_COUNT: AtomicU64 = AtomicU64::new(0);

/// preempt() の統計情報を返す（呼び出し回数, スイッチ回数）。
pub fn preempt_stats() -> (u64, u64) {
    (
        PREEMPT_CALL_COUNT.load(Ordering::Relaxed),
        PREEMPT_SWITCH_COUNT.load(Ordering::Relaxed),
    )
}

/// タスクのスタックサイズ（16 KiB）。
/// カーネルタスクなので大きなスタックは不要だが、
/// kprintln! 等のフォーマット処理がスタックを使うのである程度必要。
const TASK_STACK_SIZE: usize = 4096 * 4;

// =================================================================
// コンテキストスイッチ（アセンブリ）
// =================================================================
//
// context_switch(old_rsp_ptr: *mut u64, new_rsp: u64, new_cr3: u64)
// context_switch_enable(old_rsp_ptr: *mut u64, new_rsp: u64, new_cr3: u64)
//   rcx = old_rsp_ptr: 現在のタスクの rsp を保存する場所へのポインタ
//   rdx = new_rsp: 切り替え先タスクの rsp
//   r8  = new_cr3: 切り替え先タスクの CR3（ページテーブルの物理アドレス）
//
// Microsoft x64 ABI では第1引数が rcx、第2引数が rdx、第3引数が r8 に入る。
//
// 処理の流れ:
//   1. 現在の callee-saved レジスタをスタックに push（8個 = 64バイト）
//   2. 現在の rsp を [rcx] に保存
//   3. rsp を rdx の値に切り替え
//   4. CR3 を r8 の値に切り替え（TLB 自動フラッシュ）
//   5. 新しいスタックから callee-saved レジスタを pop
//   6. ret で新しいタスクの実行を再開
//
// CR3 の切り替えはスタック切り替え後に行う。これにより、新しいタスクの
// アドレス空間で以降の処理が実行される。カーネルマッピングは全タスクで
// 共有されているので、CR3 切り替え後もカーネルコードは正常に動作する。
//
// 新しいタスクの場合、ret は task_trampoline（カーネルタスク）または
// user_task_trampoline（ユーザープロセス）にジャンプする。
// 既存タスクの場合、ret は前回 context_switch を呼んだ箇所に戻る。
global_asm!(
    "context_switch:",
    "push rbp",
    "push rbx",
    "push rdi",
    "push rsi",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov [rcx], rsp",   // 現在の rsp を保存
    "mov rsp, rdx",     // 新しい rsp に切り替え
    "mov cr3, r8",      // CR3 を切り替え（TLB フラッシュ）
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rsi",
    "pop rdi",
    "pop rbx",
    "pop rbp",
    "ret",
);

global_asm!(
    "context_switch_enable:",
    "push rbp",
    "push rbx",
    "push rdi",
    "push rsi",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov [rcx], rsp",   // 現在の rsp を保存
    "mov rsp, rdx",     // 新しい rsp に切り替え
    "mov cr3, r8",      // CR3 を切り替え（TLB フラッシュ）
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rsi",
    "pop rdi",
    "pop rbx",
    "pop rbp",
    "sti",              // 協調的 yield のみ割り込みを再有効化
    "ret",
);

// =================================================================
// タスクトランポリン（アセンブリ）
// =================================================================
//
// 新しいタスクが初めてスケジュールされたとき、
// context_switch の ret がここにジャンプする。
//
// r12 にはタスクのエントリ関数のアドレスが入っている
// （タスク作成時にスタック上の r12 保存位置に設定済み）。
//
// 処理の流れ:
//   1. sti で割り込みを有効化する
//      （yield_now() や preempt() は割り込み無効状態で context_switch するため、
//        新しいタスクが初めて実行される時点では割り込みが無効のまま。
//        sti しないとタイマー割り込みが発火せず、プリエンプションが機能しない。）
//   2. スタックを整えてシャドウスペースを確保（Microsoft x64 ABI 要件）
//   3. r12 のエントリ関数を呼び出す
//   4. エントリ関数が return したら task_exit_handler を呼んでタスクを終了
//
// アライメント:
//   context_switch の ret でここに来た時点で rsp は 16n+8（関数エントリ規約）。
//   call r12 の前に sub rsp, 40 (32 シャドウ + 8 アライメント) で
//   rsp を 16 バイトアラインにする。
global_asm!(
    "task_trampoline:",
    "sti",            // 割り込みを有効化（プリエンプションに必要）
    "sub rsp, 40",
    "call r12",
    "add rsp, 40",
    "sub rsp, 40",
    "call {exit}",
    "ud2",
    exit = sym task_exit_handler,
);

unsafe extern "C" {
    /// アセンブリで実装されたコンテキストスイッチ関数。
    /// new_cr3 には切り替え先タスクの CR3 値（ページテーブルの物理アドレス）を渡す。
    fn context_switch(old_rsp_ptr: *mut u64, new_rsp: u64, new_cr3: u64);
    fn context_switch_enable(old_rsp_ptr: *mut u64, new_rsp: u64, new_cr3: u64);
}

/// タスクのエントリ関数が return した後に呼ばれるハンドラ。
/// 現在のタスクを Finished に設定して、他のタスクに切り替える。
#[unsafe(no_mangle)]
extern "C" fn task_exit_handler() {
    {
        let mut sched = SCHEDULER.lock();
        let current = sched.current;
        sched.tasks[current].state = TaskState::Finished;
    }
    // 他のタスクに切り替える
    yield_now();
    // ここに戻ることはないはず（Finished タスクはスケジュールされない）
    loop {
        x86_64::instructions::hlt();
    }
}

// =================================================================
// タスク定義
// =================================================================

/// タスクのスタックポインタを保持するコンテキスト。
///
/// callee-saved レジスタはスタック上に push/pop されるので、
/// コンテキスト自体には rsp だけ保存すればよい。
/// context_switch がスタック上のレジスタ値を管理する。
#[repr(C)]
pub struct Context {
    /// スタックポインタ。context_switch で保存/復帰される。
    pub rsp: u64,
}

/// タスクの状態。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaskState {
    /// 実行可能。スケジューラに選ばれるのを待っている。
    Ready,
    /// 現在実行中。
    Running,
    /// スリープ中。指定したティック数に達するまでスケジュールされない。
    /// 中の値は起床するタイマーティック数（TIMER_TICK_COUNT がこの値以上になったら Ready に戻る）。
    Sleeping(u64),
    /// 実行完了。もうスケジュールされない。
    Finished,
}

/// タスクの情報（外部からの参照用、ps コマンド等で使う）。
pub struct TaskInfo {
    pub id: u64,
    pub name: String,
    pub state: TaskState,
    /// ユーザープロセスかどうか
    pub is_user_process: bool,
}

/// メモリ使用量の情報（procfs 用の簡易統計）。
pub struct ProcessMemInfo {
    pub id: u64,
    pub name: String,
    pub is_user_process: bool,
    /// そのプロセスが確保したユーザー空間フレーム数（ざっくり）。
    pub user_frames: usize,
}

/// ユーザープロセスの情報を保持する構造体。
/// spawn_user() でユーザープロセスをタスクとして登録する際に使う。
pub struct UserProcessInfo {
    /// ユーザープロセスの状態（ページテーブル、カーネルスタック、確保フレーム）
    pub process: crate::usermode::UserProcess,
    /// ELF のエントリポイント仮想アドレス
    pub entry_point: u64,
    /// ユーザースタックのトップアドレス
    pub user_stack_top: u64,
    /// 初回遷移済みフラグ。false なら user_task_trampoline で初めて Ring 3 に遷移する。
    /// true ならプリエンプション後の復帰（iretq で自然に戻る）。
    pub first_run_done: bool,
}

/// カーネルタスク。
///
/// 各タスクは独自のスタックとコンテキスト（rsp）を持つ。
/// コンテキストスイッチではスタックポインタを切り替えることで、
/// タスクの実行状態を丸ごと切り替える。
///
/// ユーザープロセスの場合、user_process_info と cr3 が設定される。
/// カーネルタスクの場合は両方 None で、カーネルの CR3 を使う。
pub struct Task {
    /// タスク ID（一意）
    pub id: u64,
    /// タスク名（デバッグ・表示用）。ユーザープロセスは動的に名前が決まるので String。
    pub name: String,
    /// タスクの状態
    pub state: TaskState,
    /// コンテキスト（スタックポインタ）
    pub context: Context,
    /// タスク用のスタック領域。None はブートスタック（task 0）を使う。
    /// Box<[u8]> で安定したアドレスを保証する。
    _stack: Option<alloc::boxed::Box<[u8]>>,
    /// ユーザープロセスの場合、プロセス固有のページテーブルフレーム。
    /// カーネルタスクの場合は None（カーネルの CR3 を使う）。
    pub cr3: Option<PhysFrame<Size4KiB>>,
    /// ユーザープロセスの情報。カーネルタスクの場合は None。
    /// 終了後は take() されるので、is_user フラグで元の種別を保持する。
    pub user_process_info: Option<UserProcessInfo>,
    /// ユーザープロセスかどうか（終了後も保持するフラグ）
    pub is_user: bool,
    /// 親タスクの ID。カーネルタスク (task 0) や最初のユーザープロセス (init) は None。
    /// spawn_user() で子プロセスを作成した場合、呼び出し元のタスク ID が設定される。
    pub parent_id: Option<u64>,
    /// プロセスの終了コード。exit() で設定され、wait() で取得できる。
    /// Finished 状態になった時点で有効な値を持つ。
    pub exit_code: i32,
    /// wait() 済みかどうか（同じ終了を繰り返し返さないためのフラグ）
    pub reaped: bool,
}

// =================================================================
// スケジューラ
// =================================================================

/// ラウンドロビンスケジューラ。
///
/// タスクのリストを保持し、yield_now() が呼ばれるたびに
/// 次の Ready タスクに切り替える。
struct Scheduler {
    /// 全タスクのリスト
    tasks: Vec<Task>,
    /// 現在実行中のタスクのインデックス
    current: usize,
    /// 次に割り当てるタスク ID
    next_id: u64,
}

impl Scheduler {
    const fn new() -> Self {
        Self {
            tasks: Vec::new(),
            current: 0,
            next_id: 0,
        }
    }
}

/// グローバルスケジューラ。
static SCHEDULER: Mutex<Scheduler> = Mutex::new(Scheduler::new());

// =================================================================
// 公開 API
// =================================================================

/// スケジューラを初期化する。
///
/// 現在の実行コンテキストを task 0 ("kernel") として登録する。
/// task 0 はブートスタックを使い、シェルのメインループを実行する。
/// 他のすべての初期化が完了した後、タスクを spawn する前に呼ぶこと。
pub fn init() {
    let mut sched = SCHEDULER.lock();
    let id = sched.next_id;
    sched.next_id += 1;

    sched.tasks.push(Task {
        id,
        name: String::from("kernel"),
        state: TaskState::Running,
        context: Context { rsp: 0 }, // 最初の yield_now() で実際の rsp が保存される
        _stack: None,                 // ブートスタックを使用
        cr3: None,                    // カーネルの CR3 を使用
        user_process_info: None,      // カーネルタスク
        is_user: false,               // カーネルタスク
        parent_id: None,              // カーネルタスクに親はいない
        exit_code: 0,                 // 初期値
        reaped: false,                // wait() は不要
    });
    sched.current = 0;
}

/// 新しいタスクを作成してスケジューラに登録する。
///
/// entry はタスクのエントリ関数。通常の fn() で、return すると
/// task_trampoline 経由で task_exit_handler が呼ばれ、
/// タスクは自動的に Finished になる。
///
/// タスク内で yield_now() を呼ぶことで他のタスクに CPU を譲れる。
pub fn spawn(name: &'static str, entry: fn()) {
    let mut sched = SCHEDULER.lock();
    let id = sched.next_id;
    sched.next_id += 1;

    // --- タスク用スタックの確保 ---
    // Box<[u8]> で確保してアドレスの安定性を保証する。
    // Vec だと push 時にリアロケートされるとアドレスが変わる危険がある。
    let stack = vec![0u8; TASK_STACK_SIZE].into_boxed_slice();
    let stack_bottom = stack.as_ptr() as u64;
    let stack_top = stack_bottom + TASK_STACK_SIZE as u64;
    // 16 バイトアライメント（x86_64 の要件）
    let stack_top = stack_top & !0xF;

    // --- 初期スタックの設定 ---
    //
    // context_switch が期待するレイアウトに合わせて、
    // スタック上に初期値を書き込む。スタックは上位→下位に成長する。
    //
    // スタックレイアウト（上位アドレスから下位アドレスへ）:
    //
    //   stack_top - 8:  パディング（アライメント調整）
    //   stack_top - 16: task_trampoline のアドレス（context_switch の ret 先）
    //   stack_top - 24: rbp = 0
    //   stack_top - 32: rbx = 0
    //   stack_top - 40: rdi = 0
    //   stack_top - 48: rsi = 0
    //   stack_top - 56: r12 = entry 関数のアドレス ★
    //   stack_top - 64: r13 = 0
    //   stack_top - 72: r14 = 0
    //   stack_top - 80: r15 = 0  ← 初期 rsp
    //
    // r12 にエントリ関数のアドレスを入れておくことで、
    // task_trampoline が `call r12` でエントリ関数を呼び出せる。
    //
    // アライメントの計算:
    //   initial_rsp = stack_top - 80 (10 * 8 バイト)
    //   stack_top が 16n なら initial_rsp = 16n - 80 = 16(n-5) → 16 バイトアライン ✓
    //   context_switch 後に ret した時点で rsp = stack_top - 8 = 16n + 8 形式 ✓

    // task_trampoline のアドレスを取得
    unsafe extern "C" {
        fn task_trampoline();
    }
    let trampoline_addr = task_trampoline as *const () as u64;

    unsafe {
        let ptr = stack_top as *mut u64;
        *ptr.sub(1) = 0;                     // パディング
        *ptr.sub(2) = trampoline_addr;        // ret 先 → task_trampoline
        *ptr.sub(3) = 0;                     // rbp
        *ptr.sub(4) = 0;                     // rbx
        *ptr.sub(5) = 0;                     // rdi
        *ptr.sub(6) = 0;                     // rsi
        *ptr.sub(7) = entry as u64;           // r12 = エントリ関数 ★
        *ptr.sub(8) = 0;                     // r13
        *ptr.sub(9) = 0;                     // r14
        *ptr.sub(10) = 0;                    // r15
    }

    let initial_rsp = stack_top - 80;

    // カーネルタスクには親は設定しない（内部タスクなので）
    sched.tasks.push(Task {
        id,
        name: String::from(name),
        state: TaskState::Ready,
        context: Context { rsp: initial_rsp },
        _stack: Some(stack),
        cr3: None,                    // カーネルの CR3 を使用
        user_process_info: None,      // カーネルタスク
        is_user: false,               // カーネルタスク
        parent_id: None,              // カーネルタスクに親はいない
        exit_code: 0,                 // 初期値
        reaped: false,                // wait() は不要
    });

    crate::serial_println!("[scheduler] spawned task {} '{}'", id, name);
}

/// 現在のタスクの CPU を譲り、次の Ready タスクに切り替える。
///
/// 他に Ready タスクがなければ何もせず即座に戻る。
/// これが協調的マルチタスクの中核: タスクが自発的に yield しない限り切り替わらない。
///
/// 内部の流れ:
///   1. 割り込みを無効化（コンテキストスイッチ中の競合防止）
///   2. Mutex を取得して次のタスクを決定
///   3. Mutex を解放（context_switch 中にロックを保持しないため）
///   4. context_switch でスタックを切り替え
///   5. 戻ってきたら割り込みを再有効化
pub fn yield_now() {
    x86_64::instructions::interrupts::disable();

    let switch_info = {
        let mut sched = SCHEDULER.lock();
        let current = sched.current;
        let num_tasks = sched.tasks.len();

        // 次の Ready タスクをラウンドロビンで探す。
        // current+1 から始めて一周する。
        let mut next = None;
        for i in 1..=num_tasks {
            let idx = (current + i) % num_tasks;
            if sched.tasks[idx].state == TaskState::Ready {
                next = Some(idx);
                break;
            }
        }

        match next {
            None => None, // 他に Ready タスクがない
            Some(next_idx) => {
                // 現在のタスクが Running なら Ready に戻す
                // （Finished の場合はそのまま Finished）
                if sched.tasks[current].state == TaskState::Running {
                    sched.tasks[current].state = TaskState::Ready;
                }
                sched.tasks[next_idx].state = TaskState::Running;
                sched.current = next_idx;

                // context_switch に渡すポインタを取得。
                // Mutex を drop した後にこれらのポインタを使うが、
                // 割り込み無効 + シングルコアなので安全。
                let old_rsp_ptr =
                    &mut sched.tasks[current].context.rsp as *mut u64;
                let new_rsp = sched.tasks[next_idx].context.rsp;

                // 切り替え先タスクの CR3 を取得。
                // ユーザープロセスの場合はプロセス固有の CR3、
                // カーネルタスクの場合は kernel_cr3() を使う。
                let new_cr3 = sched.tasks[next_idx]
                    .cr3
                    .map(|f| f.start_address().as_u64())
                    .unwrap_or_else(|| crate::paging::kernel_cr3().as_u64());

                // 切り替え先タスクのカーネルスタックトップを取得（ユーザープロセスのみ）。
                // TSS rsp0 の更新に必要。
                let new_kernel_stack_top = sched.tasks[next_idx]
                    .user_process_info
                    .as_ref()
                    .map(|info| {
                        let ks_ptr = info.process.kernel_stack.as_ptr() as u64;
                        let ks_len = info.process.kernel_stack.len() as u64;
                        ks_ptr + ks_len
                    });

                Some((old_rsp_ptr, new_rsp, new_cr3, new_kernel_stack_top))
            }
        }
    }; // Mutex はここで drop される（context_switch 前にロックを解放）

    match switch_info {
        None => {
            // 切り替え先がない。
            // 全タスクが Sleeping の場合、割り込みを有効化して hlt で待機する。
            // enable_and_hlt() はアトミックに sti + hlt を実行するため、
            // 「割り込み有効化→hlt の間にタイマー割り込みを取りこぼす」レースを防ぐ。
            // タイマー割り込みが発火すると preempt() が Sleeping タスクの起床をチェックする。
            x86_64::instructions::interrupts::enable_and_hlt();
        }
        Some((old_rsp_ptr, new_rsp, new_cr3, new_kernel_stack_top)) => {
            // ユーザープロセスへの切り替え時は TSS rsp0 を更新する。
            // ユーザーモードで割り込み/システムコールが発生したとき、
            // CPU は TSS rsp0 のアドレスをカーネルスタックとして使用する。
            // 各プロセスは独自のカーネルスタックを持つので、切り替え時に更新が必要。
            if let Some(kernel_stack_top) = new_kernel_stack_top {
                unsafe {
                    crate::gdt::set_tss_rsp0(VirtAddr::new(kernel_stack_top));
                }
            }

            // コンテキストスイッチを実行。
            // CR3 も同時に切り替えることで、新しいタスクのアドレス空間になる。
            // この関数から「戻ってきた」時点で、このタスクは
            // 別のタスクの yield_now() から再スケジュールされている。
            unsafe {
                context_switch_enable(old_rsp_ptr, new_rsp, new_cr3);
            }
            // 戻ってきた = このタスクが再び Running になった
            x86_64::instructions::interrupts::enable();
        }
    }
}

/// タイマー割り込みハンドラから呼ばれるプリエンプション関数。
///
/// yield_now() との違い:
///   - try_lock() を使う（デッドロック防止）。
///     タイマー割り込みは SCHEDULER のロック保持中にも発生しうるので、
///     lock() で待つとデッドロックになる。try_lock() が失敗したら
///     今回のプリエンプションはスキップし、次のタイマー割り込みに任せる。
///   - 割り込みの有効/無効を操作しない。
///     この関数は割り込みハンドラの中（= 割り込み無効状態）で呼ばれるため、
///     自分で割り込みを操作する必要がない。
///     戻り先のタスクの iretq で割り込みが再有効化される。
///
/// 呼び出し元（タイマー割り込みハンドラ）は、この関数を呼ぶ前に
/// EOI (End Of Interrupt) を送っておくこと。
/// context_switch で別タスクに切り替わった場合、そのタスクがタイマー割り込みを
/// 受け取れるようにするため。
pub fn preempt() {
    PREEMPT_CALL_COUNT.fetch_add(1, Ordering::Relaxed);

    let switch_info = {
        // try_lock(): ロック取得できなければプリエンプションをスキップ。
        // SCHEDULER のロック保持中にタイマーが発火した場合のデッドロックを防ぐ。
        let mut sched = match SCHEDULER.try_lock() {
            Some(guard) => guard,
            None => return, // ロック取得失敗 → 今回はスキップ
        };

        // スリープ中のタスクの起床チェック。
        // 現在のタイマーティック数を取得して、起床時刻に達した Sleeping タスクを
        // Ready に戻す。これによりタイマーティックごとにスリープの解除判定が行われる。
        let now = crate::interrupts::TIMER_TICK_COUNT.load(Ordering::Relaxed);
        for task in sched.tasks.iter_mut() {
            if let TaskState::Sleeping(wake_at) = task.state {
                if now >= wake_at {
                    task.state = TaskState::Ready;
                }
            }
        }

        let current = sched.current;
        let num_tasks = sched.tasks.len();

        // タスクが 1 つ以下ならスイッチ不要
        if num_tasks <= 1 {
            return;
        }

        // 次の Ready タスクをラウンドロビンで探す
        let mut next = None;
        for i in 1..=num_tasks {
            let idx = (current + i) % num_tasks;
            if sched.tasks[idx].state == TaskState::Ready {
                next = Some(idx);
                break;
            }
        }

        match next {
            None => None, // 他に Ready タスクがない
            Some(next_idx) => {
                // 現在のタスクが Running なら Ready に戻す
                if sched.tasks[current].state == TaskState::Running {
                    sched.tasks[current].state = TaskState::Ready;
                }
                sched.tasks[next_idx].state = TaskState::Running;
                sched.current = next_idx;

                let old_rsp_ptr =
                    &mut sched.tasks[current].context.rsp as *mut u64;
                let new_rsp = sched.tasks[next_idx].context.rsp;

                // 切り替え先タスクの CR3 を取得
                let new_cr3 = sched.tasks[next_idx]
                    .cr3
                    .map(|f| f.start_address().as_u64())
                    .unwrap_or_else(|| crate::paging::kernel_cr3().as_u64());

                // 切り替え先タスクのカーネルスタックトップを取得（ユーザープロセスのみ）
                let new_kernel_stack_top = sched.tasks[next_idx]
                    .user_process_info
                    .as_ref()
                    .map(|info| {
                        let ks_ptr = info.process.kernel_stack.as_ptr() as u64;
                        let ks_len = info.process.kernel_stack.len() as u64;
                        ks_ptr + ks_len
                    });

                Some((old_rsp_ptr, new_rsp, new_cr3, new_kernel_stack_top))
            }
        }
    }; // Mutex はここで drop

    if let Some((old_rsp_ptr, new_rsp, new_cr3, new_kernel_stack_top)) = switch_info {
        PREEMPT_SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);

        // ユーザープロセスへの切り替え時は TSS rsp0 を更新。
        // ユーザーモードで割り込み/システムコールが発生したとき、
        // CPU は TSS rsp0 のアドレスをカーネルスタックとして使用する。
        // 各プロセスは独自のカーネルスタックを持つので、切り替え時に更新が必要。
        if let Some(kernel_stack_top) = new_kernel_stack_top {
            unsafe {
                crate::gdt::set_tss_rsp0(VirtAddr::new(kernel_stack_top));
            }
        }

        unsafe {
            context_switch(old_rsp_ptr, new_rsp, new_cr3);
        }
        // 戻ってきた = このタスクが再び Running になった
        // （割り込みハンドラ内なので iretq で割り込みが再有効化される）
    }
}

/// 現在のタスクを指定ティック数だけスリープさせる。
///
/// PIT は約 18.2 Hz で発火するので、1 ティック ≈ 55ms。
/// タスクを Sleeping 状態にして yield_now() で他のタスクに切り替える。
/// preempt() のタイマーティックごとの起床チェックで、
/// 指定ティック数が経過したら自動的に Ready に戻される。
pub fn sleep_ticks(ticks: u64) {
    let wake_at = crate::interrupts::TIMER_TICK_COUNT.load(Ordering::Relaxed) + ticks;

    {
        let mut sched = SCHEDULER.lock();
        let current = sched.current;
        sched.tasks[current].state = TaskState::Sleeping(wake_at);
    }

    // 他のタスクに切り替える。
    // このタスクが Ready に戻されるのは preempt() の起床チェックで wake_at に達したとき。
    yield_now();
}

/// 現在のタスクを指定ミリ秒だけスリープさせる。
///
/// PIT のデフォルト周波数は約 18.2 Hz（≈ 55ms 間隔）なので、
/// ミリ秒をティック数に変換してから sleep_ticks() を呼ぶ。
/// 精度は PIT の周波数に依存する（最大 55ms の誤差がある）。
pub fn sleep_ms(ms: u64) {
    // PIT のデフォルト周波数: 1193182 Hz / 65536 ≈ 18.2065 Hz
    // 1 ティック ≈ 54.925 ms
    // ticks = ms / 54.925 ≈ ms * 182 / 10000
    // 最低でも 1 ティックはスリープする（0 だと即座に起きてしまう）
    let ticks = (ms * 182 / 10000).max(1);
    sleep_ticks(ticks);
}

/// Ready または Sleeping 状態のタスクがあるかどうかを返す。
/// Sleeping タスクはいずれ Ready に戻るので、まだ終わっていないタスクがある扱い。
pub fn has_ready_tasks() -> bool {
    let sched = SCHEDULER.lock();
    sched.tasks.iter().any(|t| {
        matches!(t.state, TaskState::Ready | TaskState::Sleeping(_))
    })
}

/// 全タスクの情報を取得する（ps コマンド用）。
pub fn task_list() -> Vec<TaskInfo> {
    let sched = SCHEDULER.lock();
    sched
        .tasks
        .iter()
        .map(|t| TaskInfo {
            id: t.id,
            name: t.name.clone(),
            state: t.state,
            is_user_process: t.is_user,
        })
        .collect()
}

/// プロセスごとのメモリ使用量を取得する（procfs 用）。
///
/// ユーザープロセスは `allocated_frames` の数を返す。
/// カーネルタスクや終了済みプロセスは 0 とする。
pub fn process_mem_list() -> Vec<ProcessMemInfo> {
    let sched = SCHEDULER.lock();
    sched
        .tasks
        .iter()
        .map(|t| {
            let user_frames = t.user_process_info
                .as_ref()
                .map(|info| info.process.allocated_frames.len())
                .unwrap_or(0);
            ProcessMemInfo {
                id: t.id,
                name: t.name.clone(),
                is_user_process: t.is_user,
                user_frames,
            }
        })
        .collect()
}

/// 現在実行中のタスクIDを取得する
pub fn current_task_id() -> u64 {
    let sched = SCHEDULER.lock();
    sched.tasks[sched.current].id
}

/// 現在のタスクを参照して処理する（デバッグ用）
pub fn with_current_task<F: FnOnce(&Task)>(f: F) {
    let sched = SCHEDULER.lock();
    let task = &sched.tasks[sched.current];
    f(task);
}

/// 指定したタスクIDが存在するか確認する
pub fn task_exists(task_id: u64) -> bool {
    let sched = SCHEDULER.lock();
    sched.tasks.iter().any(|t| t.id == task_id && t.state != TaskState::Finished)
}

/// タスク名からタスク ID を探す。
///
/// ユーザープロセスは ELF のファイル名（例: "NETD.ELF"）がタスク名になる。
pub fn find_task_id_by_name(name: &str) -> Option<u64> {
    let sched = SCHEDULER.lock();
    sched
        .tasks
        .iter()
        .find(|t| t.name == name && t.state != TaskState::Finished)
        .map(|t| t.id)
}

/// wait_for_child() のエラー型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
    /// 子プロセスがいない
    NoChild,
    /// 指定したタスクは子プロセスではない
    NotChild,
    /// タイムアウト
    Timeout,
}

/// 子プロセスの終了を待つ
///
/// # 引数
/// - `target_task_id`: 待つ子プロセスのタスク ID (0 なら任意の子)
/// - `timeout_ms`: タイムアウト (ms)。0 なら無期限待ち
///
/// # 戻り値
/// - Ok(exit_code): 子プロセスの終了コード
/// - Err(WaitError): エラー
///
/// # 動作
/// - 現在のタスクの子プロセス（parent_id が自分のタスク）を探す
/// - target_task_id > 0 の場合、そのタスクが子かどうかを確認
/// - 子プロセスが Finished 状態になるまでポーリングする
/// - Finished になったら exit_code を取得して返す
pub fn wait_for_child(target_task_id: u64, timeout_ms: u64) -> Result<i32, WaitError> {
    let my_id = current_task_id();
    let start_tick = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);

    loop {
        {
            let mut sched = SCHEDULER.lock();

            // 子プロセスの中で Finished かつ未回収のものを探す
            let finished_child_idx = sched.tasks.iter().position(|t| {
                // 自分の子かどうか
                let is_my_child = t.parent_id == Some(my_id);
                // Finished 状態かどうか
                let is_finished = t.state == TaskState::Finished;
                // まだ wait() で回収されていないか
                let is_not_reaped = !t.reaped;
                // target_task_id が指定されていれば、そのタスクのみ対象
                let is_target = target_task_id == 0 || t.id == target_task_id;

                is_my_child && is_finished && is_not_reaped && is_target
            });

            if let Some(idx) = finished_child_idx {
                // 子プロセスが終了している
                let exit_code = sched.tasks[idx].exit_code;
                // 同じ終了を繰り返し返さないように回収済みにする
                sched.tasks[idx].reaped = true;
                // TODO: 将来的にはここでタスクエントリをクリーンアップする
                return Ok(exit_code);
            }

            // target_task_id が指定されている場合、そのタスクが自分の子かどうか確認
            if target_task_id > 0 {
                let target = sched.tasks.iter().find(|t| t.id == target_task_id);
                match target {
                    None => return Err(WaitError::NoChild), // タスクが存在しない
                    Some(t) if t.parent_id != Some(my_id) => return Err(WaitError::NotChild), // 子ではない
                    Some(t) if t.state == TaskState::Finished && t.reaped => {
                        return Err(WaitError::NoChild); // 既に wait() 済み
                    }
                    Some(_) => {} // 子だが、まだ終了していない
                }
            } else {
                // target_task_id == 0 の場合、未回収の子プロセスが一つもいなければエラー
                let has_child = sched.tasks.iter().any(|t| {
                    let is_my_child = t.parent_id == Some(my_id);
                    let is_unreaped = !(t.state == TaskState::Finished && t.reaped);
                    is_my_child && is_unreaped
                });
                if !has_child {
                    return Err(WaitError::NoChild);
                }
            }
        }

        // タイムアウトチェック
        if timeout_ms > 0 {
            let now = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
            // PIT は約 18.2 Hz なので 1 tick ≈ 55ms
            let elapsed_ticks = now.saturating_sub(start_tick);
            let elapsed_ms = elapsed_ticks * 55; // 近似値
            if elapsed_ms >= timeout_ms {
                return Err(WaitError::Timeout);
            }
        }

        // まだ終了していないので、yield して待つ
        yield_now();
    }
}

/// プロセスの終了コードを設定する（SYS_EXIT から呼ばれる）
/// 将来的に exit(code) システムコールで使用される
#[allow(dead_code)]
pub fn set_exit_code(exit_code: i32) {
    let mut sched = SCHEDULER.lock();
    let current = sched.current;
    sched.tasks[current].exit_code = exit_code;
}

// =================================================================
// ユーザープロセスのマルチタスク対応
// =================================================================
//
// ユーザープロセスをカーネルタスクとしてスケジューラに登録する。
// 各ユーザープロセスは専用のページテーブル (CR3) を持ち、
// コンテキストスイッチ時に CR3 も切り替えることでアドレス空間を分離する。
//
// ユーザープロセスの実行フロー:
//   1. spawn_user() でタスクを作成、user_task_trampoline をエントリに設定
//   2. スケジューラがタスクを選択 → context_switch で CR3 も切り替え
//   3. user_task_trampoline が TSS rsp0 を設定し、iretq で Ring 3 に遷移
//   4. ユーザーコードが実行される
//   5. タイマー割り込みでプリエンプション → 別タスクに切り替え
//   6. タスクに戻ると context_switch の ret → user_task_trampoline 戻り → 再び iretq
//      （または、割り込みフレームが残っている場合は iretq で直接ユーザーコードに戻る）
//   7. SYS_EXIT でプロセス終了 → user_task_exit_handler → タスクを Finished に

// =================================================================
// ユーザータスクトランポリン（アセンブリ）
// =================================================================
//
// ユーザープロセス用のタスクが初めてスケジュールされたとき、
// context_switch の ret がここにジャンプする。
//
// レジスタの初期値:
//   r12 = user_task_entry_wrapper 関数のアドレス
//   r13 = タスク ID（タスク情報を検索するため）
//
// 処理の流れ:
//   1. sti で割り込みを有効化
//   2. スタックを整えてシャドウスペースを確保（Microsoft x64 ABI 要件）
//   3. user_task_entry_wrapper(task_id) を呼び出す
//   4. エントリ関数が return したら user_task_exit_handler を呼んでタスクを終了
//
// user_task_entry_wrapper は Rust で書かれ、タスク情報から UserProcessInfo を取り出して
// Ring 3 への遷移を行う。
global_asm!(
    "user_task_trampoline:",
    "sti",            // 割り込みを有効化（プリエンプションに必要）
    "sub rsp, 40",    // シャドウスペース + アライメント
    "mov rcx, r13",   // 第1引数 = task_id
    "call r12",       // user_task_entry_wrapper(task_id)
    "add rsp, 40",
    "sub rsp, 40",
    "call {exit}",
    "ud2",
    exit = sym user_task_exit_handler,
);

/// ユーザータスクのエントリ関数が return した後に呼ばれるハンドラ。
/// SYS_EXIT でプロセスが終了した場合もここに来る。
/// 現在のタスクを Finished に設定して、他のタスクに切り替える。
#[unsafe(no_mangle)]
extern "C" fn user_task_exit_handler() {
    let user_process_info = {
        let mut sched = SCHEDULER.lock();
        let current = sched.current;
        sched.tasks[current].state = TaskState::Finished;
        // ユーザープロセス情報を取り出す（プロセス破棄のため）
        sched.tasks[current].user_process_info.take()
    };

    // ユーザープロセスのリソースを解放
    if let Some(info) = user_process_info {
        crate::usermode::destroy_user_process(info.process);
    }

    // 他のタスクに切り替える
    yield_now();

    // ここに戻ることはないはず（Finished タスクはスケジュールされない）
    loop {
        x86_64::instructions::hlt();
    }
}

/// ユーザータスクのエントリラッパー。
/// user_task_trampoline から呼ばれ、タスク情報を取り出して Ring 3 に遷移する。
///
/// この関数は Ring 0 で実行され、iretq で Ring 3 に遷移する。
/// SYS_EXIT システムコールが呼ばれると exit_usermode() 経由でここに戻る。
#[unsafe(no_mangle)]
extern "C" fn user_task_entry_wrapper(task_id: u64) {
    // タスク情報を取得
    let (entry_point, user_stack_top, kernel_stack_ptr, kernel_stack_len) = {
        let mut sched = SCHEDULER.lock();
        let task = sched.tasks.iter_mut().find(|t| t.id == task_id);
        match task {
            Some(t) => {
                if let Some(ref mut info) = t.user_process_info {
                    info.first_run_done = true;
                    let ks_ptr = info.process.kernel_stack.as_ptr() as u64;
                    let ks_len = info.process.kernel_stack.len() as u64;
                    (info.entry_point, info.user_stack_top, ks_ptr, ks_len)
                } else {
                    // ユーザープロセス情報がない → エラー
                    crate::serial_println!("[scheduler] ERROR: task {} has no user_process_info", task_id);
                    return;
                }
            }
            None => {
                crate::serial_println!("[scheduler] ERROR: task {} not found", task_id);
                return;
            }
        }
    };

    // カーネルスタックのトップアドレスを計算
    let kernel_stack_top = kernel_stack_ptr + kernel_stack_len;

    // TSS rsp0 にカーネルスタックのトップを設定する。
    // Ring 3 で int 0x80 が発生したとき、CPU は TSS rsp0 のアドレスに
    // スタックを切り替える。
    unsafe {
        crate::gdt::set_tss_rsp0(VirtAddr::new(kernel_stack_top));
    }

    // セグメントセレクタを取得
    let user_cs = crate::gdt::user_code_selector().0 as u64;

    // RFLAGS: IF (Interrupt Flag, bit 9) を立てておく
    let rflags: u64 = 0x200;

    // Ring 3 に遷移する。
    // jump_to_usermode() は usermode.rs で定義されているアセンブリ関数。
    // SYS_EXIT → exit_usermode() で RSP/RBP が復元され、ここに戻る。
    unsafe {
        crate::usermode::jump_to_usermode(entry_point, user_cs, rflags, user_stack_top);
    }

    // ここに到達 = exit_usermode() 経由で Ring 3 から戻ってきた（SYS_EXIT）
    // この後、user_task_trampoline に戻り、user_task_exit_handler が呼ばれる
}

/// ユーザープロセスを新しいタスクとして作成し、スケジューラに登録する。
///
/// ELF バイナリをロードしてユーザープロセスを作成し、バックグラウンドで実行する。
/// タスクはスケジューラに Ready 状態で登録され、次の yield_now() または preempt() で
/// スケジュールされる。
///
/// # 引数
/// - `name`: タスク名（ps コマンドで表示される）
/// - `elf_data`: ELF バイナリのデータ
///
/// # 戻り値
/// 成功時は Ok(タスクID)、失敗時は Err(エラーメッセージ)
pub fn spawn_user(name: &str, elf_data: &[u8]) -> Result<u64, &'static str> {
    // ELF からユーザープロセスを作成
    let (process, entry_point, user_stack_top) = crate::usermode::create_elf_process(elf_data)?;

    // プロセスの CR3（ページテーブル）を取得
    let cr3 = process.page_table_frame;

    let mut sched = SCHEDULER.lock();

    // 親タスクの ID を取得（呼び出し元のタスク）
    // カーネルタスク (task 0) から呼ばれた場合や初回起動時は parent_id を None にする。
    // ユーザープロセスから spawn システムコール経由で呼ばれた場合は親タスクの ID を設定する。
    let parent_id = if sched.tasks.is_empty() {
        None // 最初のタスク（init）には親がない
    } else {
        Some(sched.tasks[sched.current].id)
    };

    let id = sched.next_id;
    sched.next_id += 1;

    // --- タスク用スタックの確保（カーネルモードでの実行用） ---
    let stack = vec![0u8; TASK_STACK_SIZE].into_boxed_slice();
    let stack_bottom = stack.as_ptr() as u64;
    let stack_top = stack_bottom + TASK_STACK_SIZE as u64;
    let stack_top = stack_top & !0xF; // 16 バイトアライメント

    // --- 初期スタックの設定（user_task_trampoline 用） ---
    //
    // スタックレイアウト:
    //   stack_top - 8:  パディング
    //   stack_top - 16: user_task_trampoline のアドレス（context_switch の ret 先）
    //   stack_top - 24: rbp = 0
    //   stack_top - 32: rbx = 0
    //   stack_top - 40: rdi = 0
    //   stack_top - 48: rsi = 0
    //   stack_top - 56: r12 = user_task_entry_wrapper のアドレス
    //   stack_top - 64: r13 = task_id
    //   stack_top - 72: r14 = 0
    //   stack_top - 80: r15 = 0  ← 初期 rsp

    unsafe extern "C" {
        fn user_task_trampoline();
    }
    let trampoline_addr = user_task_trampoline as *const () as u64;
    let entry_wrapper_addr = user_task_entry_wrapper as *const () as u64;

    unsafe {
        let ptr = stack_top as *mut u64;
        *ptr.sub(1) = 0;                      // パディング
        *ptr.sub(2) = trampoline_addr;         // ret 先 → user_task_trampoline
        *ptr.sub(3) = 0;                      // rbp
        *ptr.sub(4) = 0;                      // rbx
        *ptr.sub(5) = 0;                      // rdi
        *ptr.sub(6) = 0;                      // rsi
        *ptr.sub(7) = entry_wrapper_addr;      // r12 = エントリラッパー
        *ptr.sub(8) = id;                     // r13 = task_id
        *ptr.sub(9) = 0;                      // r14
        *ptr.sub(10) = 0;                     // r15
    }

    let initial_rsp = stack_top - 80;

    // ユーザープロセス情報を作成
    let user_process_info = UserProcessInfo {
        process,
        entry_point,
        user_stack_top,
        first_run_done: false,
    };

    sched.tasks.push(Task {
        id,
        name: String::from(name),
        state: TaskState::Ready,
        context: Context { rsp: initial_rsp },
        _stack: Some(stack),
        cr3: Some(cr3),
        user_process_info: Some(user_process_info),
        is_user: true,                // ユーザープロセス
        parent_id,                    // 親タスクの ID（spawn 元）
        exit_code: 0,                 // 初期値
        reaped: false,                // wait() が呼ばれるまで未回収
    });

    crate::serial_println!("[scheduler] spawned user task {} '{}' (entry: {:#x}, parent: {:?})", id, name, entry_point, parent_id);

    Ok(id)
}
