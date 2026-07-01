use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix milliseconds を内部表現とする Timestamp Value Object。
///
/// - serde: ISO 8601文字列 `YYYY-MM-DDTHH:MM:SS.sssZ`
/// - Copy + Clone + Eq + Ord + Hash
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// 現在時刻から Timestamp を生成する。
    ///
    /// システムクロックが UNIX_EPOCH より前を指している場合は 0 を返す。
    pub fn now() -> Self {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self(millis)
    }

    /// Unix milliseconds から Timestamp を生成する（テスト用途を含む）。
    pub fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// Unix milliseconds を返す。
    pub fn as_millis(&self) -> i64 {
        self.0
    }

    /// ISO 8601 文字列 `YYYY-MM-DDTHH:MM:SS.sssZ` に変換する。
    pub fn to_iso8601(&self) -> String {
        millis_to_iso8601(self.0)
    }

    /// ISO 8601 文字列から Timestamp をパースする。
    ///
    /// # Errors
    ///
    /// フォーマットが不正な場合は `Err(String)` を返す。
    pub fn parse_iso8601(s: &str) -> Result<Self, String> {
        iso8601_to_millis(s).map(Self)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_iso8601())
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_iso8601())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Timestamp::parse_iso8601(&s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// 内部ユーティリティ: millis ↔ ISO 8601
// ---------------------------------------------------------------------------

/// Unix millis → ISO 8601 文字列 `YYYY-MM-DDTHH:MM:SS.sssZ`
fn millis_to_iso8601(millis: i64) -> String {
    // ミリ秒部分
    let ms = millis.rem_euclid(1000) as u32;
    // 秒単位
    let total_secs = millis.div_euclid(1000);

    let (year, month, day, hour, minute, second) = secs_to_datetime(total_secs);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, ms
    )
}

/// ISO 8601 文字列 → Unix millis
///
/// 受け入れフォーマット: `YYYY-MM-DDTHH:MM:SS.sssZ`
fn iso8601_to_millis(s: &str) -> Result<i64, String> {
    // 最低限のバリデーション: "YYYY-MM-DDTHH:MM:SS" (19文字) + 任意の`.sssZ`
    if s.len() < 20 {
        return Err(format!("too short: {s}"));
    }
    let bytes = s.as_bytes();

    // YYYY
    let year = parse_digits(bytes, 0, 4)?;
    expect_byte(bytes, 4, b'-')?;
    // MM
    let month = parse_digits(bytes, 5, 2)?;
    expect_byte(bytes, 7, b'-')?;
    // DD
    let day = parse_digits(bytes, 8, 2)?;
    expect_byte(bytes, 10, b'T')?;
    // HH
    let hour = parse_digits(bytes, 11, 2)?;
    expect_byte(bytes, 13, b':')?;
    // MM
    let minute = parse_digits(bytes, 14, 2)?;
    expect_byte(bytes, 16, b':')?;
    // SS
    let second = parse_digits(bytes, 17, 2)?;

    // ミリ秒(.sss)
    let ms: i64 = if s.len() > 19 && bytes[19] == b'.' {
        // 小数部: 最大3桁読み取り
        let frac_start = 20usize;
        let frac_end = s[frac_start..]
            .find('Z')
            .map(|i| i + frac_start)
            .unwrap_or(s.len());
        let frac_str = &s[frac_start..frac_end];
        let frac_digits = frac_str.len().min(3);
        let frac: i64 = frac_str[..frac_digits]
            .parse()
            .map_err(|_| format!("invalid fraction: {frac_str}"))?;
        // 3桁未満なら 10^(3 - digits) を掛けて補正
        let pad = 3u32.saturating_sub(frac_digits as u32);
        frac * 10i64.pow(pad)
    } else {
        0
    };

    let total_secs = datetime_to_secs(
        year,
        month as u32,
        day as u32,
        hour as u32,
        minute as u32,
        second as u32,
    )
    .ok_or_else(|| format!("invalid datetime: {s}"))?;

    Ok(total_secs * 1000 + ms)
}

fn parse_digits(bytes: &[u8], offset: usize, len: usize) -> Result<i64, String> {
    let slice = bytes
        .get(offset..offset + len)
        .ok_or_else(|| format!("out of bounds at offset {offset}"))?;
    let s = std::str::from_utf8(slice).map_err(|e| e.to_string())?;
    s.parse::<i64>()
        .map_err(|e| format!("parse error at offset {offset}: {e}"))
}

