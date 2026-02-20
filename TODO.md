# SABOS TODO リスト

セキュリティと型安全性を重視した、夢の自作 OS への道のり。

## 完了: ネットワーク処理のユーザー空間移行 ✓

カーネル内のネットワークスタック（ARP/IP/UDP/TCP/DNS）を削除し、すべて netd に一本化した。
カーネルは raw フレーム送受信（SYS_NET_SEND_FRAME / SYS_NET_RECV_FRAME）だけを提供し、
プロトコル処理はすべてユーザー空間の netd が担当する。レース条件も構造的に解消済み。

### Step 1: カーネル DNS テストを netd IPC 経由に移行
- [x] `test_network_dns` を netd IPC 経由（`test_network_netd_dns` と同じパス）に書き換え
- [x] カーネルの `dns_lookup()` を selftest から除去

### Step 2: カーネル内ネットワークスタックの利用箇所を洗い出し
- [x] `net.rs` の公開 API（`dns_lookup`, `send_udp_packet`, `poll_and_handle` 等）の呼び出し元を特定
- [x] カーネルシェルの `dns` / `http` / `netpoll` コマンドを削除（ユーザーシェルに統合済み）

### Step 3: カーネル内ネットワークスタックの段階的削除
- [x] `net.rs`（1616行）をカーネルから完全に除去
- [x] DNS/TCP 系 syscall（SYS_DNS_LOOKUP, SYS_TCP_CONNECT/SEND/RECV/CLOSE）を削除
- [x] IP 設定定数を `net_config.rs` に分離（SYS_GET_NET_INFO / ip コマンド用）
- [x] sabos-syscall から削除した定数を除去
- [x] syscall-list.md を更新

### Step 4: 受信キューの一元管理
- [x] virtio-net の受信パケットはすべて netd が受け取る設計に統一
  - net.rs 削除により、カーネルの `poll_and_handle()` が消滅
  - `receive_packet()` を呼ぶのは `sys_net_recv_frame` (netd 用) のみ
- [x] カーネルは `SYS_NET_RECV_FRAME` で netd にフレームを渡すだけ
- [x] レース条件が構造的に発生しないことを確認

## 短期目標（そろそろやりたい）

### HELLOSTD.ELF の残バグ修正
- [x] ~~netd との IPC タイムアウト問題~~ → IPC recv ループ修正で対応済み
  - 原因: recv() が1回の sleep-wake サイクルで終了し、タイマー起床時に即 Timeout を返していた
  - 修正: recv/recv_typed/recv_with_handle をタイムアウトまでループする方式に変更
  - 注: HELLOSTD.ELF の net テストはまだフレーキー（QEMU ネットワークのタイミング問題の可能性）
- [x] ~~HELLOSTD.ELF 後のカーネルパニック（INVALID OPCODE #UD）~~ → 修正済み
  - 原因: SAVED_RSP/SAVED_RBP がグローバル変数で、子プロセス/スレッドが上書きしていた
  - 修正: タスクごとにバックアップを取り、コンテキストスイッチ時に退避・復帰

### AC97 音声出力の実装
- [x] AC97 ドライバで実際に音を鳴らす
  - PCI デバイス検出・ミキサー初期化・BDL 設定・PCM バッファ書き込み・再生開始を実装済み
  - `play_tone` コマンド（正弦波生成）+ `beep` コマンド（短いビープ音）が動作

### ネットワーク selftest の安定化
- [x] ~~network_dns フレーキーテスト~~ → リトライ + タイマーベースタイムアウトで修正済み
  - 根本原因: netd とカーネルの `poll_and_handle()` が受信キューを取り合うレース条件
  - 暫定修正: DNS クエリを最大 3 回リトライ（最優先タスクで根本解決予定）
