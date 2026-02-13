// init.rs — SABOS init プロセス
//
// 最初のユーザープロセスとしてカーネルから起動される。
// 責務:
// 1. サービス（netd）を起動
// 2. シェルを起動
// 3. クラッシュしたサービスを再起動（restart: true のサービスのみ）
// 4. シェルが終了しても init 自体は終了しない（supervisor として常駐）
//
// マイクロカーネルアーキテクチャへの第一歩として、
// カーネルからサービス管理をユーザー空間に移行する。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use core::panic::PanicInfo;

use core::sync::atomic::{AtomicU64, Ordering};

/// サービスの定義
struct Service {
    /// サービス名（ログ表示用）
    name: &'static str,
    /// ELF ファイルのパス
    path: &'static str,
    /// クラッシュ時に自動再起動するかどうか
    restart: bool,
    /// 起動されたタスク ID（0 = 未起動）
    /// AtomicU64 を使うことで static mut を避ける
    task_id: AtomicU64,
}

// Service を Sync にするために Send + Sync を実装
// 単一スレッド（シングルコア）環境なので安全
unsafe impl Sync for Service {}

/// 管理するサービスの一覧
///
/// **起動順序が重要**:
/// fat32d は最後に起動する。これにより他サービスの ELF ロードが
/// カーネル内 FAT32 経由（高速）で行われる。fat32d を最初に起動すると
/// VFS が IPC プロキシに切り替わり、以後の ELF ロードが fat32d IPC 経由
/// （毎回ディスク読み出し + コンテキストスイッチ）になって遅い。
///
/// fat32d 登録後は、ランタイムのファイル操作（ls, cat 等）が fat32d 経由になる。
///
/// - netd: ネットワークサービス（再起動有効）
/// - gui: GUI サービス（再起動有効）
/// - telnetd: Telnet サービス（再起動有効）
/// - httpd: HTTP サービス（再起動有効）
/// - shell: ユーザーシェル（再起動無効 — ユーザーが明示的に終了したら終わり）
/// - fat32d: FAT32 ファイルシステムサービス（最後に起動）
static SERVICES: [Service; 6] = [
    Service {
        name: "netd",
        path: "/NETD.ELF",
        restart: true,
        task_id: AtomicU64::new(0),
    },
    Service {
        name: "gui",
        path: "/GUI.ELF",
        restart: true,
        task_id: AtomicU64::new(0),
    },
    Service {
        name: "telnetd",
        path: "/TELNETD.ELF",
        restart: true,
        task_id: AtomicU64::new(0),
    },
    Service {
        name: "httpd",
        path: "/HTTPD.ELF",
        restart: true,
        task_id: AtomicU64::new(0),
    },
    Service {
        name: "shell",
        path: "/SHELL.ELF",
        restart: false,
        task_id: AtomicU64::new(0),
    },
    Service {
        name: "fat32d",
        path: "/FAT32D.ELF",
        restart: true,
        task_id: AtomicU64::new(0),
    },
];

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    syscall::write_str("\n");
    syscall::write_str("[init] SABOS init process starting...\n");

    let my_pid = syscall::getpid();
    syscall::write_str("[init] PID = ");
    write_number(my_pid);
    syscall::write_str("\n");

    // 1. サービスを起動
    start_services();

    // 2. supervisor ループ — 子プロセスの終了を監視して必要に応じて再起動
    syscall::write_str("[init] Entering supervisor loop\n");
    supervisor_loop();
}

/// サービスを起動する
fn start_services() {
    for service in SERVICES.iter() {
        syscall::write_str("[init] Starting ");
        syscall::write_str(service.name);
        syscall::write_str("...\n");

        let result = syscall::spawn(service.path);
        if result < 0 {
            syscall::write_str("[init] ERROR: Failed to start ");
            syscall::write_str(service.name);
            syscall::write_str("\n");
            service.task_id.store(0, Ordering::Relaxed);
        } else {
            let task_id = result as u64;
            service.task_id.store(task_id, Ordering::Relaxed);
            syscall::write_str("[init] Started ");
            syscall::write_str(service.name);
            syscall::write_str(" (PID ");
            write_number(task_id);
            syscall::write_str(")\n");
        }

        // fat32d 起動後は初期化完了を待つ。
        // fat32d が SYS_FS_REGISTER を呼んで VFS を IPC プロキシに切り替えるまで
        // 200ms 待機する。fat32d は最後に起動されるので、他サービスの ELF は
        // カーネル内 FAT32 で高速にロード済み。以後のランタイムファイル操作
        // （ls, cat 等）が fat32d 経由になる。
        if service.name == "fat32d" {
            syscall::write_str("[init] Waiting for fat32d to register...\n");
            syscall::sleep(200);
        }
    }
}

/// supervisor ループ — 子プロセスの終了を監視して必要に応じて再起動
///
/// waitpid(0, 0) で任意の子プロセスの終了を待ち、戻り値の child_task_id から
/// どのサービスが終了したかを正確に特定する。
/// 従来は wait() で exit_code しか返されず、全サービスを総当たりで確認していたが、
/// waitpid により O(1) でサービスを特定できるようになった。
fn supervisor_loop() -> ! {
    loop {
        // 任意の子プロセスの終了を waitpid で待つ
        let (child_id, _exit_code) = syscall::waitpid(0, 0);

        if child_id < 0 {
            // エラー（子プロセスがいない、など）
            // 子プロセスがいない場合は idle として待機
            syscall::sleep(1000);
            continue;
        }

        let child_id = child_id as u64;

        // waitpid で返された child_id からどのサービスが終了したか特定
        let mut found = false;
        for service in SERVICES.iter() {
            let task_id = service.task_id.load(Ordering::Relaxed);
            if task_id != child_id {
                continue;
            }
            found = true;

            // サービスを再起動するかどうか判断
            if service.restart {
                syscall::write_str("[init] Service ");
                syscall::write_str(service.name);
                syscall::write_str(" (PID ");
                write_number(child_id);
                syscall::write_str(") exited, restarting...\n");

                let result = syscall::spawn(service.path);
                if result < 0 {
                    syscall::write_str("[init] ERROR: Failed to restart ");
                    syscall::write_str(service.name);
                    syscall::write_str("\n");
                    service.task_id.store(0, Ordering::Relaxed);
                } else {
                    let new_task_id = result as u64;
                    service.task_id.store(new_task_id, Ordering::Relaxed);
                    syscall::write_str("[init] Restarted ");
                    syscall::write_str(service.name);
                    syscall::write_str(" (PID ");
                    write_number(new_task_id);
                    syscall::write_str(")\n");
                }
            } else {
                syscall::write_str("[init] Service ");
                syscall::write_str(service.name);
                syscall::write_str(" (PID ");
                write_number(child_id);
                syscall::write_str(") exited (no restart)\n");
                service.task_id.store(0, Ordering::Relaxed);
            }
            break;
        }

        if !found {
            // 未知の子プロセスが終了した（SERVICES に登録されていない）
            syscall::write_str("[init] Unknown child PID ");
            write_number(child_id);
            syscall::write_str(" exited\n");
        }
    }
}

/// 数値を文字列として出力
fn write_number(n: u64) {
    if n == 0 {
        syscall::write_str("0");
        return;
    }

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
    syscall::write_str("[init] PANIC!\n");
    // init がパニックしてもシステムを停止しないように無限ループ
    // 本来は再起動やシャットダウンを行うべきだが、学習用なのでシンプルに
    loop {
        syscall::sleep(1000);
    }
}
