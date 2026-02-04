# SABOS システムコール一覧

SABOS のシステムコール番号と引数・戻り値の対応表。
ユーザー空間からは `int 0x80` を通じて呼び出される。

## ルール

- 文字列やバッファは null 終端ではなく **(ptr, len)** で渡す
- ユーザー空間ポインタは `UserPtr<T>` / `UserSlice<T>` で検証してから使う
- 失敗時は **負の値**（`SyscallError` の errno）を返す

## コンソール I/O (0-9)

- `0` `SYS_READ(buf_ptr, len) -> n`
- `1` `SYS_WRITE(buf_ptr, len) -> n`
- `2` `SYS_CLEAR_SCREEN() -> 0`

## テスト/デバッグ (10-11)

- `10` `SYS_SELFTEST() -> 0`
  - カーネル selftest を実行する（CI 用）

## ファイルシステム (12-19)

- `12` `SYS_FILE_DELETE(path_ptr, path_len) -> 0`
- `13` `SYS_DIR_LIST(path_ptr, path_len, buf_ptr, buf_len) -> n`

## システム情報 (20-29)

- `20` `SYS_GET_MEM_INFO(buf_ptr, buf_len) -> n`
- `21` `SYS_GET_TASK_LIST(buf_ptr, buf_len) -> n`
- `22` `SYS_GET_NET_INFO(buf_ptr, buf_len) -> n`
- `23` `SYS_PCI_CONFIG_READ(bus, device, function, offset, size) -> value`
- `24` `SYS_GET_FB_INFO(buf_ptr, buf_len) -> n`

## プロセス管理 (30-39)

- `30` `SYS_EXEC(path_ptr, path_len) -> 0`
- `31` `SYS_SPAWN(path_ptr, path_len) -> task_id`
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
  - WRITE 権限が必要（現在は未実装）

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

## ブロックデバイス (80-89)

- `80` `SYS_BLOCK_READ(sector, buf_ptr, len) -> n`
- `81` `SYS_BLOCK_WRITE(sector, buf_ptr, len) -> n`

## IPC (90-99)

- `90` `SYS_IPC_SEND(dest_task_id, buf_ptr, len) -> 0`
- `91` `SYS_IPC_RECV(sender_ptr, buf_ptr, buf_len, timeout_ms) -> n`

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

### その他

| コード | 名前 | 意味 |
|--------|------|------|
| -99 | OTHER | その他のエラー |
