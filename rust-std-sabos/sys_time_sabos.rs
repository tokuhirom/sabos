// sys/time/sabos.rs — SABOS 用 PAL time 実装
//
// SYS_CLOCK_MONOTONIC(26) を使って std::time::Instant を実装する。
// このシステムコールは起動からの経過ミリ秒を返す（PIT ティックから変換）。
//
// SYS_CLOCK_REALTIME(130) を使って std::time::SystemTime を実装する。
// このシステムコールは CMOS RTC から読み取った UNIX エポック秒を返す。

use crate::time::Duration;

/// SYS_CLOCK_MONOTONIC(26) を呼んで起動からの経過ミリ秒を取得する
fn clock_monotonic_ms() -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 26u64,   // SYS_CLOCK_MONOTONIC
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

/// SYS_CLOCK_REALTIME(130) を呼んで UNIX エポックからの秒数を取得する
fn clock_realtime_secs() -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            in("rax") 130u64,   // SYS_CLOCK_REALTIME
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

// ============================================================
// Instant — 単調増加クロック（SYS_CLOCK_MONOTONIC ベース）
// ============================================================

/// 単調増加クロックによる時刻表現。
/// 内部的には Duration（起動からの経過時間）を保持する。
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant(Duration);

impl Instant {
    /// 現在の単調増加時刻を取得する。
    /// SYS_CLOCK_MONOTONIC を呼んでミリ秒精度の時刻を返す。
    pub fn now() -> Instant {
        let ms = clock_monotonic_ms();
        Instant(Duration::from_millis(ms))
    }

    /// 2つの Instant の差を計算する（self - other）。
    /// self < other の場合は None を返す。
    pub fn checked_sub_instant(&self, other: &Instant) -> Option<Duration> {
        self.0.checked_sub(other.0)
    }

    /// Duration を加算した新しい Instant を返す。
    pub fn checked_add_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_add(*other)?))
    }

    /// Duration を減算した新しい Instant を返す。
    pub fn checked_sub_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_sub(*other)?))
    }
}

// ============================================================
// SystemTime — 壁時計時刻（SYS_CLOCK_REALTIME ベース）
// ============================================================

/// 壁時計時刻（リアルタイムクロック）。
/// CMOS RTC から読み取った UNIX エポック（1970-01-01 00:00:00 UTC）からの
/// 経過秒数を Duration として保持する。
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct SystemTime(Duration);

/// UNIX エポック（1970-01-01 00:00:00 UTC）
pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::from_secs(0));

impl SystemTime {
    /// SystemTime が取りうる最大値
    pub const MAX: SystemTime = SystemTime(Duration::MAX);

    /// SystemTime が取りうる最小値
    pub const MIN: SystemTime = SystemTime(Duration::ZERO);

    /// 現在のシステム時刻を取得する。
    /// SYS_CLOCK_REALTIME を呼んで CMOS RTC の時刻を返す。
    pub fn now() -> SystemTime {
        let secs = clock_realtime_secs();
        SystemTime(Duration::from_secs(secs))
    }

    /// 2つの SystemTime の差を計算する。
    /// self >= other なら Ok(Duration)、そうでなければ Err(Duration) を返す。
    pub fn sub_time(&self, other: &SystemTime) -> Result<Duration, Duration> {
        self.0.checked_sub(other.0).ok_or_else(|| other.0 - self.0)
    }

    /// Duration を加算した新しい SystemTime を返す。
    pub fn checked_add_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_add(*other)?))
    }

    /// Duration を減算した新しい SystemTime を返す。
    pub fn checked_sub_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_sub(*other)?))
    }
}
