// rtc.rs — CMOS RTC（リアルタイムクロック）ドライバ
//
// x86_64 の CMOS RTC はポート 0x70/0x71 を使ってアクセスする。
// 年月日時分秒を BCD 形式で読み取り、UNIX エポック（1970-01-01 00:00:00 UTC）
// からの秒数に変換して返す。
//
// CMOS RTC は通常 UTC で保持されるが、BIOS 設定によってはローカル時刻の場合もある。
// SABOS では UTC を仮定する。
//
// ## ポートアクセス
//
// - ポート 0x70（インデックスレジスタ）: 読みたいレジスタの番号を書き込む
// - ポート 0x71（データレジスタ）: 0x70 で指定したレジスタの値を読み取る
//
// ## UIP（Update In Progress）フラグ
//
// ステータスレジスタ A（0x0A）のビット 7 が 1 の場合、
// RTC が現在レジスタを更新中なので読み取りを待つ必要がある。
// 更新中に読むと不整合なデータを返す可能性がある。

use x86_64::instructions::port::Port;

/// CMOS RTC のレジスタアドレス
const RTC_SECONDS: u8 = 0x00;
const RTC_MINUTES: u8 = 0x02;
const RTC_HOURS: u8 = 0x04;
const RTC_DAY: u8 = 0x07;
const RTC_MONTH: u8 = 0x08;
const RTC_YEAR: u8 = 0x09;
const RTC_CENTURY: u8 = 0x32;
const RTC_STATUS_A: u8 = 0x0A;
const RTC_STATUS_B: u8 = 0x0B;

/// CMOS RTC レジスタを 1 バイト読み取る。
///
/// ポート 0x70 にレジスタ番号を書き込み、ポート 0x71 からデータを読む。
/// NMI ビット（ビット 7）は 0 にして NMI を有効のままにする。
fn cmos_read(reg: u8) -> u8 {
    let mut index_port = Port::new(0x70);
    let mut data_port = Port::new(0x71);
    unsafe {
        // ビット 7 = 0 で NMI は有効のまま
        index_port.write(reg);
        data_port.read()
    }
}

/// BCD（二進化十進数）をバイナリに変換する。
///
/// BCD は上位 4 ビットが十の位、下位 4 ビットが一の位を表す。
/// 例: 0x59 → 59（10進数）
fn bcd_to_binary(bcd: u8) -> u8 {
    ((bcd >> 4) * 10) + (bcd & 0x0F)
}

/// UIP（Update In Progress）フラグが 0 になるまで待つ。
///
/// ステータスレジスタ A のビット 7 が 1 の場合、
/// RTC が更新中なので読み取りを避ける。
/// 最大 10000 回のループで待つ（通常は数マイクロ秒で完了する）。
fn wait_for_uip_clear() {
    for _ in 0..10000 {
        if cmos_read(RTC_STATUS_A) & 0x80 == 0 {
            return;
        }
    }
    // タイムアウト — UIP が解除されなくても続行する（最善努力）
}

