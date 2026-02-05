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

/// システムコール番号の定義（カーネルの syscall.rs と一致させる）
///
/// 番号体系:
/// - コンソール I/O: 0-9
/// - テスト/デバッグ: 10-11
/// - ファイルシステム: 12-19
/// - システム情報: 20-29
/// - プロセス管理: 30-39
/// - システム制御: 50-59
/// - 終了: 60
/// - ファイルハンドル: 70-79
/// - ブロックデバイス: 80-89
/// - IPC: 90-99
// コンソール I/O (0-9)
pub const SYS_READ: u64 = 0;         // read(buf_ptr, len) — コンソールから読み取り
pub const SYS_WRITE: u64 = 1;        // write(buf_ptr, len) — コンソールに出力
pub const SYS_CLEAR_SCREEN: u64 = 2; // clear_screen() — 画面クリア

// テスト/デバッグ (10-11)
pub const SYS_SELFTEST: u64 = 10;    // selftest() — カーネル selftest を実行

// ファイルシステム (12-19) — パスベース（レガシー）
pub const SYS_FILE_DELETE: u64 = 12; // file_delete(path_ptr, path_len)
pub const SYS_DIR_LIST: u64 = 13;    // dir_list(path_ptr, path_len, buf_ptr, buf_len)

// システム情報 (20-29)
pub const SYS_GET_MEM_INFO: u64 = 20;   // get_mem_info(buf_ptr, buf_len) — メモリ情報
pub const SYS_GET_TASK_LIST: u64 = 21;  // get_task_list(buf_ptr, buf_len) — タスク一覧
pub const SYS_GET_NET_INFO: u64 = 22;   // get_net_info(buf_ptr, buf_len) — ネットワーク情報
pub const SYS_PCI_CONFIG_READ: u64 = 23; // pci_config_read(bus, device, function, offset, size) — PCI Config 読み取り
pub const SYS_GET_FB_INFO: u64 = 24;    // get_fb_info(buf_ptr, buf_len) — フレームバッファ情報
pub const SYS_MOUSE_READ: u64 = 25;     // mouse_read(buf_ptr, buf_len) — マウス状態取得

// プロセス管理 (30-39)
pub const SYS_EXEC: u64 = 30;    // exec(path_ptr, path_len) — プログラムを同期実行
pub const SYS_SPAWN: u64 = 31;   // spawn(path_ptr, path_len) — バックグラウンドでプロセス起動
pub const SYS_YIELD: u64 = 32;   // yield() — CPU を譲る
pub const SYS_SLEEP: u64 = 33;   // sleep(ms) — 指定ミリ秒スリープ
pub const SYS_WAIT: u64 = 34;    // wait(task_id, timeout_ms) — 子プロセスの終了を待つ
pub const SYS_GETPID: u64 = 35;  // getpid() — 自分のタスク ID を取得

// システム制御 (50-59)
pub const SYS_HALT: u64 = 50;        // halt() — システム停止
pub const SYS_DRAW_PIXEL: u64 = 51;  // draw_pixel(x, y, rgb) — 1ピクセル描画
pub const SYS_DRAW_RECT: u64 = 52;   // draw_rect(x, y, w_h, rgb) — 矩形描画（w/h は packed）
pub const SYS_DRAW_LINE: u64 = 53;   // draw_line(xy0, xy1, rgb) — 直線描画（x,y は packed）
pub const SYS_DRAW_BLIT: u64 = 54;   // draw_blit(x, y, w_h, buf_ptr) — 画像描画
pub const SYS_DRAW_TEXT: u64 = 55;   // draw_text(xy, fg_bg, buf_ptr, len) — 文字列描画

// 終了 (60)
pub const SYS_EXIT: u64 = 60;        // exit() — プログラム終了

// ファイルハンドル (70-79) — Capability-based security
pub const SYS_OPEN: u64 = 70;            // open(path_ptr, path_len, handle_ptr, rights)
pub const SYS_HANDLE_READ: u64 = 71;     // handle_read(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_WRITE: u64 = 72;    // handle_write(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_CLOSE: u64 = 73;    // handle_close(handle_ptr)
pub const SYS_OPENAT: u64 = 74;          // openat(dir_handle_ptr, path_ptr, path_len, new_handle_ptr, rights)
pub const SYS_RESTRICT_RIGHTS: u64 = 75; // restrict_rights(handle_ptr, new_rights, new_handle_ptr)
pub const SYS_HANDLE_ENUM: u64 = 76;     // handle_enum(dir_handle_ptr, buf_ptr, len)

// ブロックデバイス (80-89)
pub const SYS_BLOCK_READ: u64 = 80;   // block_read(sector, buf_ptr, len)
pub const SYS_BLOCK_WRITE: u64 = 81;  // block_write(sector, buf_ptr, len)

// IPC (90-99)
pub const SYS_IPC_SEND: u64 = 90;     // ipc_send(dest_task_id, buf_ptr, len)
pub const SYS_IPC_RECV: u64 = 91;     // ipc_recv(sender_ptr, buf_ptr, buf_len, timeout_ms)

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
    unsafe { syscall2(SYS_EXEC, path_ptr, path_len) as i64 }
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
    unsafe { syscall2(SYS_SPAWN, path_ptr, path_len) as i64 }
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
