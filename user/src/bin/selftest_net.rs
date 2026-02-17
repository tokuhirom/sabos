// selftest_net.rs — ネットワーク API の自動テスト（独立バイナリ）
//
// shell.rs に組み込まれていた cmd_selftest_net() を独立 ELF に切り出したもの。
// telnet 経由（tsh の run コマンド）で実行し、stdout に結果を出力する。
// /9p/user/target/x86_64-unknown-none/debug/selftest_net として実行可能。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../net.rs"]
mod net;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    run_selftest_net();
    syscall::exit();
}

fn run_selftest_net() {
    syscall::write_str("=== NET SELFTEST START ===\n");
    let mut passed = 0u32;
    let mut total = 0u32;

    // テスト 1: Ipv4Addr / SocketAddr の基本操作
    total += 1;
    {
        let ip = net::Ipv4Addr::new(1, 2, 3, 4);
        let addr = net::SocketAddr::new(ip, 80);
        if ip.octets == [1, 2, 3, 4] && addr.port == 80 && addr.ip == ip {
            syscall::write_str("[PASS] net_addr_types\n");
            passed += 1;
        } else {
            syscall::write_str("[FAIL] net_addr_types\n");
        }
    }

    // テスト 2: DNS 名前解決（example.com）
    total += 1;
    match net::dns_lookup("example.com") {
        Ok(ip) => {
            // example.com は 93.184.215.14 だが、IP は変わりうるので非ゼロなら OK
            if ip.octets != [0, 0, 0, 0] {
                syscall::write_str("[PASS] net_dns_lookup\n");
                passed += 1;
            } else {
                syscall::write_str("[FAIL] net_dns_lookup (zero IP)\n");
            }
        }
        Err(_) => {
            syscall::write_str("[FAIL] net_dns_lookup (error)\n");
        }
    }

    // テスト 3: TcpStream::connect + HTTP GET（example.com:80）
    total += 1;
    {
        let ok = (|| -> Result<bool, net::NetError> {
            let ip = net::dns_lookup("example.com")?;
            let addr = net::SocketAddr::new(ip, 80);
            let mut stream = net::TcpStream::connect(addr)?;
            stream.set_recv_timeout(5000);

            // 最小限の HTTP リクエスト
            stream.write(b"GET / HTTP/1.0\r\nHost: example.com\r\nConnection: close\r\n\r\n")?;

            // レスポンスを受信
            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf)?;
            if n > 0 {
                // "HTTP/" で始まるレスポンスが返ってくれば成功
                if n >= 5 && &buf[..5] == b"HTTP/" {
                    return Ok(true);
                }
            }
            Ok(false)
        })();

        match ok {
            Ok(true) => {
                syscall::write_str("[PASS] net_tcp_http_get\n");
                passed += 1;
            }
            _ => {
                syscall::write_str("[FAIL] net_tcp_http_get\n");
            }
        }
    }

    // テスト 4: UdpSocket — DNS サーバーに手動クエリ送信
    //
    // UdpSocket::bind(0) でエフェメラルポートにバインドし、
    // DNS クエリパケットを手動構築して 10.0.2.3:53 に send_to。
    // recv_from で DNS レスポンスを受信し、送信元が 10.0.2.3:53 であることを確認する。
    total += 1;
    {
        let ok = (|| -> Result<bool, net::NetError> {
            let mut sock = net::UdpSocket::bind(0)?;
            sock.set_recv_timeout(5000);

            // DNS クエリを手動構築: example.com の A レコードを問い合わせ
            // ヘッダー (12 bytes): ID=0xABCD, Flags=RD(0x0100), QDCOUNT=1
            let mut query: [u8; 33] = [0; 33];
            query[0] = 0xAB; query[1] = 0xCD; // ID
            query[2] = 0x01; query[3] = 0x00; // Flags: RD=1
            query[4] = 0x00; query[5] = 0x01; // QDCOUNT=1
            // QNAME: \x07example\x03com\x00
            query[12] = 7;
            query[13..20].copy_from_slice(b"example");
            query[20] = 3;
            query[21..24].copy_from_slice(b"com");
            query[24] = 0;
            // QTYPE=A(1), QCLASS=IN(1)
            query[25] = 0x00; query[26] = 0x01;
            query[27] = 0x00; query[28] = 0x01;

            let dns_addr = net::SocketAddr::new(net::Ipv4Addr::new(10, 0, 2, 3), 53);
            sock.send_to(&query[..29], dns_addr)?;

            // レスポンスを受信
            let mut buf = [0u8; 512];
            let (n, addr) = sock.recv_from(&mut buf)?;

            // 送信元が DNS サーバー (10.0.2.3:53) であること
            if addr.ip.octets != [10, 0, 2, 3] || addr.port != 53 {
                return Ok(false);
            }
            // レスポンスの ID が一致すること
            if n >= 2 && buf[0] == 0xAB && buf[1] == 0xCD {
                return Ok(true);
            }
            Ok(false)
        })();

        match ok {
            Ok(true) => {
                syscall::write_str("[PASS] net_udp_dns_query\n");
                passed += 1;
            }
            Ok(false) => {
                syscall::write_str("[FAIL] net_udp_dns_query (wrong response)\n");
            }
            Err(_) => {
                syscall::write_str("[FAIL] net_udp_dns_query (error)\n");
            }
        }
    }

    // テスト 5: IPv6 ping (fec0::2 = QEMU ゲートウェイ)
    total += 1;
    {
        let ipv6_gw = net::Ipv6Addr::from_octets(
            [0xfe, 0xc0, 0,0,0,0,0,0, 0,0,0,0,0,0,0, 0x02]
        );
        match net::ping6(&ipv6_gw, 5000) {
            Ok(_src_ip) => {
                syscall::write_str("[PASS] net_ipv6_ping\n");
                passed += 1;
            }
            Err(_) => {
                syscall::write_str("[FAIL] net_ipv6_ping\n");
            }
        }
    }

    // 結果出力
    write_summary(passed, total);
}

/// テスト結果のサマリーを出力する
fn write_summary(passed: u32, total: u32) {
    syscall::write_str("=== NET SELFTEST END: ");
    write_number(passed as u64);
    syscall::write_str("/");
    write_number(total as u64);
    syscall::write_str(" PASSED ===\n");
}

/// 数値を文字列として stdout に出力する
fn write_number(n: u64) {
    if n == 0 {
        syscall::write_str("0");
        return;
    }

    // 数字を逆順に格納
    let mut buf = [0u8; 20]; // u64 最大は 20 桁
    let mut i = 0;
    let mut num = n;

    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }

    // 逆順に出力
    while i > 0 {
        i -= 1;
        syscall::write(&[buf[i]]);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
