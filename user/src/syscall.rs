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

// 公開 API は外部のユーザープログラムから使用されるため、dead_code 警告を抑制
#![allow(dead_code)]

use core::arch::asm;

/// システムコール番号の定義
///
/// sabos-syscall クレートで一元管理している。
/// 番号の追加・変更は libs/sabos-syscall/src/lib.rs で行うこと。
pub use sabos_syscall::*;

// =================================================================
// Handle 構造体と権限ビット（Capability-based security）
// =================================================================

/// ファイルハンドル（ユーザー空間用）
///
/// Capability-based security の基盤。ハンドルには権限が埋め込まれており、
/// 持っていない権限の操作はできない。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Handle {
    /// テーブルのインデックス
    pub id: u64,
    /// 偽造防止用のトークン
    pub token: u64,
}

/// マウス状態（ユーザー空間向け）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MouseState {
    pub x: i32,
    pub y: i32,
    pub dx: i32,
    pub dy: i32,
    pub buttons: u8,
    pub _pad: [u8; 3],
}

/// Handle の読み取り権限（ファイル内容を読む）
pub const HANDLE_RIGHT_READ: u32 = 0x0001;
/// Handle の書き込み権限（ファイル内容を書く）
pub const HANDLE_RIGHT_WRITE: u32 = 0x0002;
/// Handle のシーク権限（ファイルポジションを変更）
pub const HANDLE_RIGHT_SEEK: u32 = 0x0004;
/// Handle のメタデータ取得権限（サイズ等を取得）
pub const HANDLE_RIGHT_STAT: u32 = 0x0008;
/// Handle のディレクトリ列挙権限（ディレクトリ内のエントリ一覧）
pub const HANDLE_RIGHT_ENUM: u32 = 0x0010;
/// Handle のファイル作成権限（ディレクトリ内にファイルを作成）
pub const HANDLE_RIGHT_CREATE: u32 = 0x0020;
/// Handle のファイル削除権限（ディレクトリ内のファイルを削除）
pub const HANDLE_RIGHT_DELETE: u32 = 0x0040;
/// Handle の相対パス解決権限（openat でファイルを開く）
pub const HANDLE_RIGHT_LOOKUP: u32 = 0x0080;

/// 読み取り専用ファイル用の権限セット
pub const HANDLE_RIGHTS_FILE_READ: u32 = HANDLE_RIGHT_READ | HANDLE_RIGHT_SEEK | HANDLE_RIGHT_STAT;

/// 読み書き可能ファイル用の権限セット
pub const HANDLE_RIGHTS_FILE_RW: u32 = HANDLE_RIGHT_READ | HANDLE_RIGHT_WRITE | HANDLE_RIGHT_SEEK | HANDLE_RIGHT_STAT;

/// ディレクトリ用の権限セット（フルアクセス）
pub const HANDLE_RIGHTS_DIRECTORY: u32 =
    HANDLE_RIGHT_STAT | HANDLE_RIGHT_ENUM | HANDLE_RIGHT_CREATE | HANDLE_RIGHT_DELETE | HANDLE_RIGHT_LOOKUP;

/// ディレクトリ用の権限セット（読み取りのみ）
pub const HANDLE_RIGHTS_DIRECTORY_READ: u32 =
    HANDLE_RIGHT_STAT | HANDLE_RIGHT_ENUM | HANDLE_RIGHT_LOOKUP;

/// システムコールの戻り値を表す型
///
/// 正の値: 成功（戻り値）
/// 負の値: エラー（errno の負値）
pub type SyscallResult = i64;

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

