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
/// - ファイルハンドル: 70-79
// コンソール I/O (0-9)
pub const SYS_READ: u64 = 0;         // read(buf_ptr, len) — コンソールから読み取り
pub const SYS_WRITE: u64 = 1;        // write(buf_ptr, len) — コンソールに出力
pub const SYS_CLEAR_SCREEN: u64 = 2; // clear_screen() — 画面クリア

// ファイルシステム (10-19)
pub const SYS_FILE_READ: u64 = 10;   // file_read(path_ptr, path_len, buf_ptr, buf_len)
pub const SYS_FILE_WRITE: u64 = 11;  // file_write(path_ptr, path_len, data_ptr, data_len)
pub const SYS_FILE_DELETE: u64 = 12; // file_delete(path_ptr, path_len)
pub const SYS_DIR_LIST: u64 = 13;    // dir_list(path_ptr, path_len, buf_ptr, buf_len)

// システム情報 (20-29)
pub const SYS_GET_MEM_INFO: u64 = 20;   // get_mem_info(buf_ptr, buf_len) — メモリ情報
pub const SYS_GET_TASK_LIST: u64 = 21;  // get_task_list(buf_ptr, buf_len) — タスク一覧
pub const SYS_GET_NET_INFO: u64 = 22;   // get_net_info(buf_ptr, buf_len) — ネットワーク情報
pub const SYS_PCI_CONFIG_READ: u64 = 23; // pci_config_read(bus, device, function, offset, size) — PCI Config 読み取り

// プロセス管理 (30-39)
pub const SYS_EXEC: u64 = 30;    // exec(path_ptr, path_len) — プログラムを同期実行
pub const SYS_SPAWN: u64 = 31;   // spawn(path_ptr, path_len) — バックグラウンドでプロセス起動
pub const SYS_YIELD: u64 = 32;   // yield() — CPU を譲る
pub const SYS_SLEEP: u64 = 33;   // sleep(ms) — 指定ミリ秒スリープ

// ネットワーク (40-49)
pub const SYS_DNS_LOOKUP: u64 = 40;  // dns_lookup(domain_ptr, domain_len, ip_ptr) — DNS 解決
pub const SYS_TCP_CONNECT: u64 = 41; // tcp_connect(ip_ptr, port) — TCP 接続
pub const SYS_TCP_SEND: u64 = 42;    // tcp_send(data_ptr, data_len) — TCP 送信
pub const SYS_TCP_RECV: u64 = 43;    // tcp_recv(buf_ptr, buf_len, timeout_ms) — TCP 受信
pub const SYS_TCP_CLOSE: u64 = 44;   // tcp_close() — TCP 切断

// システム制御 (50-59)
pub const SYS_HALT: u64 = 50;        // halt() — システム停止

// 終了 (60)
pub const SYS_EXIT: u64 = 60;        // exit() — プログラム終了

// ファイルハンドル (70-79)
pub const SYS_OPEN: u64 = 70;         // open(path_ptr, path_len, handle_ptr, rights)
pub const SYS_HANDLE_READ: u64 = 71;  // handle_read(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_WRITE: u64 = 72; // handle_write(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_CLOSE: u64 = 73; // handle_close(handle_ptr)

/// Handle の読み取り権限
pub const HANDLE_RIGHT_READ: u32 = 0x01;
/// Handle の書き込み権限
pub const HANDLE_RIGHT_WRITE: u32 = 0x02;

/// ユーザー空間に渡されるハンドル
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Handle {
    pub id: u64,
    pub token: u64,
}

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
#[allow(dead_code)]
pub const EREADONLY: i64 = -1001;       // 書き込み禁止
#[allow(dead_code)]
pub const EINVALID_HANDLE: i64 = -1002; // 不正なハンドル
#[allow(dead_code)]
pub const ENOT_SUPPORTED: i64 = -1003;  // 未対応

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

/// 画面をクリアする
pub fn clear_screen() {
    unsafe { syscall0(SYS_CLEAR_SCREEN); }
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
// ファイルシステム関連
// =================================================================

/// ファイルの内容を読み取る
///
/// # 引数
/// - `path`: ファイルパス（例: "/HELLO.TXT"）
/// - `buf`: 読み取ったデータを格納するバッファ
///
/// # 戻り値
/// - 読み取ったバイト数（成功時）
/// - 負の値（エラー時）
pub fn file_read(path: &str, buf: &mut [u8]) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall4(SYS_FILE_READ, path_ptr, path_len, buf_ptr, buf_len) as i64 }
}

/// ファイルを作成または上書き
///
/// # 引数
/// - `path`: ファイルパス（ルートディレクトリのみ対応、例: "HELLO.TXT"）
/// - `data`: 書き込むデータ
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
pub fn file_write(path: &str, data: &[u8]) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    let data_ptr = data.as_ptr() as u64;
    let data_len = data.len() as u64;
    unsafe { syscall4(SYS_FILE_WRITE, path_ptr, path_len, data_ptr, data_len) as i64 }
}