fn expect_byte(bytes: &[u8], offset: usize, expected: u8) -> Result<(), String> {
    match bytes.get(offset) {
        Some(&b) if b == expected => Ok(()),
        Some(&b) => Err(format!(
            "expected '{}' at offset {offset}, got '{}'",
            expected as char, b as char
        )),
        None => Err(format!("out of bounds at offset {offset}")),
    }
}

// ---------------------------------------------------------------------------
// グレゴリオ暦変換
// ---------------------------------------------------------------------------

/// Unix epoch 秒 → (year, month, day, hour, minute, second)
///
/// 参考: https://howardhinnant.github.io/date_algorithms.html (civil_from_days)
fn secs_to_datetime(total_secs: i64) -> (i64, u8, u8, u8, u8, u8) {
    let second = total_secs.rem_euclid(60) as u8;
    let total_mins = total_secs.div_euclid(60);
    let minute = total_mins.rem_euclid(60) as u8;
    let total_hours = total_mins.div_euclid(60);
    let hour = total_hours.rem_euclid(24) as u8;
    let days = total_hours.div_euclid(24); // days since Unix epoch (1970-01-01)

    let (year, month, day) = civil_from_days(days);
    (year, month, day, hour, minute, second)
}

/// (year, month, day, hour, minute, second) → Unix epoch 秒
fn datetime_to_secs(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<i64> {
    let days = days_from_civil(year, month, day)?;
    Some(days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64)
}

/// Hinnant algorithm: days since epoch → (year, month, day)
fn civil_from_days(z: i64) -> (i64, u8, u8) {
    let z = z + 719468;
    let era: i64 = if z >= 0 { z } else { z - 146096 }.div_euclid(146097);
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u8, d as u8)
}

/// (year, month, day) → days since epoch
fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 }.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe as i64 - 719468)
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_millis_and_as_millis_roundtrip() {
        let ts = Timestamp::from_millis(1_700_000_000_000);
        assert_eq!(ts.as_millis(), 1_700_000_000_000);
    }

    #[test]
    fn test_display_iso8601_known_value() {
        // 2023-11-14T22:13:20.000Z = 1_700_000_000 seconds
        let ts = Timestamp::from_millis(1_700_000_000_000);
        let s = ts.to_string();
        assert_eq!(s, "2023-11-14T22:13:20.000Z");
    }

    #[test]
    fn test_display_iso8601_with_millis() {
        // 1_700_000_000_123 ms = ...000.123Z
        let ts = Timestamp::from_millis(1_700_000_000_123);
        let s = ts.to_string();
        assert_eq!(s, "2023-11-14T22:13:20.123Z");
    }

    #[test]
    fn test_parse_iso8601_roundtrip() {
        let original = 1_700_000_000_456i64;
        let ts = Timestamp::from_millis(original);
        let s = ts.to_string();
        let parsed = Timestamp::parse_iso8601(&s).expect("parse should succeed");
        assert_eq!(parsed.as_millis(), original);
    }

    #[test]
    fn test_parse_iso8601_epoch() {
        // Unix epoch: 1970-01-01T00:00:00.000Z = 0 ms
        let ts = Timestamp::parse_iso8601("1970-01-01T00:00:00.000Z").expect("parse epoch");
        assert_eq!(ts.as_millis(), 0);
    }

    #[test]
    fn test_serde_serialize() {
        let ts = Timestamp::from_millis(1_700_000_000_000);
        let json = serde_json::to_string(&ts).expect("serialize");
        assert_eq!(json, r#""2023-11-14T22:13:20.000Z""#);
    }

    #[test]
    fn test_serde_deserialize() {
        let json = r#""2023-11-14T22:13:20.000Z""#;
        let ts: Timestamp = serde_json::from_str(json).expect("deserialize");
        assert_eq!(ts.as_millis(), 1_700_000_000_000);
    }

    #[test]
    fn test_serde_roundtrip() {
        let ts = Timestamp::from_millis(1_234_567_890_123);
        let json = serde_json::to_string(&ts).expect("serialize");
        let ts2: Timestamp = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ts.as_millis(), ts2.as_millis());
    }

    #[test]
    fn test_ordering() {
        let ts1 = Timestamp::from_millis(1000);
        let ts2 = Timestamp::from_millis(2000);
        assert!(ts1 < ts2);
    }

    #[test]
    fn test_now_is_recent() {
        let ts = Timestamp::now();
        // 2020-01-01 = 1_577_836_800_000 ms
        assert!(ts.as_millis() > 1_577_836_800_000);
    }

    #[test]
    fn test_parse_invalid_returns_err() {
        assert!(Timestamp::parse_iso8601("not-a-date").is_err());
        assert!(Timestamp::parse_iso8601("").is_err());
    }
}