/// 低レベルシステムコール: 引数5つ
#[inline]
#[allow(dead_code)]
unsafe fn syscall5(nr: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int 0x80",
            in("rax") nr,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
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

/// 画面をクリアする
pub fn clear_screen() {
    unsafe { syscall0(SYS_CLEAR_SCREEN); }
}

/// キーボード入力をノンブロッキングで読み取る
///
/// # 引数
/// - `buf`: 読み取ったデータを格納するバッファ
///
/// # 戻り値
/// - 読み取ったバイト数（0 = 入力なし）
/// - 負の値（エラー時）
///
/// # 動作
/// SYS_MOUSE_READ と同じパターン。入力がなければ即座に 0 を返す。
/// キーボードフォーカスを持つタスクのみが読み取れる。
pub fn key_read(buf: &mut [u8]) -> SyscallResult {
    let ptr = buf.as_mut_ptr() as u64;
    let len = buf.len() as u64;
    unsafe { syscall2(SYS_KEY_READ, ptr, len) as i64 }
}

/// キーボードフォーカスの取得/解放
///
/// # 引数
/// - `grab`: true = フォーカス取得、false = フォーカス解放
///
/// # 戻り値
/// - 0（成功）
///
/// # 動作
/// フォーカスを取得すると、他のタスクの SYS_READ はフォーカスが
/// 解放されるまでブロックされる。GUI サービスなどがキーボード入力を
/// 独占するために使う。
pub fn console_grab(grab: bool) -> SyscallResult {
    unsafe { syscall1(SYS_CONSOLE_GRAB, if grab { 1 } else { 0 }) as i64 }
}

// =================================================================
// 描画（GUI 基盤）
// =================================================================

/// 1 ピクセル描画（RGB）
///
/// # 引数
/// - `x`: X 座標
/// - `y`: Y 座標
/// - `r,g,b`: 色
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn draw_pixel(x: u32, y: u32, r: u8, g: u8, b: u8) -> SyscallResult {
    let rgb = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
    unsafe { syscall3(SYS_DRAW_PIXEL, x as u64, y as u64, rgb as u64) as i64 }
}

/// 矩形塗りつぶし描画（RGB）
///
/// # 引数
/// - `x`: X 座標
/// - `y`: Y 座標
/// - `w`: 幅
/// - `h`: 高さ
/// - `r,g,b`: 色
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn draw_rect(x: u32, y: u32, w: u32, h: u32, r: u8, g: u8, b: u8) -> SyscallResult {
    let packed_wh = ((w as u64) << 32) | (h as u64);
    let rgb = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
    unsafe { syscall4(SYS_DRAW_RECT, x as u64, y as u64, packed_wh, rgb as u64) as i64 }
}

/// 直線描画（RGB）
pub fn draw_line(x0: u32, y0: u32, x1: u32, y1: u32, r: u8, g: u8, b: u8) -> SyscallResult {
    let packed0 = ((x0 as u64) << 32) | (y0 as u64);
    let packed1 = ((x1 as u64) << 32) | (y1 as u64);
    let rgb = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
    unsafe { syscall3(SYS_DRAW_LINE, packed0, packed1, rgb as u64) as i64 }
}

/// 画像描画（RGBX）
pub fn draw_blit(x: u32, y: u32, w: u32, h: u32, buf: &[u8]) -> SyscallResult {
    let packed_wh = ((w as u64) << 32) | (h as u64);
    let ptr = buf.as_ptr() as u64;
    unsafe { syscall4(SYS_DRAW_BLIT, x as u64, y as u64, packed_wh, ptr) as i64 }
}

/// 文字列描画（RGB）
pub fn draw_text(x: u32, y: u32, fg: (u8, u8, u8), bg: (u8, u8, u8), text: &str) -> SyscallResult {
    let packed_xy = ((x as u64) << 32) | (y as u64);
    let fg_rgb = ((fg.0 as u32) << 16) | ((fg.1 as u32) << 8) | (fg.2 as u32);
    let bg_rgb = ((bg.0 as u32) << 16) | ((bg.1 as u32) << 8) | (bg.2 as u32);
    let packed_fg_bg = ((fg_rgb as u64) << 32) | (bg_rgb as u64);
    let ptr = text.as_ptr() as u64;
    let len = text.len() as u64;
    unsafe { syscall4(SYS_DRAW_TEXT, packed_xy, packed_fg_bg, ptr, len) as i64 }
}

// =================================================================
// テスト/デバッグ関連
// =================================================================

/// カーネル selftest を実行する
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn selftest() -> SyscallResult {
    unsafe { syscall0(SYS_SELFTEST) as i64 }
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

// =================================================================
// ブロックデバイス関連
// =================================================================

/// ブロックデバイスから 1 セクタ読み取る（512 バイト固定）
pub fn block_read(sector: u64, buf: &mut [u8]) -> SyscallResult {
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_BLOCK_READ, sector, buf_ptr, buf_len) as i64 }
}

/// ブロックデバイスへ 1 セクタ書き込む（512 バイト固定）
pub fn block_write(sector: u64, buf: &[u8]) -> SyscallResult {
    let buf_ptr = buf.as_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_BLOCK_WRITE, sector, buf_ptr, buf_len) as i64 }
}

// =================================================================
// IPC 関連
// =================================================================

/// IPC メッセージを送信する
pub fn ipc_send(dest_task_id: u64, buf: &[u8]) -> SyscallResult {
    let buf_ptr = buf.as_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_IPC_SEND, dest_task_id, buf_ptr, buf_len) as i64 }
}

/// IPC メッセージを受信する
///
/// sender_out に送信元タスクIDを書き込む。
pub fn ipc_recv(sender_out: &mut u64, buf: &mut [u8], timeout_ms: u64) -> SyscallResult {
    let sender_ptr = sender_out as *mut u64 as u64;
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall4(SYS_IPC_RECV, sender_ptr, buf_ptr, buf_len, timeout_ms) as i64 }
}

