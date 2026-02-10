// sabos-syscall — SABOS システムコール番号の一元定義クレート
//
// このクレートは kernel と user で共有され、syscall 番号の定義ずれ（drift）を
// コンパイル時に防止する。すべての SYS_* 定数はここで定義し、
// kernel/src/syscall.rs と user/src/syscall.rs は `pub use sabos_syscall::*;` で参照する。
//
// rust-std-sabos/ の PAL ファイルは sysroot パッチのため外部 crate に依存できない。
// PAL ファイル内の番号は scripts/check-syscall-numbers.py で検証する。
//
// ## 番号体系
//
// - コンソール I/O: 0-9
// - テスト/デバッグ: 10-11
// - ファイルシステム: 12-19
// - システム情報: 20-29
// - プロセス管理: 30-39
// - ネットワーク: 40-49
// - システム制御: 50-59
// - 終了: 60
// - ファイルハンドル: 70-79
// - ブロックデバイス: 80-89
// - IPC: 90-99
// - サウンド: 100-109
// - スレッド: 110-119
// - Futex: 120-129
// - 時刻: 130-139
// - ファイルハンドル操作拡張: 140-149

#![no_std]

// =================================================================
// コンソール I/O (0-9)
// =================================================================
pub const SYS_READ: u64 = 0;         // read(buf_ptr, len) — コンソールから読み取り
pub const SYS_WRITE: u64 = 1;        // write(buf_ptr, len) — 文字列をカーネルコンソールに出力
pub const SYS_CLEAR_SCREEN: u64 = 2; // clear_screen() — 画面をクリア
pub const SYS_KEY_READ: u64 = 3;     // key_read(buf_ptr, len) — ノンブロッキングキー読み取り
pub const SYS_CONSOLE_GRAB: u64 = 4; // console_grab(grab) — キーボードフォーカス取得/解放
pub const SYS_PIPE: u64 = 5;          // pipe(read_handle_ptr, write_handle_ptr) — パイプ作成
pub const SYS_SPAWN_REDIRECTED: u64 = 6; // spawn_redirected(args_struct_ptr) — stdio リダイレクト付きプロセス起動

// =================================================================
// テスト/デバッグ (10-11)
// =================================================================
pub const SYS_SELFTEST: u64 = 10;    // selftest() — カーネル selftest を実行

// =================================================================
// ファイルシステム (12-19) — パスベース
// =================================================================
pub const SYS_FILE_DELETE: u64 = 12; // file_delete(path_ptr, path_len) — ファイル削除
pub const SYS_DIR_LIST: u64 = 13;    // dir_list(path_ptr, path_len, buf_ptr, buf_len) — ディレクトリ一覧
pub const SYS_FILE_WRITE: u64 = 14;  // file_write(path_ptr, path_len, data_ptr, data_len) — ファイル作成/上書き
pub const SYS_DIR_CREATE: u64 = 15;  // dir_create(path_ptr, path_len) — ディレクトリ作成
pub const SYS_DIR_REMOVE: u64 = 16;  // dir_remove(path_ptr, path_len) — ディレクトリ削除
pub const SYS_FS_STAT: u64 = 17;     // fs_stat(buf_ptr, buf_len) — ファイルシステム統計情報

// =================================================================
// システム情報 (20-29)
// =================================================================
pub const SYS_GET_MEM_INFO: u64 = 20;     // get_mem_info(buf_ptr, buf_len) — メモリ情報取得
pub const SYS_GET_TASK_LIST: u64 = 21;    // get_task_list(buf_ptr, buf_len) — タスク一覧取得
pub const SYS_GET_NET_INFO: u64 = 22;     // get_net_info(buf_ptr, buf_len) — ネットワーク情報取得
pub const SYS_PCI_CONFIG_READ: u64 = 23;  // pci_config_read(bus, device, function, offset, size) — PCI Config 読み取り
pub const SYS_GET_FB_INFO: u64 = 24;      // get_fb_info(buf_ptr, buf_len) — フレームバッファ情報取得
pub const SYS_MOUSE_READ: u64 = 25;       // mouse_read(buf_ptr, buf_len) — マウス状態取得
pub const SYS_CLOCK_MONOTONIC: u64 = 26;  // clock_monotonic() — 起動からの経過ミリ秒を返す
pub const SYS_GETRANDOM: u64 = 27;        // getrandom(buf_ptr, len) — ランダムバイトを生成
pub const SYS_MMAP: u64 = 28;             // mmap(addr_hint, len, prot, flags) — 匿名ページをマッピング
pub const SYS_MUNMAP: u64 = 29;           // munmap(addr, len) — ページのマッピングを解除

