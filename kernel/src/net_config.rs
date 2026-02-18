// net_config.rs — ネットワーク設定（ランタイム変更可能）
//
// DHCP で取得した IP / ゲートウェイ / DNS / サブネットマスクを保持する。
// デフォルト値は QEMU SLIRP のデフォルト（10.0.2.15 等）。
//
// 以前は `pub const` だったが、DHCP クライアントから実行時に変更できるよう
// `static Mutex<NetConfig>` に変更した。

use spin::Mutex;

/// ネットワーク設定を保持する構造体
pub struct NetConfig {
    /// ゲストの IP アドレス
    pub my_ip: [u8; 4],
    /// ゲートウェイの IP アドレス
    pub gateway_ip: [u8; 4],
    /// DNS サーバーの IP アドレス
    pub dns_server_ip: [u8; 4],
    /// サブネットマスク
    pub subnet_mask: [u8; 4],
}

/// グローバルネットワーク設定（Mutex で保護）
///
/// デフォルト値は QEMU SLIRP 互換。DHCP 取得後に set_config() で上書きされる。
static NET_CONFIG: Mutex<NetConfig> = Mutex::new(NetConfig {
    my_ip: [10, 0, 2, 15],
    gateway_ip: [10, 0, 2, 2],
    dns_server_ip: [10, 0, 2, 3],
    subnet_mask: [255, 255, 255, 0],
});

/// 自分の IP アドレスを取得する
pub fn get_my_ip() -> [u8; 4] {
    NET_CONFIG.lock().my_ip
}

/// ゲートウェイの IP アドレスを取得する
pub fn get_gateway_ip() -> [u8; 4] {
    NET_CONFIG.lock().gateway_ip
}

/// DNS サーバーの IP アドレスを取得する
pub fn get_dns_server_ip() -> [u8; 4] {
    NET_CONFIG.lock().dns_server_ip
}

/// サブネットマスクを取得する
pub fn get_subnet_mask() -> [u8; 4] {
    NET_CONFIG.lock().subnet_mask
}

/// ネットワーク設定を一括更新する（DHCP 取得時に呼ばれる）
pub fn set_config(
    my_ip: [u8; 4],
    gateway_ip: [u8; 4],
    dns_server_ip: [u8; 4],
    subnet_mask: [u8; 4],
) {
    let mut config = NET_CONFIG.lock();
    config.my_ip = my_ip;
    config.gateway_ip = gateway_ip;
    config.dns_server_ip = dns_server_ip;
    config.subnet_mask = subnet_mask;
}
