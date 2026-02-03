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
// 4. ユーザー空間が SYS_READ システムコールで読み取り
//    または、カーネルシェルが console::read_input() で読み取り
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
use spin::Mutex;

/// 入力バッファの最大サイズ
const INPUT_BUFFER_SIZE: usize = 256;

/// コンソール入力バッファ
///
/// キーボード割り込みハンドラが push_input_char() で追加し、
/// システムコールハンドラや カーネルシェルが read_input() で読み取る。
static INPUT_BUFFER: Mutex<VecDeque<char>> = Mutex::new(VecDeque::new());

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

/// 入力バッファから1文字を読み取る（ブロッキング）
///
/// バッファが空の場合は文字が入るまで待機する。
/// スケジューラの yield を使って他のタスクに CPU を譲る。
///
/// # 注意
/// この関数はカーネルのシェルや、システムコールハンドラから呼ばれる。
/// 割り込みが有効な状態で呼ぶこと（でないと永久にブロックする）。
pub fn read_input_blocking() -> char {
    loop {
        if let Some(c) = read_input_nonblocking() {
            return c;
        }
        // CPU を他のタスクに譲る
        // 割り込みが有効なら、キーボード割り込みで文字が追加される
        crate::scheduler::yield_now();
    }
}

/// 入力バッファから指定バイト数を読み取る（ブロッキング）
///
/// 少なくとも1バイト読み取れるまでブロックし、
/// その後は利用可能な分だけ（最大 max_len バイト）を読み取って返す。
///
/// # 引数
/// - `buf`: 読み取ったデータを格納するバッファ
/// - `max_len`: 最大読み取りバイト数
///
/// # 戻り値
/// 実際に読み取ったバイト数
///
/// # 注意
/// 現在は簡易実装として、文字を1バイトとして扱う（ASCII 前提）。
/// 将来的には UTF-8 対応が必要。
pub fn read_input(buf: &mut [u8], max_len: usize) -> usize {
    let mut count = 0;
    let limit = core::cmp::min(buf.len(), max_len);

    // 少なくとも1文字は読み取る（ブロッキング）
    if limit > 0 {
        let c = read_input_blocking();
        // 簡易実装: ASCII として扱う
        // 非 ASCII 文字は ? に置換
        buf[count] = if c.is_ascii() { c as u8 } else { b'?' };
        count += 1;
    }

    // 残りはノンブロッキングで読み取る
    while count < limit {
        if let Some(c) = read_input_nonblocking() {
            buf[count] = if c.is_ascii() { c as u8 } else { b'?' };
            count += 1;
        } else {
            break;
        }
    }

    count
}

/// 入力バッファに文字があるかどうかを確認（ポーリング用）
#[allow(dead_code)]
pub fn has_input() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| {
        !INPUT_BUFFER.lock().is_empty()
    })
}
