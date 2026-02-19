// syscall/mod.rs — システムコールハンドラ（エントリポイント・ディスパッチ）
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

// サブモジュール
mod console;
mod graphics;
mod handle;
mod filesystem;
mod ipc;
mod process;
mod network;
mod sysinfo;
mod misc;

use core::arch::global_asm;
use crate::user_ptr::{UserPtr, UserSlice, SyscallError};

/// システムコール番号の定義
///
/// sabos-syscall クレートで一元管理している。
/// 番号の追加・変更は libs/sabos-syscall/src/lib.rs で行うこと。
pub use sabos_syscall::*;

// 外部から参照される公開 API を re-export
pub use process::{exec_for_test, exec_spawn_for_test, exec_with_args_for_test};
pub use filesystem::list_dir_to_buffer_for_test;
pub(crate) use handle::open_path_to_handle;
pub(crate) use ipc::sys_block_read;

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
    // 割り込みを無効化（カーネル内の再入を防ぐ）
    "cli",

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

    // --- Rust の syscall_dispatch(nr, arg1, arg2, arg3, arg4) を呼び出す ---
    // UEFI ターゲットは Microsoft x64 ABI を使用する。
    // Microsoft x64 ABI の引数渡し:
    //   第1引数: rcx, 第2引数: rdx, 第3引数: r8, 第4引数: r9
    //   第5引数以降はスタック経由
    //
    // int 0x80 のレジスタ規約（Linux 風）:
    //   rax = syscall番号, rdi = arg1, rsi = arg2, rdx = arg3, r10 = arg4
    //
    // レジスタの移動（順序が重要: 後で使うレジスタを先に移動）:
    // 注意: rdx は Linux ABI では arg3 だが、保存した値を使う必要がある
    // スタックに保存された rdx の位置: rbp(0) + rbx(8) + rcx(16) + rdx(24) からの位置
    // = [rsp + 24] が保存された rdx

    // スタックを 16 バイトアラインする（ABI 要件）
    // push を 10 回 + CPU が 5 個 push = 15 個 × 8 = 120 バイト
    // 120 % 16 = 8 なので、8 バイト追加して 16 の倍数にする。
    // さらに Microsoft x64 ABI ではシャドウスペース（32バイト）が必要。
    // 合計: 8 (アライン) + 32 (シャドウ) = 40 バイト確保
    "sub rsp, 40",

    // 第5引数 (arg4 = r10) をスタックに積む（Microsoft ABI では第5引数以降はスタック）
    "mov qword ptr [rsp+32], r10",  // arg4 → スタックの第5引数位置

    // 引数をセット（Microsoft ABI: rcx, rdx, r8, r9）
    "mov r9, rdx",    // arg3 (rdx) → 第4引数 (r9) ※先に移動
    "mov r8, rsi",    // arg2 (rsi) → 第3引数 (r8)
    "mov rdx, rdi",   // arg1 (rdi) → 第2引数 (rdx)
    "mov rcx, rax",   // syscall_nr (rax) → 第1引数 (rcx)

    // syscall_dispatch を呼び出す
    "call syscall_dispatch",
    // syscall 内で割り込みを有効化しても、復帰前に無効化しておく
    "cli",

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
///   arg3 — 第3引数（rdx から渡される）
///   arg4 — 第4引数（r10 から渡される）
///
/// 戻り値:
///   rax に格納されてユーザープログラムに返される
///   エラーの場合は負の値（SyscallError::to_errno()）
#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(nr: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    // 各システムコールハンドラを呼び出し、Result を u64 に変換
    let result = dispatch_inner(nr, arg1, arg2, arg3, arg4);
    match result {
        Ok(value) => value,
        Err(err) => err.to_errno(),
    }
}

/// syscall 引数のユーザー空間バッファを検証して取得する（共通ヘルパー）
pub(crate) fn user_slice_from_args(arg_ptr: u64, arg_len: u64) -> Result<UserSlice<u8>, SyscallError> {
    let len = usize::try_from(arg_len).map_err(|_| SyscallError::InvalidArgument)?;
    UserSlice::<u8>::from_raw(arg_ptr, len)
}

/// syscall 引数のユーザー空間ポインタを検証して取得する（共通ヘルパー）
pub(crate) fn user_ptr_from_arg<T>(arg: u64) -> Result<UserPtr<T>, SyscallError> {
    UserPtr::<T>::from_raw(arg)
}

// =================================================================
// ユーティリティ: スライスへの書き込み
// =================================================================

/// バイトスライスに書き込むための Write 実装
///
/// fmt::Write トレイトを実装して、write! / writeln! マクロを使えるようにする。
/// バッファがいっぱいになったら書き込みを止める（パニックしない）。
pub(crate) struct SliceWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> SliceWriter<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn written(&self) -> usize {
        self.pos
    }
}

