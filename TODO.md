# SABOS TODO リスト

セキュリティと型安全性を重視した、夢の自作 OS への道のり。

## 短期目標（そろそろやりたい）

### HELLOSTD.ELF の残バグ修正
- [ ] netd との IPC タイムアウト問題
  - HELLOSTD.ELF から `std::net::TcpStream` や `UdpSocket` を使うと IPC recv がタイムアウトする
  - no_std の shell selftest_net からは同じ操作が成功する
  - 原因: HELLOSTD.ELF が前のテスト（fs, time, env）を実行した後のタイミング問題の可能性
- [x] ~~HELLOSTD.ELF 後のカーネルパニック（INVALID OPCODE #UD）~~ → 修正済み
  - 原因: SAVED_RSP/SAVED_RBP がグローバル変数で、子プロセス/スレッドが上書きしていた
  - 修正: タスクごとにバックアップを取り、コンテキストスイッチ時に退避・復帰

### AC97 音声出力の実装
- [ ] AC97 ドライバで実際に音を鳴らす
  - PCI デバイス検出・ミキサー初期化・BDL 設定は完了
  - PCM バッファへのオーディオデータ書き込みと再生開始が未実装

### ネットワーク selftest の安定化
- [ ] net selftest で DNS/TCP/UDP/IPv6 テストが CI で安定して PASS するようにする
  - 現状: net_init_netd と net_addr_types のみ PASS（2/6）
  - DNS lookup、TCP HTTP GET、UDP DNS query、IPv6 ping が FAIL
  - QEMU SLIRP のタイミング問題の可能性

### IPC 基盤の改善
- [x] タイムアウト/キャンセルの改善
  - recv を Sleep/Wake 方式に改修（ポーリング廃止、CPU 浪費削減）
  - SYS_IPC_CANCEL(92) で recv 待ちをキャンセル可能に
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
- [ ] すべてのファイル操作を VFS 経由に統一
  - procfs も VFS の子として扱う
  - JSON 出力の統一

## 中期目標（いつかやりたい）

### ファイルシステムのユーザー空間移行
- [ ] FAT32 ドライバをユーザー空間で動かす（サービス化）
- [ ] VFS (Virtual File System) 層の設計・拡充
- [ ] ファイルシステムがクラッシュしてもカーネルは生き残る
- [ ] procfs をユーザー空間サービス化

### ネットワークスタックの改善
- [ ] IPv6 Phase 2: TCP/UDP over IPv6
  - IPC プロトコルのアドレスフィールドを 16 バイトに拡張
  - IpAddr enum（V4/V6）導入 + netstack 全体のアドレス抽象化
  - DNS AAAA レコード対応
  - PAL の IPv6 対応
- [ ] DHCP クライアント（IP アドレスの自動取得）
- [ ] NTP クライアント（時刻同期）
- [ ] ネットワークドライバの分離（マイクロカーネル化の第一歩）

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
- [ ] wait() が終了タスク ID も返せるようにする（waitpid 的な機能）
- [ ] サービス監視のポリシー（再起動回数/バックオフ）
- [ ] パイプ（stdin/stdout/stderr リダイレクト）

### std ライブラリの改善
- [ ] thread_local を `thread_local_key` モードに切り替え
  - 現在の `no_threads` モード (Cell ベース) では `std::thread::current()` がスレッド間で正しく動かない
- [ ] PAL net の IPv6 対応（IPv6 Phase 2 の一部）
- [ ] `std::process::Command` のパイプ対応

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
- [x] 自動テストフレームワーク (selftest, ~42 テスト項目)
- [x] CI での自動操作（sendkey による再現テスト）
- [x] ネットワーク selftest (selftest_net)
- [x] HELLOSTD.ELF による std E2E テスト
- [x] sysroot パッチ変更の自動検出 + cargo clean

---

*「完璧を目指すより、まず動くものを作る。動いたら少しずつ良くする。」*

*楽しんで作ろう！*