// =================================================================
// システム情報関連
// =================================================================

/// メモリ情報を取得
///
/// # 引数
/// - `buf`: 情報を格納するバッファ
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
///
/// # 出力形式（テキスト）
/// ```
/// total_frames=XXXX
/// allocated_frames=XXXX
/// free_frames=XXXX
/// free_kib=XXXX
/// ```
pub fn get_mem_info(buf: &mut [u8]) -> SyscallResult {
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall2(SYS_GET_MEM_INFO, buf_ptr, buf_len) as i64 }
}

/// マウス状態を取得
///
/// # 引数
/// - `state`: 書き込み先
///
/// # 戻り値
/// - 0（更新なし）
/// - 正の値（書き込んだバイト数）
/// - 負の値（エラー）
pub fn mouse_read(state: &mut MouseState) -> SyscallResult {
    let ptr = state as *mut MouseState as u64;
    let len = core::mem::size_of::<MouseState>() as u64;
    unsafe { syscall2(SYS_MOUSE_READ, ptr, len) as i64 }
}

/// タスク一覧を取得
///
/// # 引数
/// - `buf`: 情報を格納するバッファ
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
///
/// # 出力形式（テキスト、CSV 形式）
/// ```
/// id,state,type,name
/// 1,Running,kernel,shell
/// 2,Ready,user,HELLO.ELF
/// ```
pub fn get_task_list(buf: &mut [u8]) -> SyscallResult {
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall2(SYS_GET_TASK_LIST, buf_ptr, buf_len) as i64 }
}

/// ネットワーク情報を取得
///
/// # 引数
/// - `buf`: 情報を格納するバッファ
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
///
/// # 出力形式（テキスト）
/// ```
/// ip=X.X.X.X
/// gateway=X.X.X.X
/// dns=X.X.X.X
/// mac=XX:XX:XX:XX:XX:XX
/// ```
pub fn get_net_info(buf: &mut [u8]) -> SyscallResult {
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall2(SYS_GET_NET_INFO, buf_ptr, buf_len) as i64 }
}

// =================================================================
// フレームバッファ情報
// =================================================================

/// フレームバッファ情報（ユーザー空間向け）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub bytes_per_pixel: u32,
}

/// フレームバッファ情報を取得
pub fn get_fb_info(info: &mut FramebufferInfo) -> SyscallResult {
    let ptr = info as *mut FramebufferInfo as u64;
    let len = core::mem::size_of::<FramebufferInfo>() as u64;
    unsafe { syscall2(SYS_GET_FB_INFO, ptr, len) as i64 }
}

/// PCI Configuration Space を読み取る
///
/// size は 1/2/4 のみ許可。戻り値は下位 32 ビットに格納される。
pub fn pci_config_read(bus: u8, device: u8, function: u8, offset: u8, size: u8) -> SyscallResult {
    let packed = (offset as u64) | ((size as u64) << 8);
    unsafe {
        syscall4(
            SYS_PCI_CONFIG_READ,
            bus as u64,
            device as u64,
            function as u64,
            packed,
        ) as i64
    }
}

// =================================================================
// プロセス管理関連
// =================================================================

/// プログラムを同期実行（フォアグラウンド）
///
/// # 引数
/// - `path`: 実行する ELF ファイルのパス
///
/// # 戻り値
/// - 0（成功時、プログラム終了後）
/// - 負の値（エラー時）
///
/// 指定した ELF ファイルを読み込んで同期実行する。
/// プログラムが終了するまでこの関数はブロックする。
pub fn exec(path: &str) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    unsafe { syscall4(SYS_EXEC, path_ptr, path_len, 0, 0) as i64 }
}

/// バックグラウンドでプロセスを起動
///
/// # 引数
/// - `path`: 実行する ELF ファイルのパス
///
/// # 戻り値
/// - タスク ID（成功時）
/// - 負の値（エラー時）
///
/// 指定した ELF ファイルを読み込んでバックグラウンドで実行する。
/// 即座に戻り、プロセスはスケジューラで管理される。
pub fn spawn(path: &str) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    unsafe { syscall4(SYS_SPAWN, path_ptr, path_len, 0, 0) as i64 }
}

