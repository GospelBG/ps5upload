//! An archive entry's own last-modified time as Unix seconds (SPEC.md section 17: the
//! entry's time is content, so it is carried into the manifest; 0 = the archive has none).
//! A zip's DOS-style time carries no zone and is read as UTC; RAR times come from the
//! core's `dos_local_to_unix`.

/// Seconds since the Unix epoch of a civil UTC date-time; 0 when it precedes the epoch.
pub(crate) fn civil_to_unix(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> u64 {
    // Howard Hinnant's days_from_civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3_600 + mi * 60 + s).unwrap_or(0)
}

/// A 7z `FILETIME` (100 ns ticks since 1601); 0 when absent or before the epoch.
pub(crate) fn nt_to_unix(ticks: u64) -> u64 {
    (ticks / 10_000_000).saturating_sub(11_644_473_600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        assert_eq!(civil_to_unix(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(civil_to_unix(2024, 2, 29, 12, 30, 40), 1_709_209_840);
        assert_eq!(nt_to_unix(116_444_736_000_000_000 + 10_000_000), 1);
    }
}
