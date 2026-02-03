// syscall.rs — ユーザー空間システムコールライブラリ
//
// SABOS のシステムコールをユーザープログラムから呼び出すためのラッパー関数。
// int 0x80 でカーネルに要求を送り、結果を受け取る。
//
// ## レジスタ規約（Linux の int 0x80 規約に準拠）
//
// - rax: システムコール番号
// - rdi: 第1引数
// - rsi: 第2引数
// - rdx: 第3引数（将来用）
// - r10: 第4引数（将来用）
// - r8:  第5引数（将来用）
// - r9:  第6引数（将来用）
// - 戻り値: rax
//
// ## 使用例
//
// ```
// use syscall::{write, exit};
//
// fn main() {
//     write(b"Hello, SABOS!\n");
//     exit();
// }
// ```

use core::arch::asm;

/// システムコール番号の定義（カーネルの syscall.rs と一致させる）
///
/// 番号体系:
/// - コンソール I/O: 0-9
/// - ファイルシステム: 10-19
/// - システム情報: 20-29
/// - プロセス管理: 30-39
/// - ネットワーク: 40-49
/// - システム制御: 50-59
/// - 終了: 60
pub const SYS_READ: u64 = 0;   // read(buf_ptr, len) — コンソールから読み取り
pub const SYS_WRITE: u64 = 1;  // write(buf_ptr, len) — コンソールに出力
pub const SYS_EXIT: u64 = 60;  // exit() — プログラム終了

/// システムコールの戻り値を表す型
///
/// 正の値: 成功（戻り値）
/// 負の値: エラー（errno の負値）
pub type SyscallResult = i64;

/// エラーコード（カーネルの SyscallError と対応）
#[allow(dead_code)]
pub const EFAULT: i64 = -14;   // 不正なアドレス
#[allow(dead_code)]
pub const EINVAL: i64 = -22;   // 不正な引数
#[allow(dead_code)]
pub const ENOENT: i64 = -2;    // ファイルが見つからない
#[allow(dead_code)]
pub const ENOSYS: i64 = -38;   // 未実装のシステムコール

/// 戻り値がエラーかどうかをチェック
#[inline]
#[allow(dead_code)]
pub fn is_error(result: u64) -> bool {
    // 負の値として解釈できる大きな値はエラー
    // i64 として解釈して負ならエラー
    (result as i64) < 0
}

/// エラーコードを取得（エラーの場合のみ呼ぶこと）
#[inline]
#[allow(dead_code)]
pub fn get_errno(result: u64) -> i64 {
    result as i64
}

/// 低レベルシステムコール: 引数なし
#[inline]
unsafe fn syscall0(nr: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            lateout("rax") ret,
            // int 0x80 で上書きされる可能性があるレジスタを clobber 指定
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// 低レベルシステムコール: 引数1つ
#[inline]
#[allow(dead_code)]
unsafe fn syscall1(nr: u64, arg1: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") arg1,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// 低レベルシステムコール: 引数2つ
#[inline]
unsafe fn syscall2(nr: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") arg1,
            in("rsi") arg2,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// 低レベルシステムコール: 引数3つ
#[inline]
#[allow(dead_code)]
unsafe fn syscall3(nr: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// 低レベルシステムコール: 引数4つ
#[inline]
#[allow(dead_code)]
unsafe fn syscall4(nr: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

// =================================================================
// 高レベル API: ユーザーが使うラッパー関数
// =================================================================

/// コンソールからバイト列を読み取る
///
/// # 引数
/// - `buf`: 読み取ったデータを格納するバッファ
///
/// # 戻り値
/// - 読み取ったバイト数（成功時）
/// - 負の値（エラー時）
///
/// # 動作
/// - 少なくとも1バイト読み取れるまでブロックする
/// - その後、利用可能なデータがあれば最大 buf.len() バイトまで読み取る
///
/// # 例
/// ```
/// let mut buf = [0u8; 64];
/// let n = read(&mut buf);
/// if n > 0 {
///     // buf[0..n] に読み取ったデータが入っている
/// }
/// ```
pub fn read(buf: &mut [u8]) -> SyscallResult {
    let ptr = buf.as_mut_ptr() as u64;
    let len = buf.len() as u64;
    unsafe { syscall2(SYS_READ, ptr, len) as i64 }
}

/// コンソールから1文字を読み取る
///
/// 1文字読み取れるまでブロックする。
/// 非 ASCII 文字は '?' に置換される。
pub fn read_char() -> char {
    let mut buf = [0u8; 1];
    read(&mut buf);
    buf[0] as char
}

/// コンソールにバイト列を出力する
///
/// # 引数
/// - `buf`: 出力するバイト列のスライス
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
///
/// # 例
/// ```
/// write(b"Hello, SABOS!\n");
/// ```
pub fn write(buf: &[u8]) -> SyscallResult {
    let ptr = buf.as_ptr() as u64;
    let len = buf.len() as u64;
    unsafe { syscall2(SYS_WRITE, ptr, len) as i64 }
}

/// コンソールに文字列を出力する
///
/// `write()` の文字列版。UTF-8 文字列を受け取る。
pub fn write_str(s: &str) -> SyscallResult {
    write(s.as_bytes())
}

/// プログラムを終了する
///
/// この関数は戻らない。カーネルがプロセスを終了し、
/// 呼び出し元（シェルなど）に制御を返す。
pub fn exit() -> ! {
    unsafe {
        syscall0(SYS_EXIT);
    }
    // カーネルが制御を返さないので、ここには到達しない
    // しかし Rust の型システムを満たすために無限ループ
    loop {}
}

/// プログラムを終了する（exit の別名）
///
/// C 言語の _exit() に相当。
#[allow(dead_code)]
pub fn _exit() -> ! {
    exit()
}