- [x] ~~net selftest の net_ipv6_ping を安定化~~
  - QEMU SLIRP が ICMPv6 Echo Reply を返さないため外部 ping テストは不安定
  - カーネル selftest に `ipv6_stack` テストを追加（偽パケット注入で ICMPv6 処理を検証）
  - selftest_net から `net_ipv6_ping` を削除
- [x] HELLOSTD.ELF の net テストのフレーキーさを改善
  - UDP recv タイムアウトを 5秒→10秒に延長（SLIRP 遅延対策）
  - テストスクリプトで net::tcp_parse OK を検証（通信不要テスト）
  - DNS/UDP の通信結果はスクリプト検証対象外（SLIRP 依存で flaky）

### selftest ハング問題の修正 ✓
- [x] selftest が `framebuffer_info` または `handle_open` テスト付近でハングする問題を修正
  - 原因: `without_interrupts` 内で `WRITER.lock()` を取得 → 他タスクがロック保持中だとデッドロック
  - 修正: `framebuffer.rs` の全 10 関数 + `serial.rs` の `_serial_print` から `without_interrupts` を除去
  - 割り込みハンドラは WRITER/SERIAL1 に触れないため `without_interrupts` は不要だった

### IPC 基盤の改善
- [x] タイムアウト/キャンセルの改善
  - recv を Sleep/Wake 方式に改修（ポーリング廃止、CPU 浪費削減）
  - SYS_IPC_CANCEL(92) で recv 待ちをキャンセル可能に
  - recv をタイムアウトまでループするよう修正（1回 sleep-wake で諦めていたバグを修正）
- [x] IPC 経由の Capability 委譲の実装
  - SYS_IPC_SEND_HANDLE(93) / SYS_IPC_RECV_HANDLE(94) を追加
  - duplicate_handle でハンドルを複製して送信
- [x] IPC パフォーマンスの計測と最適化
  - ipc_bench コマンドで TSC サイクル計測

### Capability ベースの実装を日常運用に
- [x] ユーザーシェルをハンドル API に移行
  - write/rm/mkdir/rmdir を handle_create_file/handle_unlink/handle_mkdir に移行
  - SYS_HANDLE_CREATE_FILE(140), SYS_HANDLE_UNLINK(141), SYS_HANDLE_MKDIR(142) を追加
  - cwd_handle がフルアクセス権限（CREATE/DELETE 含む）を持つように変更
  - openat が親ハンドルの権限を引き継ぐように修正
- [x] すべてのファイル操作を VFS 経由に統一
  - VFS マネージャ（マウントテーブル）を導入、"/" に Fat32、"/proc" に ProcFs をマウント
  - syscall.rs / handle.rs / shell.rs の Fat32 直接呼び出しと /proc 分岐を除去
  - procfs も VFS の子として自動ルーティング

### ユーザーランド開発サイクルの高速化

ゴール: ユーザーランドバイナリの変更 → テスト実行 → 結果取得を、ディスクイメージの再作成なしで高速に回す。

現状の問題:
- ユーザーランドを変更するたびに `dd + mkfs.fat + 全ファイル mcopy` で disk.img を毎回ゼロから再作成
- テスト実行は QEMU + sendkey で手動的
- virtio-blk は 1 台しかサポートしておらず、ホストのディレクトリを直接マウントできない

#### Step 1: 複数 virtio-blk デバイスのサポート（カーネル） ✓
- [x] `pci::find_all_virtio_blk()` で全 virtio-blk デバイスを返すように拡張
- [x] `VIRTIO_BLK` → `VIRTIO_BLKS: Vec<VirtioBlk>` に拡張
- [x] `KernelBlockDevice` に `dev_index` フィールドを追加
  - `KernelBlockDevice { dev_index: 0 }` = disk.img、`1` = hostfs.img

