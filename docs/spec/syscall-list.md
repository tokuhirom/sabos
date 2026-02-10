# SABOS システムコール一覧

SABOS のシステムコール番号と引数・戻り値の対応表。
ユーザー空間からは `int 0x80` を通じて呼び出される。

## ルール

- 文字列やバッファは null 終端ではなく **(ptr, len)** で渡す
- ユーザー空間ポインタは `UserPtr<T>` / `UserSlice<T>` で検証してから使う
- 失敗時は **負の値**（`SyscallError` の errno）を返す

## コンソール I/O (0-9)

- `0` `SYS_READ(buf_ptr, len) -> n`
  - フォーカス対応: キーボードフォーカスが設定されている場合、フォーカス外のタスクはフォーカス解放まで待機する
  - stdin がパイプにリダイレクトされている場合はパイプから読み取り（WouldBlock 時は yield + retry）
- `1` `SYS_WRITE(buf_ptr, len) -> n`
  - stdout がパイプにリダイレクトされている場合はパイプに書き込み
- `2` `SYS_CLEAR_SCREEN() -> 0`
- `3` `SYS_KEY_READ(buf_ptr, len) -> n`
  - ノンブロッキングでキーボード入力を読み取る（`SYS_MOUSE_READ` と同パターン）
  - 入力がなければ `0` を返す
  - キーボードフォーカスを持つタスクのみが読み取れる
- `4` `SYS_CONSOLE_GRAB(grab) -> 0`
  - `grab == 1`: キーボードフォーカスを取得（他タスクの SYS_READ をブロック）
  - `grab == 0`: キーボードフォーカスを解放
  - タスク終了時に自動解放される
- `5` `SYS_PIPE(read_handle_ptr, write_handle_ptr) -> 0`
  - パイプを作成し、読み取り用と書き込み用の Handle ペアをユーザー空間に書き込む
  - read_handle で読み取り、write_handle で書き込む
  - write_handle を閉じると reader は EOF を検出する
  - reader を閉じると writer は BrokenPipe エラーになる
- `6` `SYS_SPAWN_REDIRECTED(args_struct_ptr) -> task_id`
  - stdin/stdout リダイレクト付きでプロセスを起動する
  - 構造体ベース: `SpawnRedirectArgs { path_ptr, path_len, args_ptr, args_len, stdin_handle_id, stdin_handle_token, stdout_handle_id, stdout_handle_token }`
  - handle_id が `u64::MAX` の場合はリダイレクトなし（コンソール直結）

## テスト/デバッグ (10-11)

- `10` `SYS_SELFTEST() -> 0`
  - カーネル selftest を実行する（CI 用）

## ファイルシステム (12-19)

- `12` `SYS_FILE_DELETE(path_ptr, path_len) -> 0`
- `13` `SYS_DIR_LIST(path_ptr, path_len, buf_ptr, buf_len) -> n`
- `14` `SYS_FILE_WRITE(path_ptr, path_len, data_ptr, data_len) -> 0`
  - 指定パスにファイルを作成/上書きする
  - 既にファイルが存在する場合は削除してから作成する
  - /proc 配下は書き込み禁止（ReadOnly エラー）
- `15` `SYS_DIR_CREATE(path_ptr, path_len) -> 0`
  - 指定パスにディレクトリを作成する
  - /proc 配下は書き込み禁止（ReadOnly エラー）
- `16` `SYS_DIR_REMOVE(path_ptr, path_len) -> 0`
  - 指定パスの空ディレクトリを削除する
  - /proc 配下は書き込み禁止（ReadOnly エラー）
- `17` `SYS_FS_STAT(buf_ptr, buf_len) -> n`
  - ファイルシステムの統計情報を JSON 形式でバッファに書き込む
  - 出力例: `{"fs":"fat32","total_bytes":...,"used_bytes":...,"free_bytes":...,"cluster_bytes":...,"total_clusters":...,"free_clusters":...}`

## システム情報 (20-29)

- `20` `SYS_GET_MEM_INFO(buf_ptr, buf_len) -> n`
- `21` `SYS_GET_TASK_LIST(buf_ptr, buf_len) -> n`
- `22` `SYS_GET_NET_INFO(buf_ptr, buf_len) -> n`
- `23` `SYS_PCI_CONFIG_READ(bus, device, function, offset, size) -> value`
- `24` `SYS_GET_FB_INFO(buf_ptr, buf_len) -> n`
- `25` `SYS_MOUSE_READ(buf_ptr, buf_len) -> n`
  - 更新があれば `MouseState` を書き込んでサイズを返す
  - 更新がなければ `0`
