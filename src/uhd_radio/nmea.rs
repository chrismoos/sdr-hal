//! Reads the time and validity of an NMEA Recommended Minimum sentence, and the
//! satellite count, position and dilution of a Fix Data sentence.

use crate::discipline::ReceiverReport;

const TIME_FIELD: usize = 1;
const STATUS_FIELD: usize = 2;
const DATE_FIELD: usize = 9;
const GGA_LATITUDE_FIELD: usize = 2;
const GGA_LONGITUDE_FIELD: usize = 4;
const GGA_QUALITY_FIELD: usize = 6;
const GGA_SATELLITES_FIELD: usize = 7;
const GGA_HDOP_FIELD: usize = 8;
const GGA_ALTITUDE_FIELD: usize = 9;
/// Quality zero is no fix.
const GGA_NO_FIX: u8 = 0;
const MINUTES_PER_DEGREE: f64 = 60.0;
/// Digits of whole degrees before the minutes, in `ddmm.mmmm` and `dddmm.mmmm`.
const LATITUDE_DEGREE_DIGITS: usize = 2;
const LONGITUDE_DEGREE_DIGITS: usize = 3;

/// What a Fix Data sentence adds to a report.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct FixData {
    pub satellites: Option<u8>,
    pub position: Option<crate::ReceiverPosition>,
    pub hdop: Option<f64>,
}

/// Degrees from `ddmm.mmmm` and its hemisphere. South and west are negative.
fn degrees(value: &str, hemisphere: &str, degree_digits: usize) -> Option<f64> {
    let whole: f64 = value.get(..degree_digits)?.parse().ok()?;
    let minutes: f64 = value.get(degree_digits..)?.parse().ok()?;
    let magnitude = whole + minutes / MINUTES_PER_DEGREE;
    match hemisphere {
        "N" | "E" => Some(magnitude),
        "S" | "W" => Some(-magnitude),
        _ => None,
    }
}
const STATUS_VALID: &str = "A";
/// The date field has a two-digit year, counted from here.
const CENTURY: i64 = 2000;
const SECONDS_PER_DAY: f64 = 86_400.0;
const SECONDS_PER_HOUR: i64 = 3600;
const SECONDS_PER_MINUTE: i64 = 60;

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn two_digits(text: &str, at: usize) -> Option<i64> {
    text.get(at..at + 2)?.parse().ok()
}

/// Parses `$GPGGA,hhmmss.ss,lat,N,lon,W,quality,satellites,hdop,altitude,M,...`. The
/// position and dilution are reported only with a fix.
pub(super) fn parse_gga(sentence: &str) -> FixData {
    let fields: Vec<&str> = sentence.split(',').collect();
    let field = |index: usize| fields.get(index).map(|text| text.trim());
    let number = |index: usize| field(index).and_then(|text| text.parse::<f64>().ok());
    let satellites = field(GGA_SATELLITES_FIELD).and_then(|text| text.parse().ok());
    let has_fix = field(GGA_QUALITY_FIELD)
        .and_then(|text| text.parse::<u8>().ok())
        .is_some_and(|quality| quality != GGA_NO_FIX);
    if !has_fix {
        return FixData {
            satellites,
            ..FixData::default()
        };
    }
    let position = (|| {
        Some(crate::ReceiverPosition {
            latitude_deg: degrees(
                field(GGA_LATITUDE_FIELD)?,
                field(GGA_LATITUDE_FIELD + 1)?,
                LATITUDE_DEGREE_DIGITS,
            )?,
            longitude_deg: degrees(
                field(GGA_LONGITUDE_FIELD)?,
                field(GGA_LONGITUDE_FIELD + 1)?,
                LONGITUDE_DEGREE_DIGITS,
            )?,
            altitude_m: number(GGA_ALTITUDE_FIELD),
        })
    })();
    FixData {
        satellites,
        position,
        hdop: number(GGA_HDOP_FIELD),
    }
}