#### Step 2: ホストディレクトリの VFS マウント（カーネル + QEMU） ✓
- [x] QEMU 起動オプションに 2 台目の virtio-blk を追加（hostfs.img）
- [x] `vfs::init()` で 2 台目のデバイスを `/host` にマウント
- [x] `make hostfs-update` で `mcopy -o` によるインクリメンタル更新
  - ゲスト内から `run /host/SHELL.ELF` でホスト側のバイナリを直接実行可能
  - QEMU 再起動は必要だが、disk.img の再作成は不要

#### Step 3: テストランナーの改善（スクリプト） ✓
- [x] 特定バイナリを指定してテスト実行するスクリプト
  - `make test-bin BIN=shell` → QEMU 起動 → `/host/SHELL.ELF` 実行 → 結果取得
  - `scripts/run-test-bin.sh` を新規作成、disk.img 再作成なしで高速テスト
- [x] テスト結果の構造化出力
  - selftest が JSON サマリー行を出力: `=== SELFTEST JSON {...} ===`
  - run-selftest.sh が python3 で JSON パースし正確に判定
  - selftest_net の "NET SELFTEST END:...PASSED" 誤マッチバグも修正

#### Step 4: ISA debug exit の活用（カーネル + QEMU） ✓
- [x] QEMU に `-device isa-debug-exit,iobase=0xf4,iosize=0x04` を追加
  - 全 QEMU 起動オプション（QEMU_COMMON, run-gui, スクリプト）に設定
- [x] `kernel/src/qemu.rs` に `debug_exit(code)` 関数を実装
  - `exit_qemu [code]` シェルコマンド + `selftest --exit` フラグで利用
  - SYS_SELFTEST syscall に auto_exit 引数を追加（ユーザーシェルから --exit を渡せる）
- [x] run-selftest.sh を QEMU exit code ベースの判定に改善
  - selftest --exit → QEMU 自動終了 → exit code 1=成功, 3=失敗
  - kill 不要のクリーンシャットダウン

#### Step 5: virtio-9p ドライバ（QEMU 再起動不要化） ✓
- [x] 9P2000.L プロトコルの実装（読み取り専用）
  - QEMU の `-virtfs local,path=./user/target,mount_tag=hostfs9p,security_model=none`
  - ゲストがホストの `./user/target` をリアルタイムでアクセス（QEMU 再起動不要）
  - VFS に `/9p` として 9P ファイルシステムをマウント
  - cargo build → 即座にゲストから `run /9p/x86_64-unknown-none/debug/shell` で最新バイナリを実行可能
  - 8 種の 9P 操作: version, attach, walk, lopen, read, readdir, getattr, clunk
  - selftest に `9p_read` テストを追加

## 中期目標（いつかやりたい）

### ファイルシステム
- [x] VFS (Virtual File System) 層の拡充（マウントテーブル実装済み）
- [x] FAT32 ドライバをカーネル内で運用（モノリシック化）
  - fat32d（ユーザー空間デーモン）+ Fat32IpcFs は削除済み
  - IPC オーバーヘッドが消えて高速化、コード簡素化

### ネットワークスタックの改善
- [x] ~~カーネル内ネットワークスタックの削除~~ → net.rs 削除済み、netd が唯一のプロトコル処理実装
- [ ] IPv6 Phase 2: TCP/UDP over IPv6
  - IPC プロトコルのアドレスフィールドを 16 バイトに拡張
  - IpAddr enum（V4/V6）導入 + netstack 全体のアドレス抽象化
  - DNS AAAA レコード対応
  - PAL の IPv6 対応
- [ ] DHCP クライアント（IP アドレスの自動取得）
- [ ] NTP クライアント（時刻同期）

### セキュリティ強化
- [ ] ASLR (Address Space Layout Randomization)
  - プロセスのメモリ配置をランダム化
  - 攻撃者がアドレスを推測できなくする
- [ ] スタックカナリア
  - バッファオーバーフローを検出
- [ ] KASLR (Kernel ASLR)
  - カーネル自体のアドレスもランダム化