- `26` `SYS_CLOCK_MONOTONIC() -> ms`
  - 起動からの経過ミリ秒を返す（PIT ティックから変換）
  - std::time::Instant の代替として使用可能
- `27` `SYS_GETRANDOM(buf_ptr, len) -> n`
  - RDRAND 命令（ハードウェア乱数生成器）でランダムバイトを生成
  - HashMap の RandomState 等で使用される
  - エラー: -41 (RDRAND 非対応 or エントロピー枯渇)
- `28` `SYS_MMAP(addr_hint, len, prot, flags) -> addr`
  - ユーザー空間に匿名ページ（ゼロ初期化済み）をマッピング
  - `addr_hint == 0`: カーネルが空き領域を自動選択（0x4000_0000〜）
  - `addr_hint != 0`: 指定アドレスにマッピング（4KiB アライン必須）
  - `prot`: PROT_READ(0x1) | PROT_WRITE(0x2)
  - `flags`: MAP_ANONYMOUS(0x1) のみ対応
  - プロセス終了時に自動解放される
  - エラー: -10 (不正引数), -2 (アドレス範囲外), -41 (未対応フラグ), -99 (空き不足)
- `29` `SYS_MUNMAP(addr, len) -> 0`
  - mmap で確保したページのマッピングを解除
  - `addr`: 4KiB アライン必須
  - 物理フレームは即座に解放される
  - エラー: -3 (アラインメント不正), -10 (不正引数), -2 (範囲外)

## プロセス管理 (30-39)

- `30` `SYS_EXEC(path_ptr, path_len, args_ptr, args_len) -> 0`
  - プログラムを同期実行（フォアグラウンド）
  - args_ptr=0 ならパスのみを argv[0] として渡す（後方互換）
  - 引数バッファフォーマット: `[u16 len][bytes]` の繰り返し（長さプレフィックス形式）
- `31` `SYS_SPAWN(path_ptr, path_len, args_ptr, args_len) -> task_id`
  - バックグラウンドでプロセスを起動
  - args_ptr=0 ならパスのみを argv[0] として渡す（後方互換）
  - 引数バッファフォーマット: `[u16 len][bytes]` の繰り返し（長さプレフィックス形式）
- `32` `SYS_YIELD() -> 0`
- `33` `SYS_SLEEP(ms) -> 0`
- `34` `SYS_WAIT(task_id, timeout_ms) -> exit_code`
  - `task_id == 0`: 任意の子プロセスの終了を待つ
  - `task_id > 0`: 指定した子プロセスの終了を待つ
  - `timeout_ms == 0`: 無期限待ち
  - 子プロセスが既に終了していれば即座に戻る
  - エラー: -10 (子がいない), -30 (子ではない), -42 (タイムアウト)
- `35` `SYS_GETPID() -> task_id`
  - 現在のタスク ID を取得
- `36` `SYS_KILL(task_id) -> 0`
  - 指定したタスクを強制終了する
  - ユーザープロセスのリソース（ページテーブル等）も解放される
  - 自分自身の kill はエラー（SYS_EXIT を使うこと）
  - エラー: -10 (自分自身 or タスク不在), -30 (既に終了済み)
- `37` `SYS_GETENV(key_ptr, key_len, val_buf_ptr, val_buf_len) -> val_len`
  - 現在のプロセスの環境変数を取得する
  - 成功時は val_buf に値を書き込み、値の長さを返す
  - エラー: -20 (キーが存在しない), -4 (バッファ不足)
- `38` `SYS_SETENV(key_ptr, key_len, val_ptr, val_len) -> 0`
  - 現在のプロセスの環境変数を設定する
  - 既に同じキーがあれば上書き
  - 環境変数は spawn 時に子プロセスに継承される
- `39` `SYS_LISTENV(buf_ptr, buf_len) -> n`
  - 現在のプロセスの全環境変数を一覧取得する
  - バッファに "KEY=VALUE\n" の繰り返しで書き込む
  - 戻り値は書き込んだバイト数
  - エラー: -4 (バッファ不足)

## ネットワーク (40-49)

- `40` `SYS_DNS_LOOKUP(domain_ptr, domain_len, ip_ptr) -> 0`
- `41` `SYS_TCP_CONNECT(ip_ptr, port) -> 0`
- `42` `SYS_TCP_SEND(data_ptr, data_len) -> n`
- `43` `SYS_TCP_RECV(buf_ptr, buf_len, timeout_ms) -> n`
- `44` `SYS_TCP_CLOSE() -> 0`
- `45` `SYS_NET_SEND_FRAME(buf_ptr, len) -> n`
- `46` `SYS_NET_RECV_FRAME(buf_ptr, len, timeout_ms) -> n`
- `47` `SYS_NET_GET_MAC(buf_ptr, len) -> n`

