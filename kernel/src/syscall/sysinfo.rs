// syscall/sysinfo.rs — システム情報関連システムコール
//
// SYS_GET_MEM/TASK/NET_INFO, SYS_PCI_CONFIG_READ,
// SYS_CLOCK_MONOTONIC/REALTIME, write_mem_info, write_task_list

use crate::user_ptr::SyscallError;
use super::{user_slice_from_args, SliceWriter, write_json_string};

/// メモリ情報をテキスト形式で書き込む（SYS_GET_MEM_INFO 用）
fn write_mem_info(buf: &mut [u8]) -> usize {
    use crate::memory::FRAME_ALLOCATOR;
    use core::fmt::Write;

    // メモリ情報を取得
    let fa = FRAME_ALLOCATOR.lock();
    let total = fa.total_frames();
    let allocated = fa.allocated_count();
    let free = fa.free_frames();
    drop(fa);  // ロックを早めに解放

    // JSON 形式で書き込む
    let mut writer = SliceWriter::new(buf);
    let _ = write!(
        writer,
        "{{\"total_frames\":{},\"allocated_frames\":{},\"free_frames\":{},\"free_kib\":{}}}\n",
        total,
        allocated,
        free,
        free * 4
    );

    writer.written()
}

/// タスク一覧をテキスト形式で書き込む（SYS_GET_TASK_LIST 用）
fn write_task_list(buf: &mut [u8]) -> usize {
    use crate::scheduler::{self, TaskState};
    use core::fmt::Write;

    // タスク一覧を取得
    let tasks = scheduler::task_list();

    // JSON 形式で書き込む
    let mut writer = SliceWriter::new(buf);

    let _ = write!(writer, "{{\"tasks\":[");
    for (i, t) in tasks.iter().enumerate() {
        let state_str = match t.state {
            TaskState::Ready => "Ready",
            TaskState::Running => "Running",
            TaskState::Sleeping(_) => "Sleeping",
            TaskState::Finished => "Finished",
        };
        let type_str = if t.is_user_process { "user" } else { "kernel" };
        if i != 0 {
            let _ = write!(writer, ",");
        }
        let _ = write!(writer, "{{\"id\":{},\"state\":\"", t.id);
        let _ = writer.write_str(state_str);
        let _ = write!(writer, "\",\"type\":\"");
        let _ = writer.write_str(type_str);
        let _ = write!(writer, "\",\"name\":\"");
        let _ = write_json_string(&mut writer, t.name.as_str());
        let _ = write!(writer, "\"}}");
    }
    let _ = write!(writer, "]}}\n");

    writer.written()
}

/// SYS_GET_MEM_INFO: メモリ情報を取得
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式（テキスト）:
///   total_frames=XXXX
///   allocated_frames=XXXX
///   free_frames=XXXX
///   free_kib=XXXX
pub(crate) fn sys_get_mem_info(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();
    Ok(write_mem_info(buf) as u64)
}

/// SYS_GET_TASK_LIST: タスク一覧を取得
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式（テキスト、1行目はヘッダ）:
///   id,state,type,name
///   1,Running,kernel,shell
///   2,Ready,user,HELLO.ELF
pub(crate) fn sys_get_task_list(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();
    Ok(write_task_list(buf) as u64)
}