### Capability ベース OS への進化
- [ ] 全リソースを Capability で管理
  - ファイル、ネットワーク、デバイス、メモリ全てが権限トークン
  - 「何でもできる root」を廃止
- [ ] 最小権限の原則を強制
  - プロセスは必要な権限だけを持つ
  - 権限エスカレーション攻撃を構造的に防ぐ
- [ ] Capability の型安全な受け渡し
  - IPC で権限を安全に委譲

### メモリアロケータ Phase 4-5

Phase 1-3 完了済み（バディアロケータ + スラブアロケータ）。

**Phase 4: ユーザー空間の動的メモリ改善**
- [ ] VMA (Virtual Memory Area) 管理
  - プロセスのアドレス空間を VMA リストで管理
  - mmap/munmap の管理を構造化

**Phase 5: 発展的機能**
- [ ] Demand Paging（ページを必要になるまで確保しない）
- [ ] メモリマップドファイル (mmap)
- [ ] Huge Pages サポート（2MB/1GB ページで TLB ミスを削減）
- [ ] OOM Killer（メモリ不足時のプロセス選択終了）

### プロセス管理の洗練
- [x] wait() が終了タスク ID も返せるようにする（waitpid 的な機能）
  - SYS_WAITPID(7) を追加: 戻り値で child_task_id を返し、exit_code はポインタ経由
  - WNOHANG フラグでノンブロッキング待ちも可能
- [ ] サービス監視のポリシー（再起動回数/バックオフ）
- [x] パイプ（stdin/stdout リダイレクト）
  - カーネルパイプ + SYS_SPAWN_REDIRECTED で外部コマンドのパイプ対応

### std ライブラリの改善
- [ ] thread_local を `thread_local_key` モードに切り替え
  - 現在の `no_threads` モード (Cell ベース) では `std::thread::current()` がスレッド間で正しく動かない
- [ ] PAL net の IPv6 対応（IPv6 Phase 2 の一部）
- [x] `std::process::Command` のパイプ対応
  - SYS_PIPE + SYS_SPAWN_REDIRECTED で stdout/stdin パイプをサポート
  - output() で子プロセスの stdout をキャプチャ可能に

### 形式検証への挑戦
- [ ] カーネルの重要部分を Kani/Prusti で検証
  - メモリ管理の安全性
  - スケジューラの公平性
  - IPC のデッドロックフリー
- [ ] 契約プログラミング
  - 事前条件・事後条件・不変条件を明示

### Fork-less 設計（設計方針）
SABOS は意図的に fork() を提供しない:
- **spawn ベースのプロセス生成**: 新プロセスは白紙状態から始まる
- **明示的な権限委譲**: 親から子への Capability は明示的に渡す
- **セキュリティ**: 親の状態が暗黙的に漏れない
- **シンプル**: CoW の複雑さを回避

## 長期目標（夢）

### GUI サブシステムの拡充
- [ ] ウィンドウマネージャの改善（リサイズ、重なり処理）
- [ ] 簡単な GUI ツールキット（ボタン、テキスト入力）
- [ ] 画像ビューア
- [ ] 起動時のアニメーション

### ネットワーク機能拡張
- [ ] HTTPS (TLS 1.3) — 暗号化通信 + 証明書検証
- [ ] SSH サーバー/クライアント — セキュアなリモートアクセス

### ファイルシステム拡張
- [ ] ext2/ext4 サポート
- [ ] ジャーナリング（クラッシュ時のデータ保護）
- [ ] 暗号化ファイルシステム
- [ ] FUSE 的なユーザー空間 FS インターフェース

### ハードウェアサポート拡張
- [ ] AHCI (SATA) ドライバ — 実機の SSD/HDD にアクセス
- [ ] NVMe ドライバ — 高速 SSD
- [ ] USB 3.0 (xHCI) — キーボード、マウス、ストレージ
- [ ] Intel HD Audio — 音を鳴らす
- [ ] GPU ドライバ（基本的な 2D アクセラレーション）