## システム制御 (50-59)

- `50` `SYS_HALT() -> never returns`
- `51` `SYS_DRAW_PIXEL(x, y, rgb) -> 0`
  - `rgb`: 0xRRGGBB
- `52` `SYS_DRAW_RECT(x, y, w_h, rgb) -> 0`
  - `w_h`: 上位 32bit = width, 下位 32bit = height
  - `rgb`: 0xRRGGBB
- `53` `SYS_DRAW_LINE(xy0, xy1, rgb) -> 0`
  - `xy0`: 上位 32bit = x0, 下位 32bit = y0
  - `xy1`: 上位 32bit = x1, 下位 32bit = y1
  - `rgb`: 0xRRGGBB
- `54` `SYS_DRAW_BLIT(x, y, w_h, buf_ptr) -> 0`
  - `buf_ptr`: RGBX (4 bytes/pixel)
- `55` `SYS_DRAW_TEXT(xy, fg_bg, buf_ptr, len) -> 0`
  - `xy`: 上位 32bit = x, 下位 32bit = y
  - `fg_bg`: 上位 32bit = fg, 下位 32bit = bg（各 0xRRGGBB）

## 終了 (60)

- `60` `SYS_EXIT() -> never returns`

## ファイルハンドル (70-79)

Capability-based security を実現するためのハンドル操作。

### 権限ビット

| ビット | 名前 | 意味 |
|--------|------|------|
| 0x0001 | READ | ファイル内容の読み取り |
| 0x0002 | WRITE | ファイル内容の書き込み |
| 0x0004 | SEEK | ファイルポジションの変更 |
| 0x0008 | STAT | メタデータの取得 |
| 0x0010 | ENUM | ディレクトリ内のエントリ列挙 |
| 0x0020 | CREATE | ディレクトリ内にファイルを作成 |
| 0x0040 | DELETE | ディレクトリ内のファイルを削除 |
| 0x0080 | LOOKUP | 相対パスでファイルを開く（openat 用） |

### システムコール

- `70` `SYS_OPEN(path_ptr, path_len, handle_ptr, rights) -> 0`
  - 絶対パスでファイルを開く
  - `handle_ptr`: Handle 構造体の書き込み先
  - `rights`: 要求する権限ビット

- `71` `SYS_HANDLE_READ(handle_ptr, buf_ptr, len) -> n`
  - ハンドルからデータを読み取る
  - READ 権限が必要

- `72` `SYS_HANDLE_WRITE(handle_ptr, buf_ptr, len) -> n`
  - ハンドルにデータを書き込む
  - WRITE 権限が必要
  - インメモリバッファに書き込み、close() 時に FAT32 に書き戻す（write-back 方式）

- `73` `SYS_HANDLE_CLOSE(handle_ptr) -> 0`
  - ハンドルを閉じる

- `74` `SYS_OPENAT(dir_handle_ptr, path_ptr, path_len, new_handle_ptr, rights) -> 0`
  - ディレクトリハンドルからの相対パスでファイルを開く
  - **セキュリティ**: Capability-based security の核心
    - `dir_handle` に LOOKUP 権限が必要
    - `path` が "/" で始まっていたらエラー（絶対パス禁止）
    - `path` に ".." が含まれていたらエラー（パストラバーサル防止）
    - 新しいハンドルの権限 = `requested_rights & dir_handle.rights`

- `75` `SYS_RESTRICT_RIGHTS(handle_ptr, new_rights, new_handle_ptr) -> 0`
  - ハンドルの権限を縮小して新しいハンドルを作成
  - **セキュリティ**: 権限は縮小のみ可能、拡大はエラー

- `76` `SYS_HANDLE_ENUM(dir_handle_ptr, buf_ptr, len) -> n`
  - ディレクトリハンドルの内容を一覧
  - ENUM 権限が必要

- `77` `SYS_HANDLE_STAT(handle_ptr, stat_ptr) -> 0`
  - ハンドルのメタデータを取得する
  - STAT 権限が必要
  - `stat_ptr`: HandleStat 構造体の書き込み先
  - HandleStat: `{ size: u64, kind: u64 (0=File, 1=Directory), rights: u64 }`

- `78` `SYS_HANDLE_SEEK(handle_ptr, offset, whence) -> new_pos`
  - ファイルポジションを変更する
  - SEEK 権限が必要
  - `offset`: i64（SEEK_CUR/SEEK_END で負の値あり）
  - `whence`: 0=SEEK_SET（先頭から）, 1=SEEK_CUR（現在位置から）, 2=SEEK_END（末尾から）
  - 範囲外は 0 〜 ファイルサイズにクランプ