/// プログラムを引数付きで同期実行（フォアグラウンド）
///
/// # 引数
/// - `path`: 実行する ELF ファイルのパス
/// - `args`: コマンドライン引数のスライス
///
/// # 戻り値
/// - 0（成功時、プログラム終了後）
/// - 負の値（エラー時）
///
/// argv は [path, args[0], args[1], ...] の形でカーネルが構築する。
pub fn exec_with_args(path: &str, args: &[&str]) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    if args.is_empty() {
        return unsafe { syscall4(SYS_EXEC, path_ptr, path_len, 0, 0) as i64 };
    }
    let mut buf = [0u8; ARGS_BUF_SIZE];
    let len = build_args_buffer(args, &mut buf);
    unsafe { syscall4(SYS_EXEC, path_ptr, path_len, buf.as_ptr() as u64, len as u64) as i64 }
}

/// バックグラウンドでプロセスを引数付きで起動
///
/// # 引数
/// - `path`: 実行する ELF ファイルのパス
/// - `args`: コマンドライン引数のスライス
///
/// # 戻り値
/// - タスク ID（成功時）
/// - 負の値（エラー時）
///
/// argv は [path, args[0], args[1], ...] の形でカーネルが構築する。
pub fn spawn_with_args(path: &str, args: &[&str]) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    if args.is_empty() {
        return unsafe { syscall4(SYS_SPAWN, path_ptr, path_len, 0, 0) as i64 };
    }
    let mut buf = [0u8; ARGS_BUF_SIZE];
    let len = build_args_buffer(args, &mut buf);
    unsafe { syscall4(SYS_SPAWN, path_ptr, path_len, buf.as_ptr() as u64, len as u64) as i64 }
}

/// 引数バッファの最大サイズ（1KB — コマンドライン引数の合計がこれを超えると切り捨て）
const ARGS_BUF_SIZE: usize = 1024;

/// 引数バッファを固定サイズバッファに構築する。
///
/// フォーマット: [u16 len][bytes][u16 len][bytes]...
/// 各引数は「2バイトのリトルエンディアン長さ」+「その長さ分のバイト列」で連続配置。
///
/// 戻り値: 書き込んだバイト数
fn build_args_buffer(args: &[&str], buf: &mut [u8]) -> usize {
    let mut offset = 0;
    for arg in args {
        let len = arg.len() as u16;
        let needed = 2 + arg.len();
        if offset + needed > buf.len() {
            break; // バッファが足りなければ残りの引数を切り捨て
        }
        let le_bytes = len.to_le_bytes();
        buf[offset] = le_bytes[0];
        buf[offset + 1] = le_bytes[1];
        offset += 2;
        buf[offset..offset + arg.len()].copy_from_slice(arg.as_bytes());
        offset += arg.len();
    }
    offset
}

/// CPU を譲る
///
/// 現在のタスクの実行を中断し、他の ready なタスクに CPU を譲る。
#[allow(dead_code)]
pub fn yield_now() {
    unsafe { syscall1(SYS_YIELD, 0); }
}

/// 指定ミリ秒スリープ
///
/// # 引数
/// - `ms`: スリープ時間（ミリ秒）
///
/// 指定した時間だけ現在のタスクをスリープ状態にする。
pub fn sleep(ms: u64) {
    unsafe { syscall1(SYS_SLEEP, ms); }
}

/// 子プロセスの終了を待つ
///
/// # 引数
/// - `task_id`: 待つ子プロセスのタスク ID (0 なら任意の子)
/// - `timeout_ms`: タイムアウト (ms)。0 なら無期限待ち
///
/// # 戻り値
/// - 終了コード（成功時）
/// - 負の値（エラー時）
///   - -10: 子プロセスがない、または指定したタスクが存在しない
///   - -30: 指定したタスクは子プロセスではない
///   - -42: タイムアウト
///
/// # 動作
/// - `task_id > 0`: 指定した子プロセスの終了を待つ
/// - `task_id == 0`: 任意の子プロセスの終了を待つ
/// - 子プロセスが既に終了していれば即座に戻る
pub fn wait(task_id: u64, timeout_ms: u64) -> SyscallResult {
    unsafe { syscall2(SYS_WAIT, task_id, timeout_ms) as i64 }
}

/// 自分のタスク ID を取得
///
/// # 戻り値
/// 現在のタスク ID（常に成功）
pub fn getpid() -> u64 {
    unsafe { syscall0(SYS_GETPID) }
}

/// タスクを強制終了する
///
/// # 引数
/// - `task_id`: 終了させるタスクの ID
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時: 自分自身を kill、タスク不在、既に終了済み）
pub fn kill(task_id: u64) -> SyscallResult {
    unsafe { syscall1(SYS_KILL, task_id) as i64 }
}

// =================================================================
// 環境変数関連
// =================================================================

