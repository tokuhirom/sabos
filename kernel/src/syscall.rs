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
// コンソール I/O (0-9)
pub const SYS_READ: u64 = 0;         // read(buf_ptr, len) — コンソールから読み取り
pub const SYS_WRITE: u64 = 1;        // write(buf_ptr, len) — 文字列をカーネルコンソールに出力
pub const SYS_CLEAR_SCREEN: u64 = 2; // clear_screen() — 画面をクリア

// ファイルシステム (10-19)
pub const SYS_FILE_READ: u64 = 10;   // file_read(path_ptr, path_len, buf_ptr, buf_len) — ファイル読み取り
pub const SYS_FILE_WRITE: u64 = 11;  // file_write(path_ptr, path_len, data_ptr, data_len) — ファイル書き込み
pub const SYS_FILE_DELETE: u64 = 12; // file_delete(path_ptr, path_len) — ファイル削除
pub const SYS_DIR_LIST: u64 = 13;    // dir_list(path_ptr, path_len, buf_ptr, buf_len) — ディレクトリ一覧

// システム情報 (20-29)
pub const SYS_GET_MEM_INFO: u64 = 20;   // get_mem_info(buf_ptr, buf_len) — メモリ情報取得
pub const SYS_GET_TASK_LIST: u64 = 21;  // get_task_list(buf_ptr, buf_len) — タスク一覧取得
pub const SYS_GET_NET_INFO: u64 = 22;   // get_net_info(buf_ptr, buf_len) — ネットワーク情報取得

// 終了 (60)
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