/// SYS_GET_NET_INFO: ネットワーク情報を取得
///
/// 引数:
///   arg1 — バッファのポインタ（ユーザー空間、書き込み先）
///   arg2 — バッファの長さ
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
///
/// 出力形式（テキスト）:
///   ip=X.X.X.X
///   gateway=X.X.X.X
///   dns=X.X.X.X
///   mac=XX:XX:XX:XX:XX:XX
pub(crate) fn sys_get_net_info(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    use core::fmt::Write;

    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    // ネットワーク情報を取得
    let my_ip = crate::net_config::get_my_ip();
    let gateway = crate::net_config::get_gateway_ip();
    let dns = crate::net_config::get_dns_server_ip();

    // テキスト形式で書き込む
    let mut writer = SliceWriter::new(buf);
    let _ = writeln!(writer, "ip={}.{}.{}.{}", my_ip[0], my_ip[1], my_ip[2], my_ip[3]);
    let _ = writeln!(writer, "gateway={}.{}.{}.{}", gateway[0], gateway[1], gateway[2], gateway[3]);
    let _ = writeln!(writer, "dns={}.{}.{}.{}", dns[0], dns[1], dns[2], dns[3]);

    // MAC アドレスを取得（virtio-net が初期化されていれば）
    let drv = crate::virtio_net::VIRTIO_NET.lock();
    if let Some(ref d) = *drv {
        let mac = d.mac_address;
        let _ = writeln!(writer, "mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
    } else {
        let _ = writeln!(writer, "mac=none");
    }

    Ok(writer.written() as u64)
}

/// SYS_PCI_CONFIG_READ: PCI Configuration Space を読み取る
///
/// 引数:
///   arg1 — バス番号 (0-255)
///   arg2 — デバイス番号 (0-31)
///   arg3 — ファンクション番号 (0-7)
///   arg4 — offset と size を詰めた値
///          - 下位 8 ビット: offset
///          - 上位 8 ビット: size (1/2/4)
///
/// 戻り値:
///   読み取った値（下位 32 ビットに格納）
///   負の値（エラー時）
pub(crate) fn sys_pci_config_read(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let bus = arg1 as u8;
    let device = arg2 as u8;
    let function = arg3 as u8;

    // arg4 の下位 16 ビットに offset/size を詰める
    let offset = (arg4 & 0xFF) as u8;
    let size = ((arg4 >> 8) & 0xFF) as u8;

    // 余分なビットが立っている場合は不正扱い
    if (arg4 >> 16) != 0 {
        return Err(SyscallError::InvalidArgument);
    }

    // 範囲チェック
    if arg1 > 0xFF || arg2 > 31 || arg3 > 7 {
        return Err(SyscallError::InvalidArgument);
    }

    // サイズとアライメントのチェック
    match size {
        1 => {}
        2 => {
            if (offset & 1) != 0 || offset > 0xFE {
                return Err(SyscallError::InvalidArgument);
            }
        }
        4 => {
            if (offset & 3) != 0 || offset > 0xFC {
                return Err(SyscallError::InvalidArgument);
            }
        }
        _ => {
            return Err(SyscallError::InvalidArgument);
        }
    }

    let val32 = crate::pci::pci_config_read32(bus, device, function, offset & 0xFC);
    let value = match size {
        1 => {
            let shift = (offset & 3) * 8;
            (val32 >> shift) & 0xFF
        }
        2 => {
            let shift = (offset & 2) * 8;
            (val32 >> shift) & 0xFFFF
        }
        4 => val32,
        _ => 0,
    };

    Ok(value as u64)
}

/// SYS_CLOCK_MONOTONIC: 起動からの経過ミリ秒を返す
///
/// PIT (Programmable Interval Timer) のティックカウントをミリ秒に変換する。
/// PIT のデフォルト周波数: 1193182 Hz / 65536 ≈ 18.2065 Hz
/// 1 ティック ≈ 54.925 ms
/// ms = ticks * 10000 / 182 （scheduler.rs の sleep_ms と逆算式）
///
/// 戻り値: 起動からの経過ミリ秒
pub(crate) fn sys_clock_monotonic() -> Result<u64, SyscallError> {
    let ticks = crate::interrupts::TIMER_TICK_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    // ticks → ms 変換 (sleep_ms の逆: ms = ticks * 10000 / 182)
    let ms = ticks * 10000 / 182;
    Ok(ms)
}

/// SYS_CLOCK_REALTIME: CMOS RTC から現在時刻を読み取り、
/// UNIX エポック（1970-01-01 00:00:00 UTC）からの秒数を返す。
///
/// 戻り値: UNIX エポックからの秒数
pub(crate) fn sys_clock_realtime() -> Result<u64, SyscallError> {
    Ok(crate::rtc::read_unix_epoch_seconds())
}
