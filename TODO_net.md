# SABOS ネットワークスタック TODO

ネットワークスタックの問題点を整理し、優先度順に改善していく。

## Phase 1: TCP accept 競合の解消（最優先）

httpd と telnetd が同時に listen すると TCP accept を食い合い、httpd がデフォルト無効になっている問題。

### 調査結果（2026-02-18）

`tcp_pending_accept` のポートフィルタリングは既に実装済み。根本原因は `poll_and_handle_timeout` の設計：

1. `poll_and_handle_timeout(100)` は `enable_and_hlt()` で CPU を停止してパケット到着を待つ
2. httpd と telnetd が同時に accept を呼ぶと、一方のタスクだけがパケットを処理する
3. 他方のタスクはスケジュールされた時に poll のタイムアウトが既に過ぎており、
   pending キューを確認する前にタイムアウトで抜けてしまう
4. `yield_now()` に置き換えると QEMU SLIRP のイベントループが進まなくなり、
   パケット自体が到着しなくなる

### 解決案（要検討）

- **案 A: 割り込みハンドラでパケット処理** — virtio-net 割り込みで直接 `handle_packet()` を呼ぶ。
  `tcp_accept` は pending キューを sleep/wake で待つだけにする。Mutex デッドロックに注意が必要。
- **案 B: カーネルネットワークタスク** — 専用タスクがパケットを常時処理。
  `tcp_accept` は pending キューを条件変数で待つ。
- **案 C: HLT + yield 併用** — HLT で QEMU に時間を与えつつ、フレーム処理後に
  yield で他タスクにも pending 確認の機会を与える（単純な yield_now は QEMU 非互換だった）

### TODO

- [ ] 解決案を選定して実装する
- [ ] httpd をデフォルト起動に復帰させる
- [ ] selftest で httpd + telnetd 同時起動をテスト（`test_httpd_service` は準備済み）

## Phase 2: Rust std PAL のカーネル syscall 移行

`rust-std-sabos/sys_net_connection_sabos.rs` が廃止済みの netd IPC プロトコルを参照しており、
user-std プログラムから `std::net` を使うと壊れる。カーネル直接 syscall に移行する。

- [ ] `TcpStream` を `SYS_NET_TCP_*` syscall 直接呼び出しに書き換え
- [ ] `TcpListener` を `SYS_NET_TCP_LISTEN` + `SYS_NET_TCP_ACCEPT` に書き換え
- [ ] `UdpSocket` を `SYS_NET_UDP_*` syscall 直接呼び出しに書き換え
- [ ] DNS 解決を `SYS_NET_DNS_LOOKUP` syscall に書き換え
- [ ] HELLOSTD.ELF の net テストが PASS することを確認

## Phase 3: ARP キャッシュの実装 ✅

現在すべての送信フレームで宛先 MAC にブロードキャストを使用している。
QEMU SLIRP では動くが、実ネットワークでは正しく動作しない。

- [x] ARP テーブル（IP → MAC のキャッシュ）を netstack に追加
- [x] 送信時に ARP テーブルを参照し、ミスなら ARP Request を送信
- [x] ARP Reply を受信したらテーブルを更新
- [x] ゲートウェイ MAC の解決（デフォルトルートの場合はゲートウェイの MAC を使う）
- [x] `send_tcp_packet_internal`, `send_udp_packet`, `send_icmp_echo_reply` 等からブロードキャスト MAC を除去

## Phase 4: TCP ISN / DNS ID のランダム化

セキュリティの基本。予測可能な値を使っているためスプーフィング攻撃に脆弱。

- [x] TCP 初期シーケンス番号（ISN）をランダム化（kernel_rdrand64() で RDRAND 乱数生成）
- [x] DNS クエリ ID をランダム化（kernel_rdrand64() で毎回異なる ID）
- [x] DNS ソースポートをエフェメラルポートからランダム選択（49152-65535 範囲）

## Phase 5: TCP の堅牢化

現在の TCP は最小限の実装。信頼性を高めるための改善。

- [ ] TIME_WAIT タイマーの実装（現在は即座に接続削除）
- [ ] 再送タイマーの実装（パケットロスへの対応）
- [ ] 順序外パケットのバッファリングと並べ替え
- [ ] 重複 ACK 検出と高速再送
- [ ] ウィンドウサイズの動的管理（固定 65535 からの脱却）
- [ ] 輻輳制御（スロースタート + 輻輳回避）

## Phase 6: DHCP クライアント ✅

IP アドレスの自動取得。ハードコードされた QEMU SLIRP 定数からの脱却。

- [x] DHCP Discover / Offer / Request / Ack の実装
- [x] 取得した IP / ゲートウェイ / DNS サーバーを netstack に設定
- [x] `net_config.rs` のハードコード定数を動的設定に置き換え
- [ ] リース更新タイマー

## Phase 7: IPv6 Phase 2（TCP/UDP over IPv6）

現在 ICMPv6 Echo と NDP のみ。TCP/UDP を IPv6 でも使えるようにする。

- [ ] `IpAddr` enum（V4/V6）の導入
- [ ] netstack 全体のアドレス抽象化（IPv4/IPv6 を統一的に扱う）
- [ ] TCP over IPv6
- [ ] UDP over IPv6
- [ ] DNS AAAA レコード対応
- [ ] PAL の IPv6 対応

## Phase 8: virtio-net ドライバの改善

- [ ] RX バッファ数の増加（16 → 64 以上）
- [ ] チェックサムオフロード対応
- [ ] TX ビジーウェイトの改善（割り込みベースへ）

## その他の既知の問題

- [ ] `net_ipv6_ping` selftest がフレーキー（`TODO.md:53`）
- [ ] HELLOSTD.ELF の net テストがフレーキー（`TODO.md:55`）
- [ ] QEMU TCG モードでの virtio-net キックハック（`netstack.rs:875-878`）

---

*ネットワークスタックを少しずつ堅牢にしていこう！*