/// ファイルを削除
///
/// # 引数
/// - `path`: ファイルパス（ルートディレクトリのみ対応、例: "HELLO.TXT"）
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn file_delete(path: &str) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    unsafe { syscall2(SYS_FILE_DELETE, path_ptr, path_len) as i64 }
}

/// ディレクトリの内容を一覧
///
/// # 引数
/// - `path`: ディレクトリパス（"/" ならルート）
/// - `buf`: エントリ名を改行区切りで格納するバッファ
///
/// # 戻り値
/// - 書き込んだバイト数（成功時）
/// - 負の値（エラー時）
///
/// # 出力形式
/// ファイル名を改行区切りで出力。ディレクトリには末尾に "/" が付く。
pub fn dir_list(path: &str, buf: &mut [u8]) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall4(SYS_DIR_LIST, path_ptr, path_len, buf_ptr, buf_len) as i64 }
}

// =================================================================
// ハンドル関連
// =================================================================

/// ファイルを開いて Handle を取得する
///
/// # 引数
/// - `path`: ファイルパス
/// - `rights`: 権限ビット（HANDLE_RIGHT_READ など）
/// - `handle_out`: 取得した Handle の書き込み先
pub fn open(path: &str, rights: u32, handle_out: &mut Handle) -> SyscallResult {
    let path_ptr = path.as_ptr() as u64;
    let path_len = path.len() as u64;
    let handle_ptr = handle_out as *mut Handle as u64;
    unsafe { syscall4(SYS_OPEN, path_ptr, path_len, handle_ptr, rights as u64) as i64 }
}

/// Handle から読み取る
pub fn handle_read(handle: &Handle, buf: &mut [u8]) -> SyscallResult {
    let handle_ptr = handle as *const Handle as u64;
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_HANDLE_READ, handle_ptr, buf_ptr, buf_len) as i64 }
}

/// Handle に書き込む
pub fn handle_write(handle: &Handle, buf: &[u8]) -> SyscallResult {
    let handle_ptr = handle as *const Handle as u64;
    let buf_ptr = buf.as_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_HANDLE_WRITE, handle_ptr, buf_ptr, buf_len) as i64 }
}

/// Handle を閉じる
pub fn handle_close(handle: &Handle) -> SyscallResult {
    let handle_ptr = handle as *const Handle as u64;
    unsafe { syscall1(SYS_HANDLE_CLOSE, handle_ptr) as i64 }
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
pub fn yield_cpu() {
    unsafe { syscall0(SYS_YIELD); }
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

// =================================================================
// ネットワーク関連
// =================================================================

/// DNS 解決
///
/// # 引数
/// - `domain`: ドメイン名
/// - `ip_out`: 解決した IP アドレスを格納する配列（4 バイト）
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn dns_lookup(domain: &str, ip_out: &mut [u8; 4]) -> SyscallResult {
    let domain_ptr = domain.as_ptr() as u64;
    let domain_len = domain.len() as u64;
    let ip_ptr = ip_out.as_mut_ptr() as u64;
    unsafe { syscall3(SYS_DNS_LOOKUP, domain_ptr, domain_len, ip_ptr) as i64 }
}

/// TCP 接続
///
/// # 引数
/// - `ip`: 接続先 IP アドレス（4 バイト）
/// - `port`: 接続先ポート番号
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn tcp_connect(ip: &[u8; 4], port: u16) -> SyscallResult {
    let ip_ptr = ip.as_ptr() as u64;
    let port_val = port as u64;
    unsafe { syscall2(SYS_TCP_CONNECT, ip_ptr, port_val) as i64 }
}

/// TCP 送信
///
/// # 引数
/// - `data`: 送信するデータ
///
/// # 戻り値
/// - 送信したバイト数（成功時）
/// - 負の値（エラー時）
pub fn tcp_send(data: &[u8]) -> SyscallResult {
    let data_ptr = data.as_ptr() as u64;
    let data_len = data.len() as u64;
    unsafe { syscall2(SYS_TCP_SEND, data_ptr, data_len) as i64 }
}

/// TCP 受信
///
/// # 引数
/// - `buf`: 受信データを格納するバッファ
/// - `timeout_ms`: タイムアウト（ミリ秒）
///
/// # 戻り値
/// - 受信したバイト数（成功時）
/// - 0（タイムアウトまたは接続終了時）
/// - 負の値（エラー時）
pub fn tcp_recv(buf: &mut [u8], timeout_ms: u64) -> SyscallResult {
    let buf_ptr = buf.as_mut_ptr() as u64;
    let buf_len = buf.len() as u64;
    unsafe { syscall3(SYS_TCP_RECV, buf_ptr, buf_len, timeout_ms) as i64 }
}

/// TCP 切断
///
/// # 戻り値
/// - 0（成功時）
/// - 負の値（エラー時）
pub fn tcp_close() -> SyscallResult {
    unsafe { syscall0(SYS_TCP_CLOSE) as i64 }
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