/// システムコールの内部ディスパッチ関数
///
/// Result 型を返すことで、エラーハンドリングを型安全に行う。
/// ? 演算子でエラーを早期リターンできる。
fn dispatch_inner(nr: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    match nr {
        SYS_READ => sys_read(arg1, arg2),
        SYS_WRITE => sys_write(arg1, arg2),
        SYS_CLEAR_SCREEN => sys_clear_screen(),
        // ファイルシステム
        SYS_FILE_READ => sys_file_read(arg1, arg2, arg3, arg4),
        SYS_FILE_WRITE => sys_file_write(arg1, arg2, arg3, arg4),
        SYS_FILE_DELETE => sys_file_delete(arg1, arg2),
        SYS_DIR_LIST => sys_dir_list(arg1, arg2, arg3, arg4),
        // システム情報
        SYS_GET_MEM_INFO => sys_get_mem_info(arg1, arg2),
        SYS_GET_TASK_LIST => sys_get_task_list(arg1, arg2),
        SYS_GET_NET_INFO => sys_get_net_info(arg1, arg2),
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

// =================================================================
// ファイルシステム関連システムコール
// =================================================================

/// SYS_FILE_READ: ファイルの内容を読み取る
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///   arg3 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg4 — バッファの長さ
///
/// 戻り値:
///   読み取ったバイト数（成功時）
///   負の値（エラー時）
fn sys_file_read(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;
    let buf_len = arg4 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // バッファを取得
    let buf_slice = UserSlice::<u8>::from_raw(arg3, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    // FAT16 からファイルを読み取る
    let fat16 = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    let data = fat16.read_file(path).map_err(|_| SyscallError::FileNotFound)?;

    // バッファにコピー
    let copy_len = core::cmp::min(data.len(), buf_len);
    buf[..copy_len].copy_from_slice(&data[..copy_len]);

    Ok(copy_len as u64)
}

/// SYS_FILE_WRITE: ファイルを作成または上書き
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///   arg3 — データのポインタ（ユーザー空間）
///   arg4 — データの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
fn sys_file_write(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;
    let data_len = arg4 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // データを取得
    let data_slice = UserSlice::<u8>::from_raw(arg3, data_len)?;
    let data = data_slice.as_slice();

    // FAT16 にファイルを書き込む
    let fat16 = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    fat16.create_file(path, data).map_err(|_| SyscallError::Other)?;

    Ok(data_len as u64)
}

/// SYS_FILE_DELETE: ファイルを削除
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）
///   arg2 — パスの長さ
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
fn sys_file_delete(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // FAT16 からファイルを削除
    let fat16 = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    fat16.delete_file(path).map_err(|_| SyscallError::FileNotFound)?;

    Ok(0)
}

/// SYS_DIR_LIST: ディレクトリの内容を一覧
///
/// 引数:
///   arg1 — パスのポインタ（ユーザー空間）"/" ならルート
///   arg2 — パスの長さ
///   arg3 — バッファのポインタ（ユーザー空間、エントリ名を改行区切りで書き込む）
///   arg4 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式:
///   ファイル名を改行区切りで出力。ディレクトリには末尾に "/" を付ける。
fn sys_dir_list(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let path_len = arg2 as usize;
    let buf_len = arg4 as usize;

    // パスを取得
    let path_slice = UserSlice::<u8>::from_raw(arg1, path_len)?;
    let path = path_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    // バッファを取得
    let buf_slice = UserSlice::<u8>::from_raw(arg3, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    // FAT16 からディレクトリ一覧を取得
    let fat16 = crate::fat16::Fat16::new().map_err(|_| SyscallError::Other)?;
    let entries = fat16.list_dir(path).map_err(|_| SyscallError::FileNotFound)?;

    // エントリ名を改行区切りでバッファに書き込む
    // ATTR_DIRECTORY = 0x10
    const ATTR_DIRECTORY: u8 = 0x10;

    let mut offset = 0;
    for entry in entries {
        let name = &entry.name;
        let is_dir = (entry.attr & ATTR_DIRECTORY) != 0;

        // 名前のバイト数 + 改行 (+ "/" for directories)
        let needed = name.len() + if is_dir { 2 } else { 1 };
        if offset + needed > buf_len {
            break;  // バッファがいっぱい
        }

        // 名前をコピー
        buf[offset..offset + name.len()].copy_from_slice(name.as_bytes());
        offset += name.len();

        // ディレクトリなら "/" を追加
        if is_dir {
            buf[offset] = b'/';
            offset += 1;
        }

        // 改行を追加
        buf[offset] = b'\n';
        offset += 1;
    }

    Ok(offset as u64)
}

// =================================================================
// システム情報関連システムコール
// =================================================================

/// SYS_GET_MEM_INFO: メモリ情報を取得
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式（テキスト）:
///   total_frames=XXXX
///   allocated_frames=XXXX
///   free_frames=XXXX
///   free_kib=XXXX
fn sys_get_mem_info(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    use crate::memory::FRAME_ALLOCATOR;
    use core::fmt::Write;

    let buf_len = arg2 as usize;
    let buf_slice = UserSlice::<u8>::from_raw(arg1, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    // メモリ情報を取得
    let fa = FRAME_ALLOCATOR.lock();
    let total = fa.total_frames();
    let allocated = fa.allocated_count();
    let free = fa.free_frames();
    drop(fa);  // ロックを早めに解放

    // テキスト形式で書き込む
    let mut writer = SliceWriter::new(buf);
    let _ = writeln!(writer, "total_frames={}", total);
    let _ = writeln!(writer, "allocated_frames={}", allocated);
    let _ = writeln!(writer, "free_frames={}", free);
    let _ = writeln!(writer, "free_kib={}", free * 4);  // 4 KiB/frame

    Ok(writer.written() as u64)
}

/// SYS_GET_TASK_LIST: タスク一覧を取得
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式（テキスト、1行目はヘッダ）:
///   id,state,type,name
///   1,Running,kernel,shell
///   2,Ready,user,HELLO.ELF
fn sys_get_task_list(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    use crate::scheduler::{self, TaskState};
    use core::fmt::Write;

    let buf_len = arg2 as usize;
    let buf_slice = UserSlice::<u8>::from_raw(arg1, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    // タスク一覧を取得
    let tasks = scheduler::task_list();

    // テキスト形式で書き込む
    let mut writer = SliceWriter::new(buf);

    // ヘッダ行
    let _ = writeln!(writer, "id,state,type,name");

    // 各タスクの情報
    for t in &tasks {
        let state_str = match t.state {
            TaskState::Ready => "Ready",
            TaskState::Running => "Running",
            TaskState::Sleeping(_) => "Sleeping",
            TaskState::Finished => "Finished",
        };
        let type_str = if t.is_user_process { "user" } else { "kernel" };
        let _ = writeln!(writer, "{},{},{},{}", t.id, state_str, type_str, t.name);
    }

    Ok(writer.written() as u64)
}

/// SYS_GET_NET_INFO: ネットワーク情報を取得
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式（テキスト）:
///   ip=X.X.X.X
///   gateway=X.X.X.X
///   dns=X.X.X.X
///   mac=XX:XX:XX:XX:XX:XX
fn sys_get_net_info(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    use core::fmt::Write;

    let buf_len = arg2 as usize;
    let buf_slice = UserSlice::<u8>::from_raw(arg1, buf_len)?;
    let buf = buf_slice.as_mut_slice();

    // ネットワーク情報を取得
    let my_ip = crate::net::MY_IP;
    let gateway = crate::net::GATEWAY_IP;
    let dns = crate::net::DNS_SERVER_IP;

    // テキスト形式で書き込む
    let mut writer = SliceWriter::new(buf);
    let _ = writeln!(writer, "ip={}.{}.{}.{}", my_ip[0], my_ip[1], my_ip[2], my_ip[3]);
    let _ = writeln!(writer, "gateway={}.{}.{}.{}", gateway[0], gateway[1], gateway[2], gateway[3]);
    let _ = writeln!(writer, "dns={}.{}.{}.{}", dns[0], dns[1], dns[2], dns[3]);

    // MAC アドレスを取得（virtio-net が初期化されていれば）
    let drv = crate::virtio_net::VIRTIO_NET.lock();
    if let Some(ref d) = *drv {
        let mac = d.mac_address;
        let _ = writeln!(writer, "mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
    } else {
        let _ = writeln!(writer, "mac=none");
    }

    Ok(writer.written() as u64)
}

// =================================================================
// ユーティリティ: スライスへの書き込み
// =================================================================

/// バイトスライスに書き込むための Write 実装
///
/// fmt::Write トレイトを実装して、write! / writeln! マクロを使えるようにする。
/// バッファがいっぱいになったら書き込みを止める（パニックしない）。
struct SliceWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> SliceWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn written(&self) -> usize {
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