// =================================================================
// プロセス管理 (30-39)
// =================================================================
pub const SYS_EXEC: u64 = 30;    // exec(path_ptr, path_len) — プログラムを同期実行
pub const SYS_SPAWN: u64 = 31;   // spawn(path_ptr, path_len) — バックグラウンドでプロセス起動
pub const SYS_YIELD: u64 = 32;   // yield() — CPU を譲る
pub const SYS_SLEEP: u64 = 33;   // sleep(ms) — 指定ミリ秒スリープ
pub const SYS_WAIT: u64 = 34;    // wait(task_id, timeout_ms) — 子プロセスの終了を待つ
pub const SYS_GETPID: u64 = 35;  // getpid() — 自分のタスク ID を取得
pub const SYS_KILL: u64 = 36;    // kill(task_id) — タスクを強制終了
pub const SYS_GETENV: u64 = 37;  // getenv(key_ptr, key_len, val_buf_ptr, val_buf_len) — 環境変数を取得
pub const SYS_SETENV: u64 = 38;  // setenv(key_ptr, key_len, val_ptr, val_len) — 環境変数を設定
pub const SYS_LISTENV: u64 = 39; // listenv(buf_ptr, buf_len) — 全環境変数を一覧取得

// =================================================================
// ネットワーク (40-49)
// =================================================================
pub const SYS_DNS_LOOKUP: u64 = 40;    // dns_lookup(domain_ptr, domain_len, ip_ptr) — DNS 解決
pub const SYS_TCP_CONNECT: u64 = 41;   // tcp_connect(ip_ptr, port) — TCP 接続
pub const SYS_TCP_SEND: u64 = 42;      // tcp_send(data_ptr, data_len) — TCP 送信
pub const SYS_TCP_RECV: u64 = 43;      // tcp_recv(buf_ptr, buf_len, timeout_ms) — TCP 受信
pub const SYS_TCP_CLOSE: u64 = 44;     // tcp_close() — TCP 切断
pub const SYS_NET_SEND_FRAME: u64 = 45; // net_send_frame(buf_ptr, len) — Ethernet フレーム送信
pub const SYS_NET_RECV_FRAME: u64 = 46; // net_recv_frame(buf_ptr, len, timeout_ms) — Ethernet フレーム受信
pub const SYS_NET_GET_MAC: u64 = 47;   // net_get_mac(buf_ptr, len) — MAC アドレス取得

// =================================================================
// システム制御 (50-59)
// =================================================================
pub const SYS_HALT: u64 = 50;        // halt() — システム停止
pub const SYS_DRAW_PIXEL: u64 = 51;  // draw_pixel(x, y, rgb) — 1ピクセル描画
pub const SYS_DRAW_RECT: u64 = 52;   // draw_rect(x, y, w_h, rgb) — 矩形描画（w/h は packed）
pub const SYS_DRAW_LINE: u64 = 53;   // draw_line(xy0, xy1, rgb) — 直線描画（x,y は packed）
pub const SYS_DRAW_BLIT: u64 = 54;   // draw_blit(x, y, w_h, buf_ptr) — 画像描画
pub const SYS_DRAW_TEXT: u64 = 55;   // draw_text(xy, fg_bg, buf_ptr, len) — 文字列描画

// =================================================================
// 終了 (60)
// =================================================================
pub const SYS_EXIT: u64 = 60;        // exit() — ユーザープログラムを終了してカーネルに戻る

