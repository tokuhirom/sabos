// pipe.rs — カーネルパイプバッファ管理
//
// プロセス間でデータを受け渡すためのパイプ機構。
// 書き込み端（PipeWrite）と読み取り端（PipeRead）の 2 つのハンドルで構成される。
//
// ## 設計
//
// - VecDeque<u8> でリングバッファ風のデータバッファを保持
// - writer_closed / reader_closed フラグで端の閉鎖を管理
// - 両端が閉じられたらエントリを解放
//
// ## 読み取りの挙動
//
// - データがあれば即座に返す
// - データがなく writer が生きていれば WouldBlock を返す（呼び出し側が yield + retry）
// - データがなく writer_closed なら 0 を返す（EOF）
//
// ## 書き込みの挙動
//
// - reader が生きていれば書き込み成功
// - reader_closed なら BrokenPipe エラー

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use spin::Mutex;
use lazy_static::lazy_static;

/// パイプ操作のエラー型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeError {
    /// パイプ ID が無効
    InvalidPipe,
    /// 読み取り端が閉じられている（書き込み時に発生）
    BrokenPipe,
    /// データがまだない（reader は yield して再試行すべき）
    WouldBlock,
}

/// パイプバッファ（読み書きの共有リソース）
struct PipeBuffer {
    /// データバッファ（FIFO キュー）
    buf: VecDeque<u8>,
    /// 書き込み端の参照カウント（0 になったら writer_closed と同義）
    /// ハンドル複製（duplicate_handle）で同じパイプの write 端が複数存在しうる。
    /// 例: tsh の run コマンドでは親と子が同じパイプの write 端を持つ。
    writer_count: usize,
    /// 読み取り端が閉じられたか
    reader_closed: bool,
}

lazy_static! {
    /// グローバルパイプテーブル
    ///
    /// 各エントリは Option<PipeBuffer> で、None は空きスロット。
    /// pipe_id はこの Vec のインデックス。
    static ref PIPE_TABLE: Mutex<Vec<Option<PipeBuffer>>> = Mutex::new(Vec::new());
}

/// 新しいパイプを作成し、pipe_id を返す
///
/// 空きスロットがあれば再利用し、なければ末尾に追加する。
pub fn create() -> usize {
    let mut table = PIPE_TABLE.lock();

    let pipe = PipeBuffer {
        buf: VecDeque::new(),
        writer_count: 1,
        reader_closed: false,
    };

    // 空きスロットを探して再利用
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(pipe);
            return i;
        }
    }

    // 末尾に追加
    let id = table.len();
    table.push(Some(pipe));
    id
}

/// パイプにデータを書き込む
///
/// # 引数
/// - `pipe_id`: パイプの ID
/// - `data`: 書き込むデータ
///
/// # 戻り値
/// 書き込んだバイト数
///
/// # エラー
/// - `InvalidPipe`: パイプ ID が無効
/// - `BrokenPipe`: 読み取り端が閉じられている
pub fn write(pipe_id: usize, data: &[u8]) -> Result<usize, PipeError> {
    let mut table = PIPE_TABLE.lock();
    let pipe = table.get_mut(pipe_id)
        .and_then(|slot| slot.as_mut())
        .ok_or(PipeError::InvalidPipe)?;

    // 読み取り端が閉じられていれば BrokenPipe
    if pipe.reader_closed {
        return Err(PipeError::BrokenPipe);
    }

    // データをバッファに追加
    pipe.buf.extend(data.iter());
    Ok(data.len())
}

/// パイプからデータを読み取る
///
/// # 引数
/// - `pipe_id`: パイプの ID
/// - `buf`: 読み取り先バッファ
///
/// # 戻り値
/// 読み取ったバイト数。writer_closed かつデータなしの場合は 0（EOF）。
///
/// # エラー
/// - `InvalidPipe`: パイプ ID が無効
/// - `WouldBlock`: データがなく、writer がまだ生きている（yield して再試行すべき）
pub fn read(pipe_id: usize, buf: &mut [u8]) -> Result<usize, PipeError> {
    let mut table = PIPE_TABLE.lock();
    let pipe = table.get_mut(pipe_id)
        .and_then(|slot| slot.as_mut())
        .ok_or(PipeError::InvalidPipe)?;

    if pipe.buf.is_empty() {
        if pipe.writer_count == 0 {
            // 全 writer が閉じていてデータもない → EOF
            return Ok(0);
        } else {
            // writer はまだ生きているがデータがない → WouldBlock
            return Err(PipeError::WouldBlock);
        }
    }

    // バッファからデータを読み取る
    let copy_len = core::cmp::min(buf.len(), pipe.buf.len());
    for i in 0..copy_len {
        // drain の代わりに pop_front を使う（VecDeque の FIFO 操作）
        buf[i] = pipe.buf.pop_front().unwrap();
    }
    Ok(copy_len)
}