/// Parses `$GPRMC,hhmmss.ss,A,...,ddmmyy,...`. The time is `None` when the time or the
/// date is missing, as before the receiver knows them.
pub(super) fn parse_rmc(sentence: &str) -> ReceiverReport {
    let fields: Vec<&str> = sentence.split(',').collect();
    ReceiverReport {
        unix_seconds: rmc_unix_seconds(&fields),
        locked: fields.get(STATUS_FIELD) == Some(&STATUS_VALID),
        satellites: None,
        position: None,
        hdop: None,
    }
}

fn rmc_unix_seconds(fields: &[&str]) -> Option<f64> {
    let time = fields.get(TIME_FIELD)?;
    let date = fields.get(DATE_FIELD)?;
    let seconds: f64 = time.get(4..)?.parse().ok()?;
    let (hours, minutes) = (two_digits(time, 0)?, two_digits(time, 2)?);
    let (day, month, year) = (
        two_digits(date, 0)?,
        two_digits(date, 2)?,
        two_digits(date, 4)?,
    );
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hours > 23 || minutes > 59 {
        return None;
    }
    let days = days_from_civil(CENTURY + year, month, day);
    Some(
        days as f64 * SECONDS_PER_DAY
            + (hours * SECONDS_PER_HOUR + minutes * SECONDS_PER_MINUTE) as f64
            + seconds,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_sentence_gives_its_time_and_lock() {
        let report =
            parse_rmc("$GPRMC,102705.50,A,3320.35950,N,11142.96393,W,0.021,,251126,,,A*6D");
        // 2026-11-25T10:27:05.5Z
        assert_eq!(report.unix_seconds, Some(1_795_602_425.5));
        assert!(report.locked);
    }

    #[test]
    fn a_sentence_without_a_fix_still_gives_its_time() {
        let report = parse_rmc("$GPRMC,102120.00,V,,,,,,,251126,,*1C");
        assert_eq!(report.unix_seconds, Some(1_795_602_080.0));
        assert!(!report.locked);
    }

    #[test]
    fn a_sentence_without_a_date_reports_no_time_and_no_fix() {
        for sentence in [
            "$GPRMC,,V,,,,,,,,,,N*53",
            "$GPRMC,102120.00,V,,,,,,,,,*1C",
            "",
        ] {
            let report = parse_rmc(sentence);
            assert_eq!(report.unix_seconds, None, "{sentence}");
            assert!(!report.locked, "{sentence}");
        }
    }

    #[test]
    fn a_fix_sentence_gives_satellites_position_and_dilution() {
        let fix =
            parse_gga("$GNGGA,102705.50,3320.35950,N,11142.96393,W,1,06,1.42,412.1,M,-28.5,M,,*5E");
        assert_eq!(fix.satellites, Some(6));
        assert_eq!(fix.hdop, Some(1.42));
        let position = fix.position.unwrap();
        assert!((position.latitude_deg - 33.339_325).abs() < 1e-6);
        assert!((position.longitude_deg + 111.716_065_5).abs() < 1e-6);
        assert_eq!(position.altitude_m, Some(412.1));

        let south_east =
            parse_gga("$GPGGA,000000.00,3351.0000,S,15112.0000,E,2,10,0.9,5.0,M,,M,,*00");
        let position = south_east.position.unwrap();
        assert!((position.latitude_deg + 33.85).abs() < 1e-9);
        assert!((position.longitude_deg - 151.2).abs() < 1e-9);
    }

    #[test]
    fn a_fix_sentence_without_a_fix_gives_only_the_satellite_count() {
        let fix = parse_gga("$GPGGA,102120.00,,,,,0,00,99.99,,,,,,*66");
        assert_eq!(fix.satellites, Some(0));
        assert_eq!(fix.position, None);
        assert_eq!(fix.hdop, None);
        assert_eq!(parse_gga("$GPGGA,,,,,,0,,,,,,,,*66"), FixData::default());
        assert_eq!(parse_gga(""), FixData::default());
    }

    #[test]
    fn dates_convert_across_a_leap_day() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(days_from_civil(2024, 3, 1), 19_783);
    }
}