// =================================================================
// ファイルハンドル (70-79) — Capability-based security
// =================================================================
pub const SYS_OPEN: u64 = 70;            // open(path_ptr, path_len, handle_ptr, rights)
pub const SYS_HANDLE_READ: u64 = 71;     // handle_read(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_WRITE: u64 = 72;    // handle_write(handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_CLOSE: u64 = 73;    // handle_close(handle_ptr)
pub const SYS_OPENAT: u64 = 74;          // openat(dir_handle_ptr, path_ptr, path_len, new_handle_ptr, rights)
pub const SYS_RESTRICT_RIGHTS: u64 = 75; // restrict_rights(handle_ptr, new_rights, new_handle_ptr)
pub const SYS_HANDLE_ENUM: u64 = 76;     // handle_enum(dir_handle_ptr, buf_ptr, len)
pub const SYS_HANDLE_STAT: u64 = 77;     // handle_stat(handle_ptr, stat_ptr) — メタデータ取得
pub const SYS_HANDLE_SEEK: u64 = 78;     // handle_seek(handle_ptr, offset, whence) — ポジション変更

// =================================================================
// ブロックデバイス (80-89)
// =================================================================
pub const SYS_BLOCK_READ: u64 = 80;   // block_read(sector, buf_ptr, len)
pub const SYS_BLOCK_WRITE: u64 = 81;  // block_write(sector, buf_ptr, len)

// =================================================================
// IPC (90-99)
// =================================================================
pub const SYS_IPC_SEND: u64 = 90;     // ipc_send(dest_task_id, buf_ptr, len)
pub const SYS_IPC_RECV: u64 = 91;     // ipc_recv(sender_ptr, buf_ptr, buf_len, timeout_ms)
pub const SYS_IPC_CANCEL: u64 = 92;   // ipc_cancel(target_task_id) — recv 待ちをキャンセル
pub const SYS_IPC_SEND_HANDLE: u64 = 93; // ipc_send_handle(dest, buf_ptr, len, handle_ptr) — ハンドル付きメッセージ送信
pub const SYS_IPC_RECV_HANDLE: u64 = 94; // ipc_recv_handle(sender_ptr, buf_ptr, buf_len, handle_out_ptr) — ハンドル付きメッセージ受信

// =================================================================
// サウンド (100-109)
// =================================================================
pub const SYS_SOUND_PLAY: u64 = 100;  // sound_play(freq_hz, duration_ms) — 正弦波ビープ音再生

// =================================================================
// スレッド (110-119)
// =================================================================
pub const SYS_THREAD_CREATE: u64 = 110; // thread_create(entry_ptr, stack_ptr, arg) -> thread_id
pub const SYS_THREAD_EXIT: u64 = 111;   // thread_exit(exit_code) — スレッド終了
pub const SYS_THREAD_JOIN: u64 = 112;   // thread_join(thread_id, timeout_ms) -> exit_code

// =================================================================
// Futex (120-129)
// =================================================================
pub const SYS_FUTEX: u64 = 120;         // futex(addr, op, val, timeout_ms) — Futex 操作

// =================================================================
// 時刻 (130-139)
// =================================================================
pub const SYS_CLOCK_REALTIME: u64 = 130; // clock_realtime() — UNIX エポックからの秒数を返す

// =================================================================
// ファイルハンドル操作拡張 (140-149)
// =================================================================
pub const SYS_HANDLE_CREATE_FILE: u64 = 140; // handle_create_file(dir_handle_ptr, name_ptr, name_len, out_handle_ptr) — ディレクトリ内にファイルを作成
pub const SYS_HANDLE_UNLINK: u64 = 141;      // handle_unlink(dir_handle_ptr, name_ptr, name_len) — ディレクトリ内のファイル/ディレクトリを削除
pub const SYS_HANDLE_MKDIR: u64 = 142;       // handle_mkdir(dir_handle_ptr, name_ptr, name_len) — ディレクトリ内にサブディレクトリを作成