/// 環境変数を取得する
///
/// # 引数
/// - `key`: 環境変数のキー
/// - `buf`: 値を書き込むバッファ
///
/// # 戻り値
/// - Ok(n): 値のバイト数（buf[..n] に書き込み済み）
/// - Err(errno): エラー（-20: キーが存在しない、-4: バッファ不足）
pub fn getenv(key: &str, buf: &mut [u8]) -> Result<usize, SyscallResult> {
    let result = unsafe {
        syscall4(
            SYS_GETENV,
            key.as_ptr() as u64,
            key.len() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as i64
    };
    if result < 0 {
        Err(result)
    } else {
        Ok(result as usize)
    }
}

/// 環境変数を設定する
///
/// # 引数
/// - `key`: 環境変数のキー
/// - `value`: 環境変数の値
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn setenv(key: &str, value: &str) -> SyscallResult {
    unsafe {
        syscall4(
            SYS_SETENV,
            key.as_ptr() as u64,
            key.len() as u64,
            value.as_ptr() as u64,
            value.len() as u64,
        ) as i64
    }
}

// =================================================================
// システム制御関連
// =================================================================

/// システム停止
///
/// システムを停止する。この関数は戻らない。
pub fn halt() -> ! {
    unsafe {
        syscall0(SYS_HALT);
    }
    // カーネルが制御を返さないので、ここには到達しない
    loop {}
}

// =================================================================
// ファイルシステム関連（パスベース — レガシー API）
// =================================================================

/// ファイルを削除する（パスベース）
///
/// # 引数
/// - `path`: ファイルパス
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn file_delete(path: &str) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    unsafe { syscall2(SYS_FILE_DELETE, path_ptr, path_len) as i64 }
}

/// ディレクトリ一覧を取得する（パスベース）
///
/// # 引数
/// - `path`: ディレクトリパス
/// - `buf`: 結果を書き込むバッファ（エントリ名を改行区切り）
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
pub fn dir_list(path: &str, buf: &mut [u8]) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall4(SYS_DIR_LIST, path_ptr, path_len, buf_ptr, buf_len) as i64 }
}

/// ファイルを作成/上書きする（パスベース）
///
/// 既にファイルが存在する場合は削除してから作成する。
///
/// # 引数
/// - `path`: ファイルパス
/// - `data`: 書き込むデータ
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn file_write(path: &str, data: &[u8]) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    let data_ptr = data.as_ptr() as u64;
    let data_len = data.len() as u64;
    unsafe { syscall4(SYS_FILE_WRITE, path_ptr, path_len, data_ptr, data_len) as i64 }
}

/// ディレクトリを作成する（パスベース）
///
/// # 引数
/// - `path`: ディレクトリパス
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn dir_create(path: &str) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    unsafe { syscall2(SYS_DIR_CREATE, path_ptr, path_len) as i64 }
}

/// ディレクトリを削除する（パスベース）
///
/// 空のディレクトリのみ削除可能。
///
/// # 引数
/// - `path`: ディレクトリパス
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn dir_remove(path: &str) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    unsafe { syscall2(SYS_DIR_REMOVE, path_ptr, path_len) as i64 }
}

/// ファイルシステム統計情報を取得する
///
/// JSON 形式でファイルシステムの使用状況をバッファに書き込む。
///
/// # 引数
/// - `buf`: 結果を書き込むバッファ
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
pub fn fs_stat(buf: &mut [u8]) -> SyscallResult {
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall2(SYS_FS_STAT, buf_ptr, buf_len) as i64 }
}

// =================================================================
// ファイルハンドル関連（Capability-based security）
// =================================================================

/// ファイルを開く（ハンドルベース）
///
/// Capability-based security の入り口。指定した権限でファイルを開き、
/// ハンドルを取得する。ハンドルは権限を持ち、権限外の操作はできない。
///
/// # 引数
/// - `path`: ファイルパス
/// - `rights`: 要求する権限ビット
///
/// # 戻り値
/// - Ok(Handle): 成功時、ファイルハンドル
/// - Err(errno): エラー時
///
/// # 例
/// ```
/// let handle = open("/HELLO.TXT", HANDLE_RIGHT_READ)?;
/// let mut buf = [0u8; 1024];
/// let n = handle_read(&handle, &mut buf)?;
/// handle_close(&handle)?;
/// ```
pub fn open(path: &str, rights: u32) -> Result<Handle, SyscallResult> {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    let mut handle = Handle { id: 0, token: 0 };
    let handle_ptr = &mut handle as *mut Handle as u64;
    let result = unsafe { syscall4(SYS_OPEN, path_ptr, path_len, handle_ptr, rights as u64) as i64 };
    if result < 0 {
        Err(result)
    } else {
        Ok(handle)
    }
}

