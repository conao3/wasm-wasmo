use serde_json::Value;

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parses ISO 8601 (`2015-01-25T08:00:00Z`, optional fraction and offset)
/// into epoch seconds.
pub fn parse_iso8601(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
        num(17..19)?,
    );
    let mut rest = &s[19..];
    let mut frac = 0.0;
    if let Some(r) = rest.strip_prefix('.') {
        let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
        frac = format!("0.{}", &r[..end]).parse().ok()?;
        rest = &r[end..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() == 6 && matches!(&o[0..1], "+" | "-") => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o[1..3].parse::<i64>().ok()? * 3600 + o[4..6].parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se - offset;
    Some(secs as f64 + frac)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// Parses an RFC 7231 IMF-fixdate (`Sun, 25 Jan 2015 08:00:00 GMT`).
pub fn parse_http_date(s: &str) -> Option<f64> {
    let s = s.split_once(", ").map_or(s, |(_, r)| r);
    let mut it = s.split_whitespace();
    let d: i64 = it.next()?.parse().ok()?;
    let month = it.next()?;
    let mo = MONTHS
        .iter()
        .position(|m| *m == month)
        .map(|i| i as i64 + 1)?;
    let y: i64 = it.next()?.parse().ok()?;
    let t = it.next()?;
    let (hms, frac) = t.split_once('.').map_or((t, None), |(a, b)| (a, Some(b)));
    let mut hp = hms.split(':').map(|x| x.parse::<i64>().ok());
    let (h, mi, se) = (hp.next()??, hp.next()??, hp.next()??);
    let frac = frac.map_or(Some(0.0), |f| format!("0.{f}").parse().ok())?;
    Some((days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se) as f64 + frac)
}

fn split(epoch: f64) -> (i64, i64, i64, i64, i64, i64, f64) {
    let secs = epoch.floor() as i64;
    let frac = epoch - secs as f64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60, frac)
}

fn fraction(frac: f64) -> String {
    if frac <= 0.0 {
        return String::new();
    }
    let ms = (frac * 1000.0).round() as i64;
    let s = format!("{ms:03}");
    format!(".{}", s.trim_end_matches('0'))
}

pub fn format_iso8601(epoch: f64) -> String {
    let (y, m, d, h, mi, s, frac) = split(epoch);
    format!(
        "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}{}Z",
        fraction(frac)
    )
}

pub fn format_http_date(epoch: f64) -> String {
    let (y, m, d, h, mi, s, frac) = split(epoch);
    let wd = WEEKDAYS[(epoch.floor() as i64).div_euclid(86400).rem_euclid(7) as usize];
    format!(
        "{wd}, {d:02} {} {y:04} {h:02}:{mi:02}:{s:02}{} GMT",
        MONTHS[m as usize - 1],
        fraction(frac)
    )
}

pub fn format_epoch(epoch: f64) -> String {
    if epoch.fract() == 0.0 {
        format!("{}", epoch as i64)
    } else {
        format!("{epoch}")
    }
}

pub fn parse_any(s: &str, format: &str) -> Option<f64> {
    match format {
        "unixTimestamp" => s.parse().ok(),
        "rfc822" => parse_http_date(s),
        _ => parse_iso8601(s).or_else(|| s.parse().ok()),
    }
}

pub fn epoch_value(epoch: f64) -> Value {
    if epoch.fract() == 0.0 {
        Value::from(epoch as i64)
    } else {
        Value::from(epoch)
    }
}

pub fn value_epoch(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_iso8601(s).or_else(|| s.parse().ok()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        assert_eq!(parse_iso8601("2015-01-25T08:00:00Z"), Some(1422172800.0));
        assert_eq!(format_iso8601(1422172800.0), "2015-01-25T08:00:00Z");
        assert_eq!(format_iso8601(1422172800.123), "2015-01-25T08:00:00.123Z");
        assert_eq!(
            format_http_date(1422172800.0),
            "Sun, 25 Jan 2015 08:00:00 GMT"
        );
        assert_eq!(
            parse_http_date("Sun, 25 Jan 2015 08:00:00 GMT"),
            Some(1422172800.0)
        );
        assert_eq!(
            parse_iso8601("2019-12-16T23:48:18+01:00"),
            Some(1576536498.0)
        );
    }
}
