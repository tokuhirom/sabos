# FAT32 仕様メモ（SABOS 向け）

## 目的

- FAT16 実装をベースに FAT32 をフル実装する。
- no_std の汎用ライブラリとして切り出せる設計を前提にする。
- 既存の VFS と共存し、FAT16/FAT32 を切り替えて利用できるようにする。

## サポート範囲（最終目標）

- 読み取り／書き込み／削除
- サブディレクトリ対応
- LFN (Long File Name) 対応
- 8.3 形式の互換名（短縮名）の生成・利用
- FAT32 の FSInfo による空きクラスタ管理（信頼できない場合は走査）

## ディスクレイアウト概要

```
セクタ0: ブートセクタ (BPB + 拡張BPB)
予約領域: FSInfo, 予備ブートセクタ
FAT領域: FAT1, FAT2
データ領域: クラスタ配列
```

### BPB（主要フィールド）

- bytes_per_sector (u16)
- sectors_per_cluster (u8)
- reserved_sectors (u16)
- num_fats (u8)
- total_sectors_16 / total_sectors_32
- fat_size_16 / fat_size_32
- root_cluster (u32) ← FAT32 のルートディレクトリ開始クラスタ
- fsinfo_sector (u16)
- backup_boot_sector (u16)

### FSInfo

- 署名とシグネチャの検証
- free_cluster_count / next_free_cluster_hint を利用
- 破損時は FAT 走査で補正

## ディレクトリエントリ

### ショート (8.3)

- 32 バイト固定
- attr でファイル/ディレクトリ/ボリュームID を判別
- 先頭クラスタは high/low に分割（FAT32）

### LFN

- attr = 0x0F
- UTF-16 の 13 文字断片
- 連番 + チェックサムで紐付け
- 末尾 0x0000 / 0xFFFF の終端を考慮

## 操作

- open/read/write/delete
- mkdir/rmdir
- list_dir
- cluster chain の追跡

## no_std crate 切り出し方針

### fat-core（案）

- BPB/FSInfo/DirEntry/LFN のパース
- 8.3 名の生成・正規化
- クラスタチェーンの抽象

### blockdev（案）

- read_sector / write_sector trait
- カーネル・ユーザーで同じ API を使えるようにする

### vfs-common（案）

- FileSystem trait / VfsNode / VfsDirEntry
- 既存の kernel/src/vfs.rs をベースに整理

## テスト方針

- FAT32 ルート読み取り
- ファイル作成 → 読み戻し
- ディレクトリ作成 → ファイル作成 → 削除
- LFN 作成 → 列挙 → 読み取り

## 互換性

- FAT16 は存続
- FAT32 を優先マウントする設計を検討