/// ハンドルからデータを読み取る
///
/// # 引数
/// - `handle`: ファイルハンドル（READ 権限が必要）
/// - `buf`: 読み取り先バッファ
///
/// # 戻り値
/// - 読み取ったバイト数（成功時、0 は EOF）
/// - 負の値（エラー時）
pub fn handle_read(handle: &Handle, buf: &mut [u8]) -> SyscallResult {
    let handle_ptr = handle as *const Handle as u64;
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_HANDLE_READ, handle_ptr, buf_ptr, buf_len) as i64 }
}

/// ハンドルにデータを書き込む
///
/// # 引数
/// - `handle`: ファイルハンドル（WRITE 権限が必要）
/// - `data`: 書き込むデータ
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
pub fn handle_write(handle: &Handle, data: &[u8]) -> SyscallResult {
    let handle_ptr = handle as *const Handle as u64;
    let data_ptr = data.as_ptr() as u64;
    let data_len = data.len() as u64;
    unsafe { syscall3(SYS_HANDLE_WRITE, handle_ptr, data_ptr, data_len) as i64 }
}

/// ハンドルを閉じる
///
/// # 引数
/// - `handle`: 閉じるハンドル
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn handle_close(handle: &Handle) -> SyscallResult {
    let handle_ptr = handle as *const Handle as u64;
    unsafe { syscall1(SYS_HANDLE_CLOSE, handle_ptr) as i64 }
}

/// ディレクトリハンドルの内容を一覧する
///
/// # 引数
/// - `handle`: ディレクトリハンドル（ENUM 権限が必要）
/// - `buf`: 結果を書き込むバッファ（エントリ名を改行区切り）
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
pub fn handle_enum(handle: &Handle, buf: &mut [u8]) -> SyscallResult {
    let handle_ptr = handle as *const Handle as u64;
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_HANDLE_ENUM, handle_ptr, buf_ptr, buf_len) as i64 }
}

/// ディレクトリハンドルからの相対パスでファイルを開く
///
/// Capability-based security の核心。ディレクトリハンドルが持つ権限の
/// 範囲内でのみファイルを開ける。絶対パスや ".." は禁止。
///
/// # 引数
/// - `dir_handle`: ディレクトリハンドル（LOOKUP 権限が必要）
/// - `path`: 相対パス（絶対パス・".." 禁止）
/// - `rights`: 要求する権限（親の権限以下に制限される）
///
/// # 戻り値
/// - Ok(Handle): 成功時、ファイルハンドル
/// - Err(errno): エラー時
///
/// # セキュリティ
/// - `path` が "/" で始まっていたらエラー
/// - `path` に ".." が含まれていたらエラー（パストラバーサル防止）
/// - 新しいハンドルの権限 = `rights & dir_handle.rights`
pub fn openat(dir_handle: &Handle, path: &str, rights: u32) -> Result<Handle, SyscallResult> {
    let dir_handle_ptr = dir_handle as *const Handle as u64;
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    let mut new_handle = Handle { id: 0, token: 0 };
    let new_handle_ptr = &mut new_handle as *mut Handle as u64;

    // 注: カーネル側では arg4 を new_handle_ptr として使用
    // rights は将来拡張で追加予定
    let _ = rights; // 現在は未使用（カーネル側でデフォルト READ を使用）

    let result = unsafe { syscall4(SYS_OPENAT, dir_handle_ptr, path_ptr, path_len, new_handle_ptr) as i64 };
    if result < 0 {
        Err(result)
    } else {
        Ok(new_handle)
    }
}

/// ハンドルの権限を縮小する
///
/// Capability-based security の重要な操作。権限は縮小のみ可能で、
/// 拡大はできない（セキュリティの要）。
///
/// # 引数
/// - `handle`: 元のハンドル
/// - `new_rights`: 新しい権限（縮小のみ可）
///
/// # 戻り値
/// - Ok(Handle): 成功時、権限を縮小した新しいハンドル
/// - Err(errno): エラー時（権限の拡大を試みた場合など）
///
/// # 例
/// ```
/// // 読み取り専用ハンドルを作成（書き込み権限を削除）
/// let read_only = restrict_rights(&handle, HANDLE_RIGHT_READ)?;
/// ```
pub fn restrict_rights(handle: &Handle, new_rights: u32) -> Result<Handle, SyscallResult> {
    let handle_ptr = handle as *const Handle as u64;
    let mut new_handle = Handle { id: 0, token: 0 };
    let new_handle_ptr = &mut new_handle as *mut Handle as u64;
    let result = unsafe { syscall3(SYS_RESTRICT_RIGHTS, handle_ptr, new_rights as u64, new_handle_ptr) as i64 };
    if result < 0 {
        Err(result)
    } else {
        Ok(new_handle)
    }
}

