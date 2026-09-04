//! Civil / wall-clock helpers (UTC), std-only. No chrono dependency.

/// Convert Unix epoch seconds to (year, month, day, hour, minute, second) in UTC.
/// Uses Howard Hinnant's `civil_from_days` algorithm.
pub fn unix_to_utc(secs: u64) -> (u16, u8, u8, u8, u8, u8) {
    let days = (secs as i64).div_euclid(86_400);
    let rem = (secs as i64).rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    (y as u16, mo as u8, d as u8, h as u8, m as u8, s as u8)
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as i64, d as i64)
}

/// Format seconds since epoch as the 16-digit ISO9660 volume date string
/// "YYYYMMDDHHMMSScc" (cc = hundredths of a second, always "00" here).
pub fn iso_volume_date_digits(secs: u64) -> [u8; 16] {
    let (y, mo, d, h, mi, s) = unix_to_utc(secs);
    let mut out = [0u8; 16];
    let txt = format!("{:04}{:02}{:02}{:02}{:02}{:02}00", y, mo, d, h, mi, s);
    out.copy_from_slice(txt.as_bytes());
    out
}

/// 7-byte ISO9660 directory-record date: [year-1900, month, day, hour, min, sec, gmt-offset].
/// Offset is stored in 15-minute units; 0 == UTC.
pub fn iso_record_date(secs: u64) -> [u8; 7] {
    let (y, mo, d, h, mi, s) = unix_to_utc(secs);
    let year = y.saturating_sub(1900).min(255) as u8;
    [year, mo, d, h, mi, s, 0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch() {
        assert_eq!(unix_to_utc(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn known_date() {
        // 2024-02-29 12:34:56 UTC  (leap day)
        let t = 1_709_210_096;
        assert_eq!(unix_to_utc(t), (2024, 2, 29, 12, 34, 56));
    }

    #[test]
    fn iso9660_year_window() {
        let (y, mo, d, _, _, _) = unix_to_utc(4_102_444_800); // 2100-01-01
        assert_eq!((y, mo, d), (2100, 1, 1));
        // year byte = year-1900 must fit u8
        assert_eq!(iso_record_date(4_102_444_800)[0], 200);
    }

    #[test]
    fn digits16() {
        let d = iso_volume_date_digits(1_709_210_096);
        assert_eq!(&d, b"2024022912345600");
    }
}
