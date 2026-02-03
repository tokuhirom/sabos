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

## ファイルシステム (10-19)

- `10` `SYS_FILE_READ(path_ptr, path_len, buf_ptr, buf_len) -> n`
- `11` `SYS_FILE_WRITE(path_ptr, path_len, data_ptr, data_len) -> n`
- `12` `SYS_FILE_DELETE(path_ptr, path_len) -> 0`
- `13` `SYS_DIR_LIST(path_ptr, path_len, buf_ptr, buf_len) -> n`

## システム情報 (20-29)

- `20` `SYS_GET_MEM_INFO(buf_ptr, buf_len) -> n`
- `21` `SYS_GET_TASK_LIST(buf_ptr, buf_len) -> n`
- `22` `SYS_GET_NET_INFO(buf_ptr, buf_len) -> n`
- `23` `SYS_PCI_CONFIG_READ(bus, device, function, offset, size) -> value`

## プロセス管理 (30-39)

- `30` `SYS_EXEC(path_ptr, path_len) -> never returns`
- `31` `SYS_SPAWN(path_ptr, path_len) -> task_id`
- `32` `SYS_YIELD() -> 0`
- `33` `SYS_SLEEP(ms) -> 0`

## ネットワーク (40-49)

- `40` `SYS_DNS_LOOKUP(domain_ptr, domain_len, ip_ptr) -> 0`
- `41` `SYS_TCP_CONNECT(ip_ptr, port) -> 0`
- `42` `SYS_TCP_SEND(data_ptr, data_len) -> n`
- `43` `SYS_TCP_RECV(buf_ptr, buf_len, timeout_ms) -> n`
- `44` `SYS_TCP_CLOSE() -> 0`

## システム制御 (50-59)

- `50` `SYS_HALT() -> never returns`

## 終了 (60)

- `60` `SYS_EXIT() -> never returns`

## ファイルハンドル (70-79)

- `70` `SYS_OPEN(path_ptr, path_len, handle_ptr, rights) -> 0`
- `71` `SYS_HANDLE_READ(handle_ptr, buf_ptr, len) -> n`
- `72` `SYS_HANDLE_WRITE(handle_ptr, buf_ptr, len) -> n`
- `73` `SYS_HANDLE_CLOSE(handle_ptr) -> 0`

## ブロックデバイス (80-89)

- `80` `SYS_BLOCK_READ(sector, buf_ptr, len) -> n`
- `81` `SYS_BLOCK_WRITE(sector, buf_ptr, len) -> n`

## IPC (90-99)

- `90` `SYS_IPC_SEND(dest_task_id, buf_ptr, len) -> 0`
- `91` `SYS_IPC_RECV(sender_ptr, buf_ptr, buf_len, timeout_ms) -> n`
