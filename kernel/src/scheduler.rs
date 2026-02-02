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
// context_switch(old_rsp_ptr: *mut u64, new_rsp: u64)
//   rcx = old_rsp_ptr: 現在のタスクの rsp を保存する場所へのポインタ
//   rdx = new_rsp: 切り替え先タスクの rsp
//
// Microsoft x64 ABI では第1引数が rcx、第2引数が rdx に入る。
//
// 処理の流れ:
//   1. 現在の callee-saved レジスタをスタックに push（8個 = 64バイト）
//   2. 現在の rsp を [rcx] に保存
//   3. rsp を rdx の値に切り替え
//   4. 新しいスタックから callee-saved レジスタを pop
//   5. ret で新しいタスクの実行を再開
//
// 新しいタスクの場合、ret は task_trampoline にジャンプする。
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
    "mov [rcx], rsp",
    "mov rsp, rdx",
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
    fn context_switch(old_rsp_ptr: *mut u64, new_rsp: u64);
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
}

/// カーネルタスク。
///
/// 各タスクは独自のスタックとコンテキスト（rsp）を持つ。
/// コンテキストスイッチではスタックポインタを切り替えることで、
/// タスクの実行状態を丸ごと切り替える。
pub struct Task {
    /// タスク ID（一意）
    pub id: u64,
    /// タスク名（デバッグ・表示用）
    pub name: &'static str,
    /// タスクの状態
    pub state: TaskState,
    /// コンテキスト（スタックポインタ）
    pub context: Context,
    /// タスク用のスタック領域。None はブートスタック（task 0）を使う。
    /// Box<[u8]> で安定したアドレスを保証する。
    _stack: Option<alloc::boxed::Box<[u8]>>,
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
        name: "kernel",
        state: TaskState::Running,
        context: Context { rsp: 0 }, // 最初の yield_now() で実際の rsp が保存される
        _stack: None,                 // ブートスタックを使用
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

    sched.tasks.push(Task {
        id,
        name,
        state: TaskState::Ready,
        context: Context { rsp: initial_rsp },
        _stack: Some(stack),
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

                Some((old_rsp_ptr, new_rsp))
            }
        }
    }; // Mutex はここで drop される（context_switch 前にロックを解放）

    match switch_info {
        None => {
            // 切り替え先がない → そのまま戻る
            x86_64::instructions::interrupts::enable();
        }
        Some((old_rsp_ptr, new_rsp)) => {
            // コンテキストスイッチを実行。
            // この関数から「戻ってきた」時点で、このタスクは
            // 別のタスクの yield_now() から再スケジュールされている。
            unsafe {
                context_switch(old_rsp_ptr, new_rsp);
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

                Some((old_rsp_ptr, new_rsp))
            }
        }
    }; // Mutex はここで drop

    if let Some((old_rsp_ptr, new_rsp)) = switch_info {
        PREEMPT_SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe {
            context_switch(old_rsp_ptr, new_rsp);
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
            name: String::from(t.name),
            state: t.state,
        })
        .collect()
}