## ファイルハンドル操作拡張 (140-149)

ディレクトリハンドルの CREATE/DELETE 権限を使って、ファイルの作成・削除・ディレクトリ作成を行う。
パスベース API（SYS_FILE_WRITE 等）のハンドルベース代替。

- `140` `SYS_HANDLE_CREATE_FILE(dir_handle_ptr, name_ptr, name_len, out_handle_ptr) -> 0`
  - ディレクトリハンドル内にファイルを作成し、RW 権限付きハンドルを返す
  - CREATE 権限が必要
  - ファイル名に "/" や ".." を含んではいけない
  - 既存ファイルがあれば上書き（削除→作成）
  - /proc 配下は書き込み禁止（ReadOnly エラー）

- `141` `SYS_HANDLE_UNLINK(dir_handle_ptr, name_ptr, name_len) -> 0`
  - ディレクトリハンドル内のファイルまたは空ディレクトリを削除
  - DELETE 権限が必要
  - まずファイルとして削除を試み、失敗したらディレクトリとして削除
  - /proc 配下は書き込み禁止（ReadOnly エラー）

- `142` `SYS_HANDLE_MKDIR(dir_handle_ptr, name_ptr, name_len) -> 0`
  - ディレクトリハンドル内にサブディレクトリを作成
  - CREATE 権限が必要
  - /proc 配下は書き込み禁止（ReadOnly エラー）

## ブロックデバイス (80-89)

- `80` `SYS_BLOCK_READ(sector, buf_ptr, len) -> n`
- `81` `SYS_BLOCK_WRITE(sector, buf_ptr, len) -> n`

## IPC (90-99)

recv は Sleep/Wake 方式で実装されており、ポーリングではなくタスクを Sleeping 状態にして
メッセージ到着時に wake_task で起床させる。CPU サイクルの浪費を防ぎ、レイテンシも改善する。

- `90` `SYS_IPC_SEND(dest_task_id, buf_ptr, len) -> 0`
  - メッセージを dest タスクの受信キューに追加する
  - dest が recv 待ち（Sleeping）の場合は自動的に起床させる

- `91` `SYS_IPC_RECV(sender_ptr, buf_ptr, buf_len, timeout_ms) -> n`
  - メッセージを受信する（Sleep/Wake 方式）
  - `timeout_ms == 0`: 無期限待ち
  - キャンセルされた場合は -50 (Cancelled) を返す

- `92` `SYS_IPC_CANCEL(target_task_id) -> 0`
  - 対象タスクの IPC recv 待ちをキャンセルする
  - 対象タスクが Sleeping 状態なら起床させ、recv は -50 (Cancelled) を返す
  - エラー: -10 (タスクが存在しない)

- `93` `SYS_IPC_SEND_HANDLE(dest_task_id, buf_ptr, len, handle_ptr) -> 0`
  - ハンドル（Capability）付きメッセージを送信する
  - `handle_ptr`: 送信する Handle 構造体のポインタ
  - カーネル内部でハンドルを duplicate（元ハンドルは送信元が引き続き使用可能）
  - dest が recv 待ちの場合は自動的に起床させる

- `94` `SYS_IPC_RECV_HANDLE(sender_ptr, buf_ptr, buf_len, handle_out_ptr) -> n`
  - ハンドル付きメッセージを受信する（タイムアウトなし、cancel で中断）
  - `handle_out_ptr`: 受信した Handle 構造体の書き込み先
  - キャンセルされた場合は -50 (Cancelled) を返す

## サウンド (100-109)

- `100` `SYS_SOUND_PLAY(freq_hz, duration_ms) -> 0`
  - AC97 ドライバで正弦波ビープ音を再生する
  - `freq_hz`: 周波数 (Hz)、1〜20000
  - `duration_ms`: 持続時間 (ms)、1〜10000
  - 再生が完了するまでブロックする
  - エラー: -10 (引数範囲外), -41 (AC97 未検出)

## スレッド (110-119)

同一アドレス空間内でスレッドを作成・終了・合流するためのシステムコール。
スレッドは親プロセスとページテーブル（CR3）を共有し、独立したスタックで実行される。

- `110` `SYS_THREAD_CREATE(entry_ptr, stack_ptr, arg) -> thread_id`
  - 現在のプロセス内で新しいスレッドを作成する
  - `entry_ptr`: スレッドのエントリポイント（ユーザー空間アドレス）
  - `stack_ptr`: スレッド用のユーザースタックトップ（mmap で確保済み）
  - `arg`: スレッドに渡す引数（rdi レジスタにセット）
  - スレッドは親と同じ CR3（アドレス空間）を共有する
  - エラー: -10 (カーネルタスクからの呼び出し等)

