# SABOS 実機対応ロードマップ

実機（物理 x86_64 マシン）での運用に耐えるために必要な機能を整理し、段階的な実装計画としてまとめる。

## 現状サマリー

SABOS は QEMU (q35 + OVMF) 上では安定動作するが、実機では以下の理由で起動すら困難と予想される:

1. **割り込みルーティングが壊れる**: Legacy 8259 PIC のみ対応。現代の HW では I/O APIC + Local APIC が必須
2. **ストレージにアクセスできない**: virtio-blk のみ対応。実機には AHCI (SATA) / NVMe が必要
3. **入力デバイスが動かない可能性**: PS/2 のみ対応。最近の PC は USB キーボード/マウスしかない
4. **ネットワークが動かない**: virtio-net のみ、ARP キャッシュなし、IP ハードコード

## Phase 0: QEMU でも必要な基盤改善（実機前の必須準備）

実機に挑む前に、QEMU 上で仕上げておくべき基盤。

### 0-1. ACPI テーブルパース

**なぜ必要**: 実機の HW 構成を知る唯一の標準的方法。UEFI が提供する ACPI テーブルから CPU トポロジ、I/O APIC アドレス、PCI 割り込みルーティングなどを取得する。

- RSDP（Root System Description Pointer）を UEFI Configuration Table から取得
- XSDT/RSDT をパースしてテーブル群を列挙
- MADT（Multiple APIC Description Table）から Local APIC / I/O APIC 情報を取得
- FADT（Fixed ACPI Description Table）からシャットダウン/リブート方法を取得

**推定工数**: 中（`acpi` crate を活用すれば大幅に短縮可能）

### 0-2. Local APIC + I/O APIC

**なぜ必要**: Legacy 8259 PIC は modern HW では仮想互換モードでしか動かない。APIC がないと割り込みルーティングが正しくできない。

- Local APIC の初期化（APIC Base MSR、Spurious Interrupt Vector）
- APIC タイマーの設定（PIT の代替、より高精度）
- I/O APIC の初期化（MADT から取得したアドレスを使用）
- IRQ リダイレクションテーブルの設定（キーボード、マウス、ストレージ等を適切なベクタに割り当て）
- 8259 PIC を無効化（IMCR レジスタ or 全マスク）

**推定工数**: 大（割り込み周りは慎重なデバッグが必要）

### 0-3. PCI 改善

**なぜ必要**: 現在 Bus 0 しかスキャンしない。実機では PCI ブリッジ配下にデバイスがある。

- PCI ブリッジ（Type 1 Header）の検出と Secondary Bus のスキャン（再帰スキャン）
- BAR サイズプローブ（write-all-ones パターン）
- MSI/MSI-X のサポート（PCIe デバイスの多くが要求する）
- PCIe ECAM（Enhanced Configuration Access Mechanism）サポート（MCFG テーブルから取得）

**推定工数**: 中

### 0-4. CPU 例外ハンドラの充実

**なぜ必要**: 実機では NMI やマシンチェック例外が実際に発生する。

- NMI ハンドラ（#2）— ハードウェアウォッチドッグ、メモリエラー
- Machine Check Exception（#18）— ハードウェアエラー検出
- Stack Fault（#12）、Alignment Check（#17）
- SIMD Floating Point Exception（#19）

**推定工数**: 小

## Phase 1: 実機ブート最小構成

Phase 0 が完了した上で、実機で起動してシェルを操作できる最小構成。

### 1-1. AHCI (SATA) ドライバ ✅ 完了

**なぜ必要**: 大半の x86_64 PC は SATA SSD/HDD を搭載。virtio-blk は実機に存在しない。

- ✅ PCI クラス 01h/06h (Mass Storage / SATA) の検出
- ✅ AHCI HBA（Host Bus Adapter）の初期化
- ✅ ポートスキャンでデバイス検出
- ✅ Read/Write コマンド（IDENTIFY, READ DMA EXT, WRITE DMA EXT）
- ✅ `BlockDevice` トレイトを実装して VFS に接続（`/ahci` にマウント）
- ✅ QEMU テスト通過（54/54 PASSED、ahci_detect + ahci_read 含む）

**実装ファイル**: `kernel/src/ahci.rs`（新規）、`kernel/src/pci.rs`（`find_ahci_controllers` 追加）、`kernel/src/fat32.rs`（`BlockBackend` enum 追加）

### 1-2. USB (xHCI) ドライバ — キーボード・マウス

**なぜ必要**: PS/2 ポートがない PC では USB HID が唯一の入力手段。

- xHCI（USB 3.0 Host Controller）の PCI デバイス検出
- DCBAA、Command Ring、Event Ring の初期化
- デバイス列挙（Port Status Change → Address Device → Configure Endpoint）
- HID クラスドライバ（Interrupt Transfer でキーコード取得）
- Boot Protocol でまず動かす（シンプル。Report Protocol は後回し）

**推定工数**: 特大（xHCI spec は複雑。段階的に進める）

### 1-3. NVMe ドライバ ✅ 完了

**なぜ必要**: 最近の PC は NVMe SSD が主流。AHCI があれば初期は不要だが、実用上は必要。