/// 書き込み端を閉じる（参照カウントをデクリメント）
///
/// writer_count が 0 になり、かつ reader_closed なら エントリを解放する。
pub fn close_writer(pipe_id: usize) {
    let mut table = PIPE_TABLE.lock();
    if let Some(Some(pipe)) = table.get_mut(pipe_id) {
        pipe.writer_count = pipe.writer_count.saturating_sub(1);
        // 両端が閉じられたらエントリを解放
        if pipe.writer_count == 0 && pipe.reader_closed {
            table[pipe_id] = None;
        }
    }
}

/// 書き込み端の参照カウントをインクリメントする
///
/// ハンドル複製（duplicate_handle）で同じパイプの PipeWrite ハンドルを
/// 追加作成する際に呼ぶ。
pub fn add_writer(pipe_id: usize) {
    let mut table = PIPE_TABLE.lock();
    if let Some(Some(pipe)) = table.get_mut(pipe_id) {
        pipe.writer_count += 1;
    }
}

/// 読み取り端を閉じる
///
/// reader_closed = true にする。
/// 両端が閉じられていればエントリを解放する。
pub fn close_reader(pipe_id: usize) {
    let mut table = PIPE_TABLE.lock();
    if let Some(Some(pipe)) = table.get_mut(pipe_id) {
        pipe.reader_closed = true;
        // 両端が閉じられたらエントリを解放
        if pipe.writer_count == 0 {
            table[pipe_id] = None;
        }
    }
}

// =================================================================
// テスト用 API
// =================================================================

/// パイプのテスト: 書き込み → 読み取り → EOF 確認
///
/// selftest から呼ばれる。
pub fn test_pipe() -> bool {
    // 1. パイプ作成
    let pipe_id = create();

    // 2. 書き込み
    let data = b"Hello, Pipe!";
    match write(pipe_id, data) {
        Ok(n) => {
            if n != data.len() {
                crate::serial_println!("[pipe test] write returned wrong length: {} vs {}", n, data.len());
                return false;
            }
        }
        Err(e) => {
            crate::serial_println!("[pipe test] write failed: {:?}", e);
            return false;
        }
    }

    // 3. 読み取り
    let mut buf = [0u8; 64];
    match read(pipe_id, &mut buf) {
        Ok(n) => {
            if n != data.len() {
                crate::serial_println!("[pipe test] read returned wrong length: {} vs {}", n, data.len());
                return false;
            }
            if &buf[..n] != data {
                crate::serial_println!("[pipe test] data mismatch");
                return false;
            }
        }
        Err(e) => {
            crate::serial_println!("[pipe test] read failed: {:?}", e);
            return false;
        }
    }

    // 4. writer が生きている状態で空読み → WouldBlock
    match read(pipe_id, &mut buf) {
        Err(PipeError::WouldBlock) => { /* 期待通り */ }
        other => {
            crate::serial_println!("[pipe test] expected WouldBlock, got {:?}", other);
            return false;
        }
    }

    // 5. writer を閉じる
    close_writer(pipe_id);

    // 6. writer closed + データなし → EOF (0)
    match read(pipe_id, &mut buf) {
        Ok(0) => { /* 期待通り: EOF */ }
        other => {
            crate::serial_println!("[pipe test] expected EOF (Ok(0)), got {:?}", other);
            return false;
        }
    }

    // 7. reader を閉じる（エントリ解放）
    close_reader(pipe_id);

    // 8. BrokenPipe テスト: 新しいパイプで reader を先に閉じてから書き込み
    let pipe_id2 = create();
    close_reader(pipe_id2);
    match write(pipe_id2, b"test") {
        Err(PipeError::BrokenPipe) => { /* 期待通り */ }
        other => {
            crate::serial_println!("[pipe test] expected BrokenPipe, got {:?}", other);
            close_writer(pipe_id2);
            return false;
        }
    }
    close_writer(pipe_id2);

    true
}