/// ハンドルのメタデータ（stat 情報）
///
/// ファイルサイズ、種別、権限をまとめて取得するための構造体。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct HandleStat {
    /// ファイルサイズ（バイト）
    pub size: u64,
    /// ハンドルの種別（0 = File, 1 = Directory）
    pub kind: u64,
    /// 現在のハンドルの権限ビット
    pub rights: u64,
}

/// ハンドルのメタデータを取得する
///
/// # 引数
/// - `handle`: ファイルハンドル（STAT 権限が必要）
///
/// # 戻り値
/// - Ok(HandleStat): 成功時
/// - Err(errno): エラー時
pub fn handle_stat(handle: &Handle) -> Result<HandleStat, SyscallResult> {
    let handle_ptr = handle as *const Handle as u64;
    let mut stat = HandleStat { size: 0, kind: 0, rights: 0 };
    let stat_ptr = &mut stat as *mut HandleStat as u64;
    let result = unsafe { syscall2(SYS_HANDLE_STAT, handle_ptr, stat_ptr) as i64 };
    if result < 0 {
        Err(result)
    } else {
        Ok(stat)
    }
}

/// シーク方向の定数: ファイル先頭からの絶対位置
pub const SEEK_SET: u64 = 0;
/// シーク方向の定数: 現在位置からの相対オフセット
pub const SEEK_CUR: u64 = 1;
/// シーク方向の定数: ファイル末尾からの相対オフセット
pub const SEEK_END: u64 = 2;

/// ファイルポジションを変更する
///
/// # 引数
/// - `handle`: ファイルハンドル（SEEK 権限が必要）
/// - `offset`: オフセット値（i64、SEEK_CUR/SEEK_END で負の値あり）
/// - `whence`: シーク方向（SEEK_SET / SEEK_CUR / SEEK_END）
///
/// # 戻り値
/// - Ok(new_pos): 新しいファイルポジション
/// - Err(errno): エラー時
pub fn handle_seek(handle: &Handle, offset: i64, whence: u64) -> Result<u64, SyscallResult> {
    let handle_ptr = handle as *const Handle as u64;
    let result = unsafe { syscall3(SYS_HANDLE_SEEK, handle_ptr, offset as u64, whence) as i64 };
    if result < 0 {
        Err(result)
    } else {
        Ok(result as u64)
    }
}

// =================================================================
// 時刻・乱数
// =================================================================

/// 起動からの経過ミリ秒を取得する。
///
/// PIT タイマーのティックカウントをミリ秒に変換した値が返る。
/// std::time::Instant の代替として使用できる。
#[allow(dead_code)]
pub fn clock_monotonic() -> u64 {
    unsafe { syscall0(SYS_CLOCK_MONOTONIC) }
}

/// ランダムバイトをバッファに書き込む。
///
/// RDRAND 命令（ハードウェア乱数生成器）を使って暗号学的に安全な
/// ランダムバイトを生成する。HashMap の RandomState 等で使用される。
///
/// # 戻り値
/// - Ok(n): 書き込んだバイト数
/// - Err(errno): エラー時
#[allow(dead_code)]
pub fn getrandom(buf: &mut [u8]) -> Result<usize, SyscallResult> {
    let result = unsafe {
        syscall2(SYS_GETRANDOM, buf.as_mut_ptr() as u64, buf.len() as u64) as i64
    };
    if result < 0 {
        Err(result)
    } else {
        Ok(result as usize)
    }
}

/// mmap のプロテクションフラグ: 読み取り可能
pub const MMAP_PROT_READ: u64 = 0x1;
/// mmap のプロテクションフラグ: 書き込み可能
pub const MMAP_PROT_WRITE: u64 = 0x2;
/// mmap のフラグ: 匿名マッピング（ファイルに紐付かない）
pub const MMAP_FLAG_ANONYMOUS: u64 = 0x1;

/// 匿名メモリをマッピングする（mmap）。
///
/// ユーザー空間に新しいゼロ初期化済みページを動的に確保する。
/// POSIX の mmap(NULL, len, PROT_READ|PROT_WRITE, MAP_ANONYMOUS|MAP_PRIVATE, -1, 0)
/// に相当する。
///
/// # 引数
/// - `addr_hint`: マッピング先アドレスのヒント（0 ならカーネルが決定）
/// - `len`: 確保するバイト数（4KiB 単位にアラインされる）
/// - `prot`: プロテクションフラグ（MMAP_PROT_READ | MMAP_PROT_WRITE）
/// - `flags`: マッピングフラグ（MMAP_FLAG_ANONYMOUS）
///
/// # 戻り値
/// - Ok(ptr): マッピングされたメモリの先頭アドレス
/// - Err(errno): エラー時
#[allow(dead_code)]
pub fn mmap(addr_hint: u64, len: usize, prot: u64, flags: u64) -> Result<*mut u8, SyscallResult> {
    let result = unsafe {
        syscall4(SYS_MMAP, addr_hint, len as u64, prot, flags) as i64
    };
    if result < 0 {
        Err(result)
    } else {
        Ok(result as *mut u8)
    }
}