- `111` `SYS_THREAD_EXIT(exit_code) -> never returns`
  - 現在のスレッドを終了する
  - プロセスリーダー（メインスレッド）ではなく、生成されたスレッドの終了に使用
  - アドレス空間の破棄は行わない（リーダーが管理）

- `112` `SYS_THREAD_JOIN(thread_id, timeout_ms) -> exit_code`
  - 指定したスレッドの終了を待つ
  - 同じプロセスグループ内のスレッドのみ待機可能
  - `timeout_ms == 0`: 無期限待ち
  - エラー: -10 (スレッドが存在しない), -30 (別プロセスのスレッド), -42 (タイムアウト)

## Futex (120-129)

ユーザー空間同期プリミティブ（Mutex/Condvar）の基盤となる futex（Fast Userspace Mutex）。
競合がなければカーネルに入らず、競合時のみ syscall でスリープ/ウェイクする。

- `120` `SYS_FUTEX(addr, op, val, timeout_ms) -> result`
  - `addr`: ユーザー空間の AtomicU32 のアドレス
  - `op == 0` (FUTEX_WAIT): `addr` の値が `val` と一致したらスリープ
    - `timeout_ms == 0`: 無期限待ち
    - 値が不一致なら即座に -99 (OTHER) を返す
    - 起床時は 0 を返す
  - `op == 1` (FUTEX_WAKE): `addr` で待機中のタスクを最大 `val` 個起床
    - 戻り値は実際に起床したタスクの数
  - キーは (アドレス空間 ID, 仮想アドレス) で管理し、プロセス間の区別を保証
  - エラー: -10 (不正な op)

## 時刻 (130-139)

- `130` `SYS_CLOCK_REALTIME() -> epoch_secs`
  - CMOS RTC から現在時刻を読み取り、UNIX エポック（1970-01-01 00:00:00 UTC）からの秒数を返す
  - BCD → バイナリ変換、UIP フラグ確認、Gregorian 暦 → エポック秒変換を含む
  - 関連: `SYS_CLOCK_MONOTONIC(26)` は起動からの経過ミリ秒（PIT ベース）

## エラーコード

SABOS 独自のエラーコード体系。POSIX 互換は目指さない。

### ポインタ・メモリ関連 (1-9)

| コード | 名前 | 意味 |
|--------|------|------|
| -1 | NULL_POINTER | ポインタが null |
| -2 | INVALID_ADDRESS | アドレスがユーザー空間の範囲外 |
| -3 | MISALIGNED_POINTER | アラインメントが不正 |
| -4 | BUFFER_OVERFLOW | バッファがユーザー空間をオーバーフロー |

### 引数・データ形式関連 (10-19)

| コード | 名前 | 意味 |
|--------|------|------|
| -10 | INVALID_ARGUMENT | 不正な引数 |
| -11 | INVALID_UTF8 | 不正な UTF-8 文字列 |

### ファイル・ハンドル関連 (20-29)

| コード | 名前 | 意味 |
|--------|------|------|
| -20 | FILE_NOT_FOUND | ファイルが見つからない |
| -21 | INVALID_HANDLE | 不正なハンドル |
| -22 | READ_ONLY | 書き込み禁止 |

### 権限・セキュリティ関連 (30-39)

| コード | 名前 | 意味 |
|--------|------|------|
| -30 | PERMISSION_DENIED | 権限不足 |
| -31 | PATH_TRAVERSAL | パストラバーサル試行（".." を含むパス） |

### システム関連 (40-49)

| コード | 名前 | 意味 |
|--------|------|------|
| -40 | UNKNOWN_SYSCALL | 未知のシステムコール |
| -41 | NOT_SUPPORTED | 未対応の操作 |
| -42 | TIMEOUT | タイムアウト |

### IPC 関連 (50-59)

| コード | 名前 | 意味 |
|--------|------|------|
| -50 | CANCELLED | 操作がキャンセルされた（IPC recv のキャンセル等） |

### パイプ関連 (60-69)

| コード | 名前 | 意味 |
|--------|------|------|
| -60 | WOULD_BLOCK | パイプにデータがまだない（yield して再試行すべき） |
| -61 | BROKEN_PIPE | パイプの読み取り端が閉じている |

### その他

| コード | 名前 | 意味 |
|--------|------|------|
| -99 | OTHER | その他のエラー |