impl<'a> core::fmt::Write for SliceWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        let to_write = core::cmp::min(bytes.len(), remaining);
        self.buf[self.pos..self.pos + to_write].copy_from_slice(&bytes[..to_write]);
        self.pos += to_write;
        Ok(())
    }
}

/// JSON 文字列用のエスケープ付き書き込み
pub(crate) fn write_json_string(writer: &mut SliceWriter<'_>, s: &str) -> core::fmt::Result {
    use core::fmt::Write;

    let mut buf = [0u8; 4];
    for ch in s.chars() {
        match ch {
            '\\' => {
                let _ = writer.write_str("\\\\");
            }
            '"' => {
                let _ = writer.write_str("\\\"");
            }
            '\n' => {
                let _ = writer.write_str("\\n");
            }
            '\r' => {
                let _ = writer.write_str("\\r");
            }
            '\t' => {
                let _ = writer.write_str("\\t");
            }
            _ => {
                let encoded = ch.encode_utf8(&mut buf);
                let _ = writer.write_str(encoded);
            }
        }
    }
    Ok(())
}

/// システムコールの内部ディスパッチ関数
///
/// Result 型を返すことで、エラーハンドリングを型安全に行う。
/// ? 演算子でエラーを早期リターンできる。
fn dispatch_inner(nr: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    match nr {
        SYS_READ => console::sys_read(arg1, arg2),
        SYS_WRITE => console::sys_write(arg1, arg2),
        SYS_CLEAR_SCREEN => console::sys_clear_screen(),
        SYS_KEY_READ => console::sys_key_read(arg1, arg2),
        SYS_CONSOLE_GRAB => console::sys_console_grab(arg1),
        SYS_PIPE => console::sys_pipe(arg1, arg2),
        SYS_SPAWN_REDIRECTED => console::sys_spawn_redirected(arg1),
        // テスト/デバッグ
        SYS_SELFTEST => misc::sys_selftest(arg1),
        // ファイルシステム
        SYS_FILE_DELETE => filesystem::sys_file_delete(arg1, arg2),
        SYS_DIR_LIST => filesystem::sys_dir_list(arg1, arg2, arg3, arg4),
        SYS_FILE_WRITE => filesystem::sys_file_write(arg1, arg2, arg3, arg4),
        SYS_DIR_CREATE => filesystem::sys_dir_create(arg1, arg2),
        SYS_DIR_REMOVE => filesystem::sys_dir_remove(arg1, arg2),
        SYS_FS_STAT => filesystem::sys_fs_stat(arg1, arg2),
        // SYS_FS_REGISTER(18) は削除済み（モノリシック化により不要）
        // システム情報
        SYS_GET_MEM_INFO => sysinfo::sys_get_mem_info(arg1, arg2),
        SYS_GET_TASK_LIST => sysinfo::sys_get_task_list(arg1, arg2),
        SYS_GET_NET_INFO => sysinfo::sys_get_net_info(arg1, arg2),
        SYS_PCI_CONFIG_READ => sysinfo::sys_pci_config_read(arg1, arg2, arg3, arg4),
        SYS_GET_FB_INFO => graphics::sys_get_fb_info(arg1, arg2),
        SYS_MOUSE_READ => graphics::sys_mouse_read(arg1, arg2),
        SYS_CLOCK_MONOTONIC => sysinfo::sys_clock_monotonic(),
        SYS_GETRANDOM => misc::sys_getrandom(arg1, arg2),
        SYS_MMAP => misc::sys_mmap(arg1, arg2, arg3, arg4),
        SYS_MUNMAP => misc::sys_munmap(arg1, arg2),
        // プロセス管理
        SYS_EXEC => process::sys_exec(arg1, arg2, arg3, arg4),
        SYS_SPAWN => process::sys_spawn(arg1, arg2, arg3, arg4),
        SYS_YIELD => process::sys_yield(),
        SYS_SLEEP => process::sys_sleep(arg1),
        SYS_WAIT => process::sys_wait(arg1, arg2),
        SYS_WAITPID => process::sys_waitpid(arg1, arg2, arg3),
        SYS_GETPID => process::sys_getpid(),
        SYS_KILL => process::sys_kill(arg1),
        SYS_GETENV => process::sys_getenv(arg1, arg2, arg3, arg4),
        SYS_SETENV => process::sys_setenv(arg1, arg2, arg3, arg4),
        SYS_LISTENV => process::sys_listenv(arg1, arg2),
        // ネットワーク（カーネル内ネットワークスタック）
        SYS_NET_DNS_LOOKUP => network::sys_net_dns_lookup(arg1, arg2, arg3),
        SYS_NET_TCP_CONNECT => network::sys_net_tcp_connect(arg1, arg2),
        SYS_NET_TCP_SEND => network::sys_net_tcp_send(arg1, arg2, arg3),
        SYS_NET_TCP_RECV => network::sys_net_tcp_recv(arg1, arg2, arg3, arg4),
        SYS_NET_TCP_CLOSE => network::sys_net_tcp_close(arg1),
        SYS_NET_SEND_FRAME => network::sys_net_send_frame(arg1, arg2),
        SYS_NET_RECV_FRAME => network::sys_net_recv_frame(arg1, arg2, arg3),
        SYS_NET_GET_MAC => network::sys_net_get_mac(arg1, arg2),
        SYS_NET_TCP_LISTEN => network::sys_net_tcp_listen(arg1),
        SYS_NET_TCP_ACCEPT => network::sys_net_tcp_accept(arg1, arg2),
        SYS_NET_UDP_BIND => network::sys_net_udp_bind(arg1),
        SYS_NET_UDP_SEND_TO => network::sys_net_udp_send_to(arg1),
        SYS_NET_UDP_RECV_FROM => network::sys_net_udp_recv_from(arg1),
        SYS_NET_UDP_CLOSE => network::sys_net_udp_close(arg1),
        SYS_NET_PING6 => network::sys_net_ping6(arg1, arg2, arg3),
        // ハンドル
        SYS_OPEN => handle::sys_open(arg1, arg2, arg3, arg4),
        SYS_HANDLE_READ => handle::sys_handle_read(arg1, arg2, arg3),
        SYS_HANDLE_WRITE => handle::sys_handle_write(arg1, arg2, arg3),
        SYS_HANDLE_CLOSE => handle::sys_handle_close(arg1),
        SYS_OPENAT => handle::sys_openat(arg1, arg2, arg3, arg4),
        SYS_RESTRICT_RIGHTS => handle::sys_restrict_rights(arg1, arg2, arg3),
        SYS_HANDLE_ENUM => handle::sys_handle_enum(arg1, arg2, arg3),
        SYS_HANDLE_STAT => handle::sys_handle_stat(arg1, arg2),
        SYS_HANDLE_SEEK => handle::sys_handle_seek(arg1, arg2, arg3),
        // ハンドル操作拡張
        SYS_HANDLE_CREATE_FILE => handle::sys_handle_create_file(arg1, arg2, arg3, arg4),
        SYS_HANDLE_UNLINK => handle::sys_handle_unlink(arg1, arg2, arg3),
        SYS_HANDLE_MKDIR => handle::sys_handle_mkdir(arg1, arg2, arg3),
        // ブロックデバイス
        SYS_BLOCK_READ => ipc::sys_block_read(arg1, arg2, arg3, arg4),
        SYS_BLOCK_WRITE => ipc::sys_block_write(arg1, arg2, arg3, arg4),
        // IPC
        SYS_IPC_SEND => ipc::sys_ipc_send(arg1, arg2, arg3),
        SYS_IPC_RECV => ipc::sys_ipc_recv(arg1, arg2, arg3, arg4),
        SYS_IPC_RECV_FROM => ipc::sys_ipc_recv_from(arg1, arg2, arg3, arg4),
        SYS_IPC_CANCEL => ipc::sys_ipc_cancel(arg1),
        SYS_IPC_SEND_HANDLE => ipc::sys_ipc_send_handle(arg1, arg2, arg3, arg4),
        SYS_IPC_RECV_HANDLE => ipc::sys_ipc_recv_handle(arg1, arg2, arg3, arg4),
        // サウンド
        SYS_SOUND_PLAY => misc::sys_sound_play(arg1, arg2),
        // スレッド
        SYS_THREAD_CREATE => misc::sys_thread_create(arg1, arg2, arg3),
        SYS_THREAD_EXIT => misc::sys_thread_exit(arg1),
        SYS_THREAD_JOIN => misc::sys_thread_join(arg1, arg2),
        // Futex
        SYS_FUTEX => misc::sys_futex(arg1, arg2, arg3, arg4),
        // 時刻
        SYS_CLOCK_REALTIME => sysinfo::sys_clock_realtime(),
        // システム制御
        SYS_DRAW_PIXEL => graphics::sys_draw_pixel(arg1, arg2, arg3),
        SYS_DRAW_RECT => graphics::sys_draw_rect(arg1, arg2, arg3, arg4),
        SYS_DRAW_LINE => graphics::sys_draw_line(arg1, arg2, arg3),
        SYS_DRAW_BLIT => graphics::sys_draw_blit(arg1, arg2, arg3, arg4),
        SYS_DRAW_TEXT => graphics::sys_draw_text(arg1, arg2, arg3, arg4),
        SYS_HALT => misc::sys_halt(),
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