/// メモリマッピングを解除する（munmap）。
///
/// mmap で確保したメモリを解放する。
///
/// # 引数
/// - `addr`: 解除する先頭アドレス（4KiB アライン必須）
/// - `len`: 解除するバイト数
///
/// # 戻り値
/// - Ok(0): 成功
/// - Err(errno): エラー時
#[allow(dead_code)]
pub fn munmap(addr: *mut u8, len: usize) -> Result<u64, SyscallResult> {
    let result = unsafe {
        syscall2(SYS_MUNMAP, addr as u64, len as u64) as i64
    };
    if result < 0 {
        Err(result)
    } else {
        Ok(result as u64)
    }
}

// =================================================================
// サウンド関連
// =================================================================

/// AC97 ドライバで正弦波ビープ音を再生する。
///
/// # 引数
/// - `freq_hz`: 周波数 (Hz)。1〜20000 の範囲。
/// - `duration_ms`: 持続時間 (ミリ秒)。1〜10000 の範囲。
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時: 引数範囲外、AC97 未検出）
pub fn sound_play(freq_hz: u32, duration_ms: u32) -> SyscallResult {
    unsafe { syscall2(SYS_SOUND_PLAY, freq_hz as u64, duration_ms as u64) as i64 }
}

// =================================================================
// Futex 関連
// =================================================================

/// Futex 操作コード: 値が一致したらスリープ
pub const FUTEX_WAIT: u64 = 0;
/// Futex 操作コード: 待機中のタスクを起床
pub const FUTEX_WAKE: u64 = 1;

/// FUTEX_WAIT: ユーザー空間アドレスの値が expected と一致したらスリープ
///
/// # 引数
/// - `addr`: AtomicU32 のアドレス
/// - `expected`: 期待する値（一致したらスリープ）
/// - `timeout_ms`: タイムアウト (ms)。0 なら無期限待ち。
///
/// # 戻り値
/// - 0（起床した）
/// - 負の値（値が不一致で即リターン、等）
pub fn futex_wait(addr: *const u32, expected: u32, timeout_ms: u64) -> SyscallResult {
    unsafe {
        syscall4(SYS_FUTEX, addr as u64, FUTEX_WAIT, expected as u64, timeout_ms) as i64
    }
}

/// FUTEX_WAKE: ユーザー空間アドレスで待機中のタスクを最大 count 個起床させる
///
/// # 引数
/// - `addr`: AtomicU32 のアドレス
/// - `count`: 起床させる最大タスク数
///
/// # 戻り値
/// - 起床したタスクの数
/// - 負の値（エラー時）
pub fn futex_wake(addr: *const u32, count: u32) -> SyscallResult {
    unsafe {
        syscall4(SYS_FUTEX, addr as u64, FUTEX_WAKE, count as u64, 0) as i64
    }
}

// =================================================================
// スレッド関連
// =================================================================

/// スレッドを作成する
///
/// # 引数
/// - `entry_ptr`: スレッドのエントリポイント関数ポインタ
/// - `stack_ptr`: スレッド用スタックのトップアドレス（mmap で確保済み）
/// - `arg`: スレッドに渡す引数（rdi レジスタにセット）
///
/// # 戻り値
/// - スレッドのタスク ID（成功時）
/// - 負の値（エラー時）
pub fn thread_create(entry_ptr: u64, stack_ptr: u64, arg: u64) -> SyscallResult {
    unsafe { syscall3(SYS_THREAD_CREATE, entry_ptr, stack_ptr, arg) as i64 }
}

/// 現在のスレッドを終了する
///
/// # 引数
/// - `exit_code`: 終了コード
pub fn thread_exit(exit_code: i32) -> ! {
    unsafe { syscall1(SYS_THREAD_EXIT, exit_code as u64); }
    // 戻ることはないが、コンパイラを満足させるためにループ
    loop {}
}

/// スレッドの終了を待つ
///
/// # 引数
/// - `thread_id`: 待つスレッドのタスク ID
/// - `timeout_ms`: タイムアウト (ms)。0 なら無期限待ち。
///
/// # 戻り値
/// - 終了コード（成功時）
/// - 負の値（エラー時）
pub fn thread_join(thread_id: u64, timeout_ms: u64) -> SyscallResult {
    unsafe { syscall2(SYS_THREAD_JOIN, thread_id, timeout_ms) as i64 }
}
