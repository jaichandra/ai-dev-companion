//! Local calendar time, as JS `Date`'s local getters and `setHours`/`setDate`
//! see it (the scheduler's quiet hours, daily budget and digest time are in
//! the user's own time zone). Built on the C library's `localtime_r` and
//! `mktime`, so the zone and daylight saving come from the system.

/// The local calendar fields of an instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalParts {
    pub year: i32,
    /// 0-11, as `Date.getMonth()`.
    pub month0: i32,
    pub day: i32,
    pub hours: i32,
    pub minutes: i32,
    /// 0 is Sunday, as `Date.getDay()`.
    pub weekday: i32,
}

pub fn local_parts(ms: i64) -> LocalParts {
    let t: libc::time_t = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in; both pointers are valid.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    LocalParts { year: tm.tm_year + 1900, month0: tm.tm_mon, day: tm.tm_mday, hours: tm.tm_hour, minutes: tm.tm_min, weekday: tm.tm_wday }
}

/// The instant of a local wall-clock time; out-of-range fields roll over
/// (minute 510 is 08:30, day 32 is the next month), as `new Date(y, m, d, h, min)` does.
pub fn local_ms(year: i32, month0: i32, day: i32, hours: i32, minutes: i32) -> i64 {
    // SAFETY: as above; `mktime` normalizes the fields in place.
    let secs = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = year - 1900;
        tm.tm_mon = month0;
        tm.tm_mday = day;
        tm.tm_hour = hours;
        tm.tm_min = minutes;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    let secs: i64 = secs;
    secs * 1000
}

/// `d.setHours(0, minutes, 0, 0)` on the local day of `ms`, plus `plus_days`.
pub fn local_day_at(ms: i64, minutes: i32, plus_days: i32) -> i64 {
    let p = local_parts(ms);
    local_ms(p.year, p.month0, p.day + plus_days, 0, minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_fields_round_trip_through_local_ms() {
        let ms = local_ms(2026, 8, 29, 8, 30);
        assert_eq!(local_parts(ms), LocalParts { year: 2026, month0: 8, day: 29, hours: 8, minutes: 30, weekday: 2 });
    }

    #[test]
    fn fields_roll_over_like_a_js_date() {
        assert_eq!(local_day_at(local_ms(2026, 8, 29, 17, 5), 510, 0), local_ms(2026, 8, 29, 8, 30));
        assert_eq!(local_day_at(local_ms(2026, 8, 30, 17, 5), 480, 1), local_ms(2026, 9, 1, 8, 0));
        assert_eq!(local_parts(local_ms(2026, 8, 27, 0, 0)).weekday, 0, "Sunday 27 Sep 2026");
    }

    #[test]
    fn instants_before_the_epoch_have_local_fields_too() {
        assert!(local_parts(-1).year <= 1970);
    }
}