### マルチコア対応
- [ ] SMP (Symmetric Multi-Processing) — 複数 CPU コアを活用
- [ ] Per-CPU データ構造
- [ ] ロックフリーデータ構造
- [ ] コア間 IPI (Inter-Processor Interrupt)

### セルフホスティング
- [ ] SABOS 上で Rust コンパイラを動かす
- [ ] SABOS 上で SABOS をビルドする
- [ ] 自分自身をコンパイルできる OS になる

## お楽しみ機能

### ゲーム
- [x] スネークゲーム
- [ ] マインスイーパー（GUI ウィンドウ + マウス操作）
- [ ] Doom（移植）
- [ ] ネットワーク対戦ゲーム — Telnet 経由で 2 人対戦テトリス
- [ ] シンプルな 2D ゲームエンジン

### デモ・ビジュアル
- [x] マンデルブロ集合レンダラー — フレームバッファにフラクタル描画、ズーム操作
- [x] ライフゲーム（Conway's Game of Life）— セルオートマトンの美しいパターン
- [ ] プラズマエフェクト — デモシーン的なリアルタイムエフェクト
- [ ] スクリーンセーバー — 一定時間操作なしで起動（星空 or Matrix rain）
- [ ] 3D ワイヤーフレーム回転（ソフトウェアレンダリング）
- [ ] レイトレーサー — ソフトウェアレイトレーシングで球体を描画
- [ ] 起動時スプラッシュスクリーン — SABOS ロゴをかっこよく表示

### サウンド
- [ ] WAV 再生 — AC97 で実際に音声ファイルを再生
- [ ] ビープ音楽シーケンサー — MML 的な記法でメロディを演奏
- [ ] 起動音 — OS 起動完了時にジングルを鳴らす

### 言語処理系
- [ ] Brainfuck インタプリタ — 自作 OS 上で動く最小限の言語
- [ ] Forth インタプリタ — スタックベース言語、OS 開発と相性抜群
- [ ] Lisp インタプリタ — S 式パーサー + eval/apply で REPL が動く
- [ ] シェルスクリプト — if/for/変数/パイプをサポートする SABOS 専用スクリプト言語

### ネットワーク応用
- [ ] IRC クライアント — テキストベースのリアルタイムチャット
- [ ] Gopher クライアント — レトロなインターネットプロトコル
- [ ] Web ダッシュボード — httpd 拡張でシステム情報を HTML 表示
- [ ] curl 的コマンド — HTTP GET/POST をコマンドラインから

### 開発者ツール
- [ ] カーネルデバッガ
- [ ] システムモニター（top の改善版、リアルタイムグラフ付き）
- [ ] ファイルマネージャ（GUI ウィンドウ、ダブルクリックで実行）
- [ ] hexdump コマンド — バイナリファイルの 16 進ダンプ表示
- [ ] スプレッドシート — 簡易表計算（A1 セル参照 + 四則演算）

## 研究的な挑戦

### 言語統合
- [ ] コンパイル時に権限チェック
- [ ] システムコールの型を自動生成
- [ ] WASM ランタイム — WebAssembly を SABOS 上で実行

### 新しいセキュリティモデル
- [ ] 情報フロー制御 — 機密データがどこに流れるか追跡
- [ ] サンドボックス — 信頼できないコードを隔離実行
- [ ] Secure Enclave 的な機能 — 機密処理を分離

### 分散システム
- [ ] 複数マシンでの透過的なプロセス移動
- [ ] 分散ファイルシステム
- [ ] クラスタ上での並列計算

---

## 完了した項目

### ブート・基盤
- [x] UEFI ブート
- [x] フレームバッファ描画（1280x800, BGR）
- [x] キーボード入力（PS/2）
- [x] マウスドライバ（PS/2, IRQ12）
- [x] GDT / IDT / PIC 初期化
- [x] ページング・仮想メモリ
- [x] W^X (Write XOR Execute) — ELF セグメントのパーミッションに基づくページ保護