/// CMOS RTC から現在時刻を読み取る。
///
/// 年月日時分秒を (year, month, day, hour, minute, second) のタプルで返す。
/// 整合性を保証するため、2 回連続で同じ値が読めるまでリトライする。
fn read_rtc_raw() -> (u16, u8, u8, u8, u8, u8) {
    // ステータスレジスタ B のビット 2: データ形式（0=BCD, 1=バイナリ）
    // ステータスレジスタ B のビット 1: 時間形式（0=12時間, 1=24時間）
    let status_b = cmos_read(RTC_STATUS_B);
    let is_binary = status_b & 0x04 != 0;
    let is_24h = status_b & 0x02 != 0;

    // 整合性チェック: 2 回読んで同じ値になるまでリトライ
    loop {
        wait_for_uip_clear();

        let sec1 = cmos_read(RTC_SECONDS);
        let min1 = cmos_read(RTC_MINUTES);
        let hour1 = cmos_read(RTC_HOURS);
        let day1 = cmos_read(RTC_DAY);
        let month1 = cmos_read(RTC_MONTH);
        let year1 = cmos_read(RTC_YEAR);
        let century1 = cmos_read(RTC_CENTURY);

        wait_for_uip_clear();

        let sec2 = cmos_read(RTC_SECONDS);
        let min2 = cmos_read(RTC_MINUTES);
        let hour2 = cmos_read(RTC_HOURS);
        let day2 = cmos_read(RTC_DAY);
        let month2 = cmos_read(RTC_MONTH);
        let year2 = cmos_read(RTC_YEAR);
        let century2 = cmos_read(RTC_CENTURY);

        // 2 回の読み取りが一致したら整合性 OK
        if sec1 == sec2
            && min1 == min2
            && hour1 == hour2
            && day1 == day2
            && month1 == month2
            && year1 == year2
            && century1 == century2
        {
            // BCD → バイナリ変換（必要な場合）
            let (sec, min, mut hour, day, month, year_2digit, century) = if is_binary {
                (sec1, min1, hour1, day1, month1, year1, century1)
            } else {
                (
                    bcd_to_binary(sec1),
                    bcd_to_binary(min1),
                    bcd_to_binary(hour1 & 0x7F), // 12 時間制の PM フラグを除外
                    bcd_to_binary(day1),
                    bcd_to_binary(month1),
                    bcd_to_binary(year1),
                    bcd_to_binary(century1),
                )
            };

            // 12 時間制 → 24 時間制変換
            if !is_24h {
                let is_pm = hour1 & 0x80 != 0;
                if is_pm && hour != 12 {
                    hour += 12;
                } else if !is_pm && hour == 12 {
                    hour = 0;
                }
            }

            // 完全な西暦年を組み立てる
            // century レジスタが 0 の場合（一部のハードウェアで未対応）は 20 を仮定
            let full_century = if century == 0 { 20u16 } else { century as u16 };
            let full_year = full_century * 100 + year_2digit as u16;

            return (full_year, month, day, hour, min, sec);
        }
        // 不一致なら再試行
    }
}

/// Gregorian 暦の日付を UNIX エポック（1970-01-01 00:00:00 UTC）からの秒数に変換する。
///
/// 閏年の計算を含む正確な変換を行う。
/// 年は 1970 以降を想定している。
///
/// アルゴリズム:
/// 1. 1970 年から指定年の前年までの日数を計算
/// 2. 指定年の 1 月から指定月の前月までの日数を加算
/// 3. 閏年の 2 月補正
/// 4. 日・時・分・秒を加算
fn datetime_to_unix_epoch(year: u16, month: u8, day: u8, hour: u8, min: u8, sec: u8) -> u64 {
    // 各月の日数（非閏年）
    const DAYS_IN_MONTH: [u16; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    let year = year as u64;
    let month = month as u64;
    let day = day as u64;
    let hour = hour as u64;
    let min = min as u64;
    let sec = sec as u64;

    // 1970 年から year-1 年までの日数を計算
    let mut total_days: u64 = 0;
    for y in 1970..year {
        total_days += if is_leap_year(y as u16) { 366 } else { 365 };
    }

    // 1 月から month-1 月までの日数を加算
    for m in 0..(month - 1) {
        total_days += DAYS_IN_MONTH[m as usize] as u64;
    }

    // 閏年で 3 月以降なら 1 日追加
    if month > 2 && is_leap_year(year as u16) {
        total_days += 1;
    }

    // 日を加算（1 日始まりなので -1）
    total_days += day - 1;

    // 秒に変換
    total_days * 86400 + hour * 3600 + min * 60 + sec
}

/// 指定した年が閏年かどうかを判定する。
///
/// 閏年の条件:
/// - 4 で割り切れる
/// - ただし 100 で割り切れる年は閏年ではない
/// - ただし 400 で割り切れる年は閏年
fn is_leap_year(year: u16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// CMOS RTC から現在時刻を読み取り、UNIX エポックからの秒数を返す。
///
/// これが SYS_CLOCK_REALTIME のエントリポイント。
pub fn read_unix_epoch_seconds() -> u64 {
    let (year, month, day, hour, min, sec) = read_rtc_raw();
    datetime_to_unix_epoch(year, month, day, hour, min, sec)
}
