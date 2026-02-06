// console.rs — コンソール入出力管理
//
// ユーザー空間とカーネル空間の両方からコンソール I/O を行うための
// 統一インターフェース。
//
// ## 入力の流れ
//
// 1. キーボード割り込みが発火
// 2. interrupts::keyboard_interrupt_handler() がスキャンコードを文字に変換
// 3. console::push_input_char() で入力バッファに追加
// 4. ユーザー空間が SYS_READ / SYS_KEY_READ システムコールで読み取り
//    （キーボードフォーカス機構により、特定タスクに入力を限定可能）
//
// ## 出力の流れ
//
// - ユーザー空間: SYS_WRITE → sys_write() → kprint!
// - カーネル: kprint! / kprintln! マクロ
//
// ## 設計原則
//
// - 入力は行バッファリング（改行まで溜める）
// - エコーバックは呼び出し側で行う（フレキシビリティのため）
// - ブロッキング読み取りはスケジューラの yield を使う

use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

/// 入力バッファの最大サイズ
const INPUT_BUFFER_SIZE: usize = 256;

/// コンソール入力バッファ
///
/// キーボード割り込みハンドラが push_input_char() で追加し、
/// システムコールハンドラや カーネルシェルが read_input() で読み取る。
static INPUT_BUFFER: Mutex<VecDeque<char>> = Mutex::new(VecDeque::new());

/// キーボードフォーカスを持つタスク ID（0 = フォーカスなし）
///
/// 特定のタスク（例: GUI サービス）がキーボードを独占したい場合に使う。
/// フォーカスが設定されている間は、そのタスクだけがキーボード入力を読み取れる。
/// フォーカス外のタスクが read_input_for_task() を呼ぶとブロッキングで待機し続け、
/// read_input_nonblocking_for_task() を呼ぶと None が返る。
static KEYBOARD_FOCUS_TASK: AtomicU64 = AtomicU64::new(0);

/// キーボード割り込みハンドラから呼ばれる: 1文字を入力バッファに追加
///
/// 割り込みコンテキストから呼ばれるため、ロック取得は短時間で完了すること。
/// バッファがいっぱいの場合は古い文字を捨てる。
pub fn push_input_char(c: char) {
    let mut buffer = INPUT_BUFFER.lock();
    if buffer.len() >= INPUT_BUFFER_SIZE {
        // バッファがいっぱいなら最も古い文字を捨てる
        buffer.pop_front();
    }
    buffer.push_back(c);
}

/// 入力バッファから1文字を読み取る（ノンブロッキング）
///
/// バッファが空の場合は None を返す。
/// 割り込みを無効化してロックを取得し、デッドロックを防ぐ。
pub fn read_input_nonblocking() -> Option<char> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        INPUT_BUFFER.lock().pop_front()
    })
}

/// 入力バッファに文字があるかどうかを確認（ポーリング用）
#[allow(dead_code)]
pub fn has_input() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| {
        !INPUT_BUFFER.lock().is_empty()
    })
}

// =================================================================
// キーボードフォーカス機構
// =================================================================
//
// GUI サービスなど、特定のタスクがキーボード入力を独占したい場合に使う。
// フォーカスを取得したタスクだけが入力バッファからキーを読み取れる。
// フォーカス外のタスク（例: シェル）は SYS_READ でブロックされたまま待機する。
// フォーカスを解放すれば、シェルなど元のタスクが再びキー入力を受け取れるようになる。

/// キーボードフォーカスを取得する
///
/// 指定したタスクがキーボード入力を独占する。
/// 他のタスクの read_input_for_task() はフォーカスが解放されるまで
/// 入力を受け取れなくなる。
pub fn grab_keyboard(task_id: u64) {
    KEYBOARD_FOCUS_TASK.store(task_id, Ordering::SeqCst);
    crate::serial_println!("[console] keyboard focus grabbed by task {}", task_id);
}

/// キーボードフォーカスを解放する
///
/// 指定したタスクがフォーカスを持っている場合のみ解放する。
/// フォーカスを持っていないタスクが呼んでも何もしない。
pub fn release_keyboard(task_id: u64) {
    // 現在のフォーカスが指定タスクの場合のみ解放（compare_exchange）
    let _ = KEYBOARD_FOCUS_TASK.compare_exchange(
        task_id, 0, Ordering::SeqCst, Ordering::SeqCst
    );
    crate::serial_println!("[console] keyboard focus released by task {}", task_id);
}

/// フォーカス対応のノンブロッキング入力読み取り
///
/// - フォーカスなし (focus == 0): 誰でも読み取れる
/// - フォーカスあり: フォーカスを持つタスクだけが読み取れる
/// - フォーカス外のタスクは None を返す
pub fn read_input_nonblocking_for_task(caller_task_id: u64) -> Option<char> {
    let focus = KEYBOARD_FOCUS_TASK.load(Ordering::SeqCst);
    if focus != 0 && focus != caller_task_id {
        // フォーカス外のタスクには入力を渡さない
        return None;
    }
    read_input_nonblocking()
}

/// フォーカス対応のブロッキング入力読み取り
///
/// 既存の read_input() と同じインターフェースだが、
/// フォーカス外のタスクはフォーカスが解放されるまで yield で待機する。
///
/// # 引数
/// - `buf`: 読み取ったデータを格納するバッファ
/// - `max_len`: 最大読み取りバイト数
/// - `caller_task_id`: 呼び出し元のタスク ID
///
/// # 戻り値
/// 実際に読み取ったバイト数
pub fn read_input_for_task(buf: &mut [u8], max_len: usize, caller_task_id: u64) -> usize {
    let mut count = 0;
    let limit = core::cmp::min(buf.len(), max_len);

    // 少なくとも1文字は読み取る（ブロッキング）
    if limit > 0 {
        loop {
            // フォーカスチェック: フォーカス外なら yield して待つ
            let focus = KEYBOARD_FOCUS_TASK.load(Ordering::SeqCst);
            if focus != 0 && focus != caller_task_id {
                crate::scheduler::yield_now();
                continue;
            }
            if let Some(c) = read_input_nonblocking() {
                buf[count] = if c.is_ascii() { c as u8 } else { b'?' };
                count += 1;
                break;
            }
            crate::scheduler::yield_now();
        }
    }

    // 残りはノンブロッキングで読み取る
    while count < limit {
        if let Some(c) = read_input_nonblocking_for_task(caller_task_id) {
            buf[count] = if c.is_ascii() { c as u8 } else { b'?' };
            count += 1;
        } else {
            break;
        }
    }

    count
}