### メモリ管理
- [x] ヒープアロケータ（16 MiB カーネルヒープ）
- [x] バディアロケータ（物理フレーム層, Phase 2）
- [x] スラブアロケータ（カーネルヒープ層, Phase 3, O(1) alloc/dealloc）
- [x] SYS_MMAP / SYS_MUNMAP（匿名ページマッピング）
- [x] フレームアロケータの二重解放検出・二分探索高速化 (Phase 1)

### プロセス・スケジューラ
- [x] 協調的マルチタスク
- [x] プリエンプティブマルチタスク（PIT タイマー）
- [x] ユーザーモード (Ring 3)
- [x] プロセス分離（独立アドレス空間）
- [x] ELF ローダー
- [x] init / supervisor（ユーザー空間サービス管理）
- [x] SYS_EXEC / SYS_SPAWN / SYS_WAIT / SYS_KILL でのプロセス制御
- [x] SYS_THREAD_CREATE / THREAD_EXIT / THREAD_JOIN（スレッド管理）

### ファイルシステム
- [x] virtio-blk ドライバ
- [x] FAT32 ファイルシステム（読み書き + LFN + サブディレクトリ）
- [x] VFS 基盤 + procfs（JSON 出力）
- [x] ハンドルベース API (SYS_OPEN / READ / WRITE / CLOSE / SEEK / STAT)
- [x] virtio-9p ドライバ（9P2000.L, 読み取り専用, ホストディレクトリ共有）

### ネットワーク
- [x] virtio-net ドライバ
- [x] ARP / ICMP (ping)
- [x] UDP / DNS クライアント
- [x] TCP クライアント / HTTP GET
- [x] UdpSocket（bind / send_to / recv_from）
- [x] ICMPv6 Echo + NDP（IPv6 Phase 1, ping6 動作）
- [x] HTTP サーバー (httpd)
- [x] Telnet サーバー (telnetd)
- [x] netd（ユーザー空間ネットワークデーモン）

### IPC・セキュリティ
- [x] 型安全 IPC（TypeId による型一致保証, SYS_IPC_SEND / RECV）
- [x] Capability ベースの VFS / ハンドル API (open / openat / restrict_rights)
- [x] syscall 番号一元管理 (libs/sabos-syscall + CI 検証スクリプト)

### Rust std ライブラリ対応
- [x] カスタムターゲット `x86_64-sabos.json` + `-Zbuild-std`
- [x] PAL 実装完了: pal, alloc, stdio, random, args, env, fs, net, os, thread, time, process, sync
- [x] `std::env::args()` — argc/argv のレジスタ渡し + PAL 保存
- [x] 外部クレート対応 — `serde` + `serde_json` が動作
- [x] CMOS RTC ドライバ + `std::time::SystemTime`

### ユーザー空間アプリケーション
- [x] ユーザーシェル（42 コマンド）
- [x] GUI フレームワーク + ウィンドウシステム
- [x] テトリス
- [x] ライフゲーム（Conway's Game of Life）
- [x] 計算機 (calc)
- [x] メモ帳 (pad)
- [x] テキストエディタ (ed)
- [x] ターミナルエミュレータ (term)
- [x] AC97 ドライバ（デバイス検出 + ミキサー初期化）

### テスト・CI
- [x] 自動テストフレームワーク (selftest, 50 テスト項目)
- [x] CI での自動操作（sendkey による再現テスト）
- [x] ネットワーク selftest (selftest_net)
- [x] HELLOSTD.ELF による std E2E テスト
- [x] sysroot パッチ変更の自動検出 + cargo clean

---

*「完璧を目指すより、まず動くものを作る。動いたら少しずつ良くする。」*

*楽しんで作ろう！*