- ✅ PCI クラス 01h/08h (NVMe) の検出
- ✅ Admin Queue / I/O Queue の作成
- ✅ Identify Controller / Namespace
- ✅ Read/Write コマンド
- ✅ VFS 統合（`/nvme` マウント）
- ✅ selftest 追加（nvme_detect, nvme_read）

**推定工数**: 中（NVMe spec は AHCI より素直）

## Phase 2: ネットワーク実機対応

### 2-1. ARP キャッシュと正しい MAC 解決 ✅

**なぜ必要**: 現在はブロードキャスト MAC で全フレームを送信。実ネットワークではスイッチが正しく転送してくれない場合がある。

- ARP キャッシュテーブル（IP → MAC のマッピング）
- ARP リクエスト送信 → リプライ受信 → キャッシュ登録
- ~~キャッシュの TTL 管理~~ （TTL は未実装、最大 64 エントリの LRU 風管理）

**推定工数**: 小 → **完了**

### 2-2. DHCP クライアント ✅

**なぜ必要**: IP アドレスが 10.0.2.15 ハードコード。実ネットワークでは DHCP で取得する必要がある。

- ✅ DHCP Discover → Offer → Request → Ack の 4 ステップ
- ✅ IP アドレス、サブネットマスク、デフォルトゲートウェイ、DNS サーバーの設定
- リース更新（未実装）

**推定工数**: 中 → **完了**

### 2-3. 実 NIC ドライバ（1 種類）

**なぜ必要**: virtio-net は仮想デバイスで実機に存在しない。

候補（どれか 1 つ、ターゲット HW に応じて選択）:
- **Intel e1000e** — 広く普及、仕様公開、QEMU でもテスト可能（`-device e1000e`）
- **Realtek RTL8168** — 安価なマザーボードに多い
- **Intel I219-LM/I219-V** — 最近のビジネス PC に多い

**推奨**: e1000e から始める（QEMU で `-device e1000e` でテスト可能）

**推定工数**: 大

## Phase 3: 堅牢性・信頼性

実機で安定運用するための品質改善。

### 3-1. TCP の堅牢化
- TIME_WAIT ステート
- 再送タイマー
- アウトオブオーダーバッファリング
- 輻輳制御（TCP Reno / Cubic）
- TCP ISN のランダム化

### 3-2. 電源管理
- ACPI シャットダウン（S5 ステート）
- リブート（ACPI Reset Register or キーボードコントローラ）

### 3-3. エラーリカバリ
- ストレージ I/O エラー時のリトライ・報告
- USB デバイスの抜き差し対応（ホットプラグ）
- ネットワークリンクダウン/アップ検出

## Phase 4: マルチコア（SMP）

実機の性能を引き出すために。

- MADT から AP (Application Processor) を列挙
- AP の起動シーケンス（SIPI: Startup IPI）
- Per-CPU データ構造
- スケジューラのマルチコア対応
- スピンロック / ロックフリーキュー
- IPI（Inter-Processor Interrupt）

## 優先順位サマリー

| 優先度 | 項目 | 依存関係 |
|--------|------|----------|
| **最優先** | ACPI テーブルパース | なし |
| **最優先** | Local APIC + I/O APIC | ACPI |
| **高** | PCI 改善（マルチバス、MSI） | APIC |
| **高** | CPU 例外ハンドラ充実 | なし |
| **高** | AHCI ドライバ | PCI 改善 |
| **高** | xHCI ドライバ（USB キーボード） | PCI 改善、MSI |
| **中** | ARP キャッシュ | なし |
| **中** | DHCP クライアント | ARP キャッシュ |
| **中** | e1000e NIC ドライバ | PCI 改善、MSI |
| **低** | NVMe ドライバ | PCI 改善、MSI |
| **低** | SMP | APIC |
| **低** | TCP 堅牢化 | なし |

## テスト戦略

実機に持っていく前に QEMU で可能な限りテストする:

- **ACPI**: QEMU (q35) は完全な ACPI テーブルを提供するので、パースのテストは QEMU で可能
- **APIC**: QEMU は APIC をエミュレーションするので、Legacy PIC → APIC 切り替えは QEMU でテスト可能
- **PCI**: QEMU に複数デバイスを追加して再帰スキャンをテスト
- **AHCI**: `qemu-system-x86_64 -drive if=none,id=disk,file=test.img -device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0` でテスト可能
- **e1000e**: `qemu-system-x86_64 -device e1000e,netdev=net0 -netdev user,id=net0` でテスト可能
- **xHCI**: `qemu-system-x86_64 -device qemu-xhci -device usb-kbd -device usb-mouse` でテスト可能
- **SMP**: `qemu-system-x86_64 -smp 4` でテスト可能

つまり **Phase 0〜2 のほぼ全てを QEMU でテスト可能**。実機投入前にしっかり検証できる。

## 最初の一歩

**まず ACPI テーブルパースから始める**。理由:
1. 後続の全ての Phase が ACPI 情報に依存する
2. `acpi` crate で大部分のパースを省力化できる
3. QEMU 上で完全にテスト可能
4. 成功すれば MADT の内容をダンプして「実機の HW 構成が見える」達成感がある
