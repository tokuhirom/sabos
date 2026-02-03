// syscall.rs — システムコールハンドラ
//
// Ring 3（ユーザーモード）から `int 0x80` で呼び出されるシステムコールの処理を行う。
//
// システムコール（system call）とは、ユーザープログラムがカーネルの機能を
// 利用するための仕組み。Ring 3 のコードは直接ハードウェアにアクセスできないため、
// ソフトウェア割り込み `int 0x80` を使って CPU の特権レベルを Ring 0 に上げ、
// カーネルのコードを実行する。
//
// レジスタ規約（Linux の int 0x80 規約に準拠）:
//   rax = システムコール番号
//   rdi = 第1引数
//   rsi = 第2引数
//   戻り値は rax に格納される
//
// アセンブリエントリポイント (syscall_handler_asm):
//   1. 汎用レジスタを保存
//   2. Microsoft x64 ABI に合わせて引数を rcx/rdx/r8 にセット
//   3. Rust の syscall_dispatch() を呼び出す
//   4. 汎用レジスタを復帰（rax は戻り値として上書き）
//   5. iretq でユーザーモードに復帰
//
// 注意: x86_64-unknown-uefi ターゲットでは extern "C" が Microsoft x64 ABI になる。
// System V ABI（Linux）とは引数の渡し方が異なるので注意。
//
// ## 設計原則（CLAUDE.md より）
//
// - null 終端文字列を使わない: すべてのバッファは (ptr, len) 形式
// - UserSlice<T> で型安全にラップ: ユーザー空間ポインタを検証してからアクセス
// - SyscallError で明確なエラー型: 生の数値ではなく型付きエラーを使用

use core::arch::global_asm;
use crate::user_ptr::{UserSlice, SyscallError};

/// システムコール番号の定義
///
/// 番号体系は計画に従う:
/// - コンソール I/O: 0-9
/// - ファイルシステム: 10-19
/// - システム情報: 20-29
/// - プロセス管理: 30-39
/// - ネットワーク: 40-49
/// - システム制御: 50-59
/// - 終了: 60
pub const SYS_READ: u64 = 0;         // read(buf_ptr, len) — コンソールから読み取り
pub const SYS_WRITE: u64 = 1;        // write(buf_ptr, len) — 文字列をカーネルコンソールに出力
pub const SYS_CLEAR_SCREEN: u64 = 2; // clear_screen() — 画面をクリア
pub const SYS_EXIT: u64 = 60;        // exit() — ユーザープログラムを終了してカーネルに戻る

// =================================================================
// アセンブリエントリポイント
// =================================================================
//
// int 0x80 が発火すると CPU は自動的に以下を行う:
//   1. TSS の rsp0 からカーネルスタックに切り替え
//   2. SS, RSP, RFLAGS, CS, RIP をカーネルスタックに push
//   3. IDT 0x80 番のハンドラ（= syscall_handler_asm）にジャンプ
//
// ハンドラ側では汎用レジスタを保存し、Rust 関数を呼び、
// レジスタを復帰して iretq でユーザーモードに戻る。

global_asm!(
    ".global syscall_handler_asm",
    "syscall_handler_asm:",

    // --- 汎用レジスタの保存 ---
    // int 0x80 で CPU が自動保存するのは SS/RSP/RFLAGS/CS/RIP のみ。
    // 残りの汎用レジスタは手動で保存する必要がある。
    "push r11",
    "push r10",
    "push r9",
    "push r8",
    "push rdi",
    "push rsi",
    "push rdx",
    "push rcx",
    "push rbx",
    "push rbp",

    // --- Rust の syscall_dispatch(nr, arg1, arg2) を呼び出す ---
    // UEFI ターゲットは Microsoft x64 ABI を使用する。
    // Microsoft x64 ABI の引数渡し:
    //   第1引数: rcx, 第2引数: rdx, 第3引数: r8
    //
    // int 0x80 のレジスタ規約（Linux 風）:
    //   rax = syscall番号, rdi = arg1, rsi = arg2
    //
    // レジスタの移動:
    "mov r8, rsi",    // arg2 (rsi) → 第3引数 (r8)
    "mov rdx, rdi",   // arg1 (rdi) → 第2引数 (rdx)
    "mov rcx, rax",   // syscall_nr (rax) → 第1引数 (rcx)

    // スタックを 16 バイトアラインする（ABI 要件）
    // push を 10 回 + CPU が 5 個 push = 15 個 × 8 = 120 バイト
    // 120 % 16 = 8 なので、8 バイト追加して 16 の倍数にする。
    // さらに Microsoft x64 ABI ではシャドウスペース（32バイト）が必要。
    // シャドウスペースは呼び出し先が引数をスタックに退避するための領域。
    // 合計: 8 (アライン) + 32 (シャドウ) = 40 バイト確保
    "sub rsp, 40",

    // syscall_dispatch を呼び出す
    "call syscall_dispatch",

    // スタックの調整を元に戻す
    "add rsp, 40",

    // 戻り値は rax に入っている。このまま保持する。

    // --- 汎用レジスタの復帰 ---
    // rax は syscall_dispatch の戻り値なので復帰しない（ユーザーに返す値）
    "pop rbp",
    "pop rbx",
    "pop rcx",
    "pop rdx",
    "pop rsi",
    "pop rdi",
    "pop r8",
    "pop r9",
    "pop r10",
    "pop r11",

    // --- iretq でユーザーモードに復帰 ---
    // CPU が自動的に push した SS/RSP/RFLAGS/CS/RIP を pop して
    // Ring 3 の実行を再開する。
    "iretq",
);

