// net_config.rs — ネットワーク設定定数
//
// カーネルはプロトコル処理を行わない（netd に一元化）が、
// ユーザープログラムが SYS_GET_NET_INFO で設定を取得するために
// IP/ゲートウェイ/DNS の定数が必要。
//
// QEMU SLIRP のデフォルト値を定義する。

/// ゲストの IP アドレス (QEMU user mode デフォルト)
pub const MY_IP: [u8; 4] = [10, 0, 2, 15];

/// ゲートウェイの IP アドレス
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

/// DNS サーバーの IP アドレス (QEMU user mode デフォルト)
pub const DNS_SERVER_IP: [u8; 4] = [10, 0, 2, 3];
