# procfs JSON 仕様（ドラフト）

この文書は SABOS の procfs が返す JSON の基本方針とスキーマの方向性をまとめる。

## 目的

- procfs を **読み取り専用** のシステム情報 API として定義する
- すべての procfs ファイルは **UTF-8 の JSON** を返す
- POSIX 互換ではなく、**分かりやすさ優先の独自設計** にする

## 重要な設計ルール

- procfs は **書き込み禁止**
- JSON は **トップレベルがオブジェクト**
- すべての JSON に `schema` フィールドを持たせ、将来の変更に備える
- 数値は用途に合わせて単位を明示したフィールド名にする（例: `uptime_ms`）
- 文字列は null 終端に依存しない（UTF-8 と長さで扱う）

## 共通スキーマ（トップレベル）

```
{
  "schema": "procfs-1",
  "ok": true,
  "data": { ... }
}
```

エラー時は `ok=false` にして `error` を返す。

```
{
  "schema": "procfs-1",
  "ok": false,
  "error": {
    "code": "InvalidArgument",
    "message": "field 'pid' is required"
  }
}
```

### エラーコード（暫定）

- `InvalidArgument` : 入力が不足・不正
- `NotFound` : 対象が存在しない
- `PermissionDenied` : アクセス不可
- `Internal` : 想定外エラー

## 代表的なエンドポイント案

### `/proc/uptime`

```
{
  "schema": "procfs-1",
  "ok": true,
  "data": {
    "uptime_ms": 12345678,
    "ticks": 123456
  }
}
```

### `/proc/meminfo`

```
{
  "total_frames": 12345,
  "allocated_frames": 6789,
  "free_frames": 5556,
  "free_kib": 22224,
  "processes": [
    { "id": 1, "type": "kernel", "name": "kernel", "user_frames": 0 },
    { "id": 2, "type": "user", "name": "SHELL.ELF", "user_frames": 120 }
  ]
}
```

### `/proc/tasks`

```
{
  "schema": "procfs-1",
  "ok": true,
  "data": {
    "tasks": [
      { "id": 1, "name": "kernel", "state": "running" },
      { "id": 2, "name": "shell", "state": "sleeping" }
    ]
  }
}
```

### `/proc/pci`

```
{
  "schema": "procfs-1",
  "ok": true,
  "data": {
    "devices": [
      {
        "bus": 0,
        "device": 1,
        "function": 0,
        "vendor_id": 4660,
        "device_id": 22136,
        "class": "network",
        "subclass": "ethernet"
      }
    ]
  }
}
```

## 文字列・数値の方針

- **単位はフィールド名で明示**（`_ms`, `_bytes` など）
- **列挙型は文字列**（`state`: `running`, `sleeping` など）

## 今後の TODO

- `/proc/net` の JSON 形式を詰める（socket 状態など）
- `/proc/tasks/<id>` の詳細形式
- バージョニング方針（`schema` の互換性ルール）