// アセンブリで定義したシンボルを Rust から参照できるようにする
unsafe extern "C" {
    pub safe fn syscall_handler_asm();
}

// =================================================================
// Rust ディスパッチ関数
// =================================================================

/// システムコールのディスパッチ関数。
/// アセンブリエントリポイントから呼ばれる。
///
/// 引数:
///   nr   — システムコール番号（rax から渡される）
///   arg1 — 第1引数（rdi から渡される）
///   arg2 — 第2引数（rsi から渡される）
///
/// 戻り値:
///   rax に格納されてユーザープログラムに返される
///   エラーの場合は負の値（SyscallError::to_errno()）
#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(nr: u64, arg1: u64, arg2: u64) -> u64 {
    // 各システムコールハンドラを呼び出し、Result を u64 に変換
    let result = dispatch_inner(nr, arg1, arg2);
    match result {
        Ok(value) => value,
        Err(err) => err.to_errno(),
    }
}

/// システムコールの内部ディスパッチ関数
///
/// Result 型を返すことで、エラーハンドリングを型安全に行う。
/// ? 演算子でエラーを早期リターンできる。
fn dispatch_inner(nr: u64, arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    match nr {
        SYS_READ => sys_read(arg1, arg2),
        SYS_WRITE => sys_write(arg1, arg2),
        SYS_CLEAR_SCREEN => sys_clear_screen(),
        SYS_EXIT => {
            // exit()
            // ユーザープログラムの終了を要求する。
            // 保存されたカーネルスタック（RSP/RBP）を復元して
            // run_in_usermode() の呼び出し元に return する。
            // この関数は戻らない
            crate::usermode::exit_usermode();
        }
        _ => {
            // 未知のシステムコール番号
            crate::kprintln!("Unknown syscall: {}", nr);
            Err(SyscallError::UnknownSyscall)
        }
    }
}

/// SYS_READ: コンソールから読み取り
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ（最大読み取りバイト数）
///
/// 戻り値:
///   読み取ったバイト数
///
/// 少なくとも1バイト読み取れるまでブロックする。
/// その後、利用可能なデータがあれば最大 len バイトまで読み取って返す。
fn sys_read(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let len = arg2 as usize;

    // 長さ 0 の場合は何もしない
    if len == 0 {
        return Ok(0);
    }

    // UserSlice で型安全にユーザー空間のバッファを取得
    let user_slice = UserSlice::<u8>::from_raw(arg1, len)?;

    // 可変スライスとしてアクセス（書き込み用）
    let buf = user_slice.as_mut_slice();

    // コンソール入力バッファから読み取り（ブロッキング）
    let bytes_read = crate::console::read_input(buf, len);

    Ok(bytes_read as u64)
}

/// SYS_WRITE: コンソールに文字列を出力
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間）
///   arg2 — バッファの長さ（バイト数）
///
/// 戻り値:
///   書き込んだバイト数
///
/// UserSlice を使って型安全にユーザー空間のバッファを検証してからアクセスする。
fn sys_write(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let len = arg2 as usize;

    // UserSlice で型安全にユーザー空間のバッファを取得
    // アドレス範囲、アラインメント、オーバーフローを検証
    let user_slice = UserSlice::<u8>::from_raw(arg1, len)?;

    // UTF-8 として解釈してカーネルコンソールに出力
    // as_str_lossy() は不正な UTF-8 を "<invalid utf-8>" に置換
    let s = user_slice.as_str_lossy();
    crate::kprint!("{}", s);

    // 書き込んだバイト数を返す
    Ok(len as u64)
}

/// SYS_CLEAR_SCREEN: 画面をクリア
///
/// 引数: なし
/// 戻り値: 0（成功）
fn sys_clear_screen() -> Result<u64, SyscallError> {
    crate::framebuffer::clear_global_screen();
    Ok(0)
}
