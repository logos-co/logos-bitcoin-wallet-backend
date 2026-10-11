//! Amounts: satoshis as decimal strings on the wire, BTC strings for display. Exact, no floats.

pub const SAT_PER_BTC: u64 = 100_000_000;
/// No amount can exceed the 21 million BTC that will ever exist.
pub const MAX_SAT: u64 = 21_000_000 * SAT_PER_BTC;

/// Satoshis as BTC with all 8 decimals: 123_450_000 → "1.23450000".
pub fn format_btc(sat: u64) -> String {
    format!("{}.{:08}", sat / SAT_PER_BTC, sat % SAT_PER_BTC)
}

/// A decimal string of satoshis: digits only, at most the supply.
pub fn parse_sat(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err("not a whole number of satoshis".into());
    }
    let v: u64 = t.parse().map_err(|_| "more than the bitcoin supply".to_string())?;
    if v > MAX_SAT {
        return Err("more than the bitcoin supply".into());
    }
    Ok(v)
}

/// The display form of a satoshi string, or "" when it isn't one (busy reads stay empty).
pub fn btc_of(sat: &str) -> String {
    parse_sat(sat).map(format_btc).unwrap_or_default()
}

/// A BTC decimal string as satoshis. At most 8 decimals, refused rather than rounded; no
/// sign, exponent, separator or space inside.
pub fn parse_btc(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && frac.is_empty()) || !digits(whole) || !digits(frac) {
        return Err("not a BTC amount".into());
    }
    if frac.len() > 8 {
        return Err("at most 8 decimal places".into());
    }
    let w: u64 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| "more than the bitcoin supply".to_string())? };
    let f: u64 = format!("{frac:0<8}").parse().map_err(|_| "not a BTC amount".to_string())?;
    w.checked_mul(SAT_PER_BTC)
        .and_then(|v| v.checked_add(f))
        .filter(|v| *v <= MAX_SAT)
        .ok_or_else(|| "more than the bitcoin supply".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_exact_to_eight_places() {
        assert_eq!(format_btc(0), "0.00000000");
        assert_eq!(format_btc(1), "0.00000001");
        assert_eq!(format_btc(123_450_000), "1.23450000");
        assert_eq!(format_btc(SAT_PER_BTC), "1.00000000");
        assert_eq!(format_btc(MAX_SAT), "21000000.00000000");
        assert_eq!(btc_of("2100"), "0.00002100");
        assert_eq!(btc_of(""), "", "an unread amount stays unread, never zero");
        assert_eq!(btc_of("-1"), "");
    }

    #[test]
    fn parse_refuses_precision_loss_and_odd_spellings() {
        assert_eq!(parse_btc("1.2345").unwrap(), 123_450_000);
        assert_eq!(parse_btc(".5").unwrap(), 50_000_000);
        assert_eq!(parse_btc("1.").unwrap(), SAT_PER_BTC);
        assert_eq!(parse_btc(" 0.00000001 ").unwrap(), 1);
        assert_eq!(parse_btc("21000000").unwrap(), MAX_SAT);
        assert_eq!(parse_btc("0.000000001").unwrap_err(), "at most 8 decimal places", "refused, not rounded");
        for bad in ["", ".", "-1", "+1", "1e3", "1,5", "1 000", "0x10", "1.2.3", "₿1", "NaN"] {
            assert!(parse_btc(bad).is_err(), "{bad:?}");
        }
        assert!(parse_btc("21000000.00000001").is_err());
        assert!(parse_btc("99999999999999999999").is_err());
    }

    #[test]
    fn satoshi_strings() {
        assert_eq!(parse_sat("0").unwrap(), 0);
        assert_eq!(parse_sat(" 2100 ").unwrap(), 2100);
        for bad in ["", "-5", "1.5", "1e3", "abc", "2100000000000001"] {
            assert!(parse_sat(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn round_trips() {
        for sat in [0, 1, 99_999_999, SAT_PER_BTC, 123_456_789, MAX_SAT] {
            assert_eq!(parse_btc(&format_btc(sat)).unwrap(), sat);
        }
    }
}
