//! The documents callers send: opening a wallet, asking for a send or a fee bump. Each is
//! checked whole, unknown fields refused, and restated in the shape the wallet core takes.

use serde_json::{json, Map, Value};

use crate::amount::{parse_btc, parse_sat};

pub const NETWORKS: &[&str] = &["mainnet", "testnet4", "signet"];
/// A local test chain, offered only with a `regtest.json` in the persistence directory, as
/// the wallet core and the node module require too.
pub const REGTEST: &str = "regtest";

pub const FAMILIES: &[&str] = &["bitcoin", "bitcoin_taproot"];
pub const OPEN_FIELDS: &[&str] = &["network", "group", "family", "birthday", "label"];
pub const SEND_FIELDS: &[&str] = &["recipients", "feeRate", "target", "sendMax"];
pub const RECIPIENT_FIELDS: &[&str] = &["address", "amountSat", "amountBtc"];
pub const BUMP_FIELDS: &[&str] = &["txid", "feeRate"];

const MAX_RECIPIENTS: usize = 100;

pub fn networks(regtest: bool) -> Vec<&'static str> {
    let mut out = NETWORKS.to_vec();
    if regtest {
        out.push(REGTEST);
    }
    out
}

pub fn is_network(n: &str, regtest: bool) -> bool {
    networks(regtest).contains(&n)
}

fn object<'a>(v: &'a Value, allowed: &[&str], what: &str) -> Result<&'a Map<String, Value>, String> {
    let o = v.as_object().ok_or_else(|| format!("{what} must be a JSON object"))?;
    if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("unknown field {k} in {what}"));
    }
    Ok(o)
}

fn doc(text: &str, what: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| format!("{what}: {e}"))
}

fn text_field(o: &Map<String, Value>, k: &str) -> Result<Option<String>, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(_) => Err(format!("{k} must be a string")),
    }
}

pub fn valid_group(g: &str) -> bool {
    g.starts_with("g_") && g.len() <= 80 && g.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn valid_day(d: &str) -> bool {
    let b = d.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenParams {
    pub network: String,
    pub group: Option<String>,
    pub family: Option<String>,
    pub birthday: Value,
    pub label: Option<String>,
}

impl OpenParams {
    /// The wallet core's `open_wallet` job parameters; the label stays here.
    pub fn core_params(&self) -> Value {
        let mut p = json!({"network": self.network});
        if let Some(g) = &self.group {
            p["group"] = json!(g);
        }
        if let Some(f) = &self.family {
            p["family"] = json!(f);
        }
        if !self.birthday.is_null() {
            p["birthday"] = self.birthday.clone();
        }
        p
    }
}

/// `{ network?, group?, family?, birthday?, label? }`. The network defaults to the active one
/// and must be it: switching networks is a separate, deliberate step.
pub fn parse_open(text: &str, active: &str, regtest: bool) -> Result<OpenParams, String> {
    let v = doc(text, "open_wallet")?;
    let o = object(&v, OPEN_FIELDS, "open_wallet")?;
    let network = text_field(o, "network")?.unwrap_or_else(|| active.to_string());
    if !is_network(&network, regtest) {
        return Err(format!("unknown network {network}"));
    }
    if network != active {
        return Err(format!("the active network is {active}; switch to {network} first"));
    }
    let group = text_field(o, "group")?.filter(|g| !g.is_empty());
    if group.as_deref().is_some_and(|g| !valid_group(g)) {
        return Err("group must be a keystore group (g_…)".into());
    }
    let family = text_field(o, "family")?.filter(|f| !f.is_empty());
    if family.as_deref().is_some_and(|f| !FAMILIES.contains(&f)) {
        return Err("family must be bitcoin (native segwit) or bitcoin_taproot".into());
    }
    if group.is_none() && family.is_none() {
        return Err("name a group to reopen, or a family to pick, create or restore one".into());
    }
    let birthday = match o.get("birthday") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Number(n)) if n.as_u64().is_some_and(|h| h <= u32::MAX as u64) => Value::Number(n.clone()),
        Some(Value::String(s)) if valid_day(s.trim()) => json!(s.trim()),
        Some(_) => return Err("birthday must be a block height or a YYYY-MM-DD day".into()),
    };
    let label = text_field(o, "label")?;
    if label.as_ref().is_some_and(|l| l.chars().count() > 64 || l.chars().any(char::is_control)) {
        return Err("label must be at most 64 printable characters".into());
    }
    Ok(OpenParams { network, group, family, birthday, label })
}

/// A recipient whose address the core must check, and whether it needs the URI's amount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToCheck {
    pub index: usize,
    pub address: String,
    pub needs_uri_amount: bool,
}

fn is_uri(a: &str) -> bool {
    a.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("bitcoin:"))
}

fn fee_rate(v: &Value) -> Result<f64, String> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|r| r.is_finite() && *r > 0.0 && *r <= 100_000.0)
        .ok_or_else(|| "feeRate must be sat/vB above zero".to_string())
}

/// `{ recipients: [{ address, amountSat | amountBtc }], feeRate? | target?, sendMax? }` as the core's
/// `build_send` parameters, plus the addresses to check first; a BIP21 URI may carry the amount.
pub fn parse_send(text: &str) -> Result<(Value, Vec<ToCheck>), String> {
    let v = doc(text, "request_send")?;
    let o = object(&v, SEND_FIELDS, "request_send")?;
    let list = o.get("recipients").and_then(Value::as_array).ok_or("recipients is required")?;
    if list.is_empty() || list.len() > MAX_RECIPIENTS {
        return Err(format!("between 1 and {MAX_RECIPIENTS} recipients"));
    }
    let send_max = match o.get("sendMax") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("sendMax must be true or false".into()),
    };
    if send_max && list.len() != 1 {
        return Err("sendMax pays exactly one recipient".into());
    }
    let mut recipients = vec![];
    let mut checks = vec![];
    for (i, r) in list.iter().enumerate() {
        let ro = object(r, RECIPIENT_FIELDS, "a recipient")?;
        let address = text_field(ro, "address")?.filter(|a| !a.is_empty()).ok_or("each recipient needs an address")?;
        if address.len() > 2048 {
            return Err("an address is too long".into());
        }
        let sat = match (ro.get("amountSat").filter(|x| !x.is_null()), ro.get("amountBtc").filter(|x| !x.is_null())) {
            (Some(_), Some(_)) => return Err("give amountSat or amountBtc, not both".into()),
            (Some(Value::String(s)), None) => Some(parse_sat(s)?),
            (Some(Value::Number(n)), None) => Some(n.as_u64().ok_or("amountSat must be a whole number of satoshis")?),
            (Some(_), None) => return Err("amountSat must be a decimal string of satoshis".into()),
            (None, Some(Value::String(s))) => Some(parse_btc(s)?),
            (None, Some(_)) => return Err("amountBtc must be a decimal string".into()),
            (None, None) => None,
        };
        if sat == Some(0) {
            return Err("an amount must be above zero".into());
        }
        if send_max && sat.is_some() {
            return Err("sendMax takes no amount: it pays everything spendable, less the fee".into());
        }
        if !send_max && sat.is_none() && !is_uri(&address) {
            return Err("every recipient needs an amount".into());
        }
        let mut out = json!({"address": address});
        if let Some(s) = sat {
            out["amountSat"] = json!(s.to_string());
        }
        checks.push(ToCheck { index: i, address: address.clone(), needs_uri_amount: !send_max && sat.is_none() });
        recipients.push(out);
    }
    let mut core = json!({"recipients": recipients});
    match (o.get("feeRate").filter(|x| !x.is_null()), o.get("target").filter(|x| !x.is_null())) {
        (Some(_), Some(_)) => return Err("give feeRate or target, not both".into()),
        (Some(r), None) => core["feeRate"] = json!(fee_rate(r)?),
        (None, Some(Value::String(t))) if matches!(t.as_str(), "fast" | "normal" | "slow") => core["target"] = json!(t),
        (None, Some(Value::Number(n))) if n.as_u64().is_some_and(|b| (1..=1008).contains(&b)) => core["target"] = json!(n),
        (None, Some(_)) => return Err("target must be fast, normal, slow or a number of blocks".into()),
        (None, None) => {}
    }
    if send_max {
        core["sendMax"] = json!(true);
    }
    Ok((core, checks))
}

/// `{ txid, feeRate }`, the core's `bump_fee` parameters.
pub fn parse_bump(text: &str) -> Result<Value, String> {
    let v = doc(text, "request_fee_bump")?;
    let o = object(&v, BUMP_FIELDS, "request_fee_bump")?;
    let txid = text_field(o, "txid")?.unwrap_or_default().to_ascii_lowercase();
    if !is_txid(&txid) {
        return Err("txid must be 64 hex characters".into());
    }
    let rate = fee_rate(o.get("feeRate").ok_or("feeRate is required, in sat/vB")?)?;
    Ok(json!({"txid": txid, "feeRate": rate}))
}

pub fn is_txid(t: &str) -> bool {
    t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regtest_only_when_enabled() {
        assert!(is_network("mainnet", false) && is_network("testnet4", false) && is_network("signet", false));
        assert!(!is_network("regtest", false) && is_network("regtest", true));
        assert!(!is_network("testnet", true) && !is_network("main", true), "the core's names only");
    }

    #[test]
    fn opening_names_a_group_or_a_family_on_the_active_network() {
        let p = parse_open(r#"{"family":"bitcoin"}"#, "signet", false).unwrap();
        assert_eq!(p.core_params(), json!({"network": "signet", "family": "bitcoin"}));
        let p = parse_open(r#"{"network":"signet","group":"g_ab12","birthday":290000,"label":"Savings"}"#, "signet", false).unwrap();
        assert_eq!(p.core_params(), json!({"network": "signet", "group": "g_ab12", "birthday": 290000}), "the label stays here");
        assert_eq!(p.label.as_deref(), Some("Savings"));
        assert!(parse_open(r#"{"family":"bitcoin","birthday":"2026-10-01"}"#, "mainnet", false).is_ok());
        for (bad, why) in [
            (r#"{"network":"signet","family":"bitcoin"}"#, "switch to signet first"),
            (r#"{}"#, "name a group"),
            (r#"{"family":"evm"}"#, "family must be"),
            (r#"{"group":"../x"}"#, "keystore group"),
            (r#"{"family":"bitcoin","password":"x"}"#, "unknown field password"),
            (r#"{"family":"bitcoin","birthday":"soon"}"#, "birthday must be"),
            (r#"{"family":"bitcoin","birthday":-1}"#, "birthday must be"),
            (r#"{"network":"regtest","family":"bitcoin"}"#, "unknown network"),
            (r#"[]"#, "JSON object"),
        ] {
            let e = parse_open(bad, "mainnet", false).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        assert!(parse_open(r#"{"network":"regtest","family":"bitcoin"}"#, "regtest", true).is_ok());
    }

    #[test]
    fn a_send_request_is_restated_in_satoshis() {
        let (core, checks) = parse_send(r#"{"recipients":[{"address":"bcrt1qa","amountBtc":"0.0006"},{"address":"bcrt1qb","amountSat":"1000"}],"target":"fast"}"#).unwrap();
        assert_eq!(core, json!({"recipients": [{"address": "bcrt1qa", "amountSat": "60000"}, {"address": "bcrt1qb", "amountSat": "1000"}], "target": "fast"}));
        assert_eq!(checks.len(), 2);
        let (core, _) = parse_send(r#"{"recipients":[{"address":"bcrt1qa","amountSat":5000}],"feeRate":2.5}"#).unwrap();
        assert_eq!(core["feeRate"], 2.5);
        let (core, checks) = parse_send(r#"{"recipients":[{"address":"bcrt1qa"}],"sendMax":true}"#).unwrap();
        assert_eq!(core["sendMax"], true);
        assert!(!checks[0].needs_uri_amount);
        let (_, checks) = parse_send(r#"{"recipients":[{"address":"BITCOIN:bcrt1qa?amount=0.1"}]}"#).unwrap();
        assert!(checks[0].needs_uri_amount, "the URI's amount must be there");
    }

    #[test]
    fn a_bad_send_request_is_refused_whole() {
        for (bad, why) in [
            (r#"{"recipients":[]}"#, "between 1"),
            (r#"{"recipients":[{"address":"a"}]}"#, "needs an amount"),
            (r#"{"recipients":[{"address":"a","amountSat":"0"}]}"#, "above zero"),
            (r#"{"recipients":[{"address":"a","amountSat":"1","amountBtc":"1"}]}"#, "not both"),
            (r#"{"recipients":[{"address":"a","amountBtc":"0.000000001"}]}"#, "8 decimal places"),
            (r#"{"recipients":[{"address":"a","amountSat":"1.5"}]}"#, "whole number"),
            (r#"{"recipients":[{"address":"a","amountSat":-1}]}"#, "whole number"),
            (r#"{"recipients":[{"address":"a","amountSat":"1"}],"feeRate":0}"#, "above zero"),
            (r#"{"recipients":[{"address":"a","amountSat":"1"}],"feeRate":1,"target":"fast"}"#, "not both"),
            (r#"{"recipients":[{"address":"a","amountSat":"1"}],"target":"asap"}"#, "target must be"),
            (r#"{"recipients":[{"address":"a","amountSat":"1"}],"sendMax":true}"#, "takes no amount"),
            (r#"{"recipients":[{"address":"a"},{"address":"b"}],"sendMax":true}"#, "exactly one"),
            (r#"{"recipients":[{"address":"a","amountSat":"1","memo":"x"}]}"#, "unknown field memo"),
            (r#"{"recipients":[{"address":"a","amountSat":"1"}],"psbt":"x"}"#, "unknown field psbt"),
            (r#"{"recipients":[{"amountSat":"1"}]}"#, "needs an address"),
        ] {
            let e = parse_send(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn a_fee_bump_names_a_txid_and_a_rate() {
        let t = "AB".repeat(32);
        assert_eq!(parse_bump(&format!(r#"{{"txid":"{t}","feeRate":"12"}}"#)).unwrap(), json!({"txid": "ab".repeat(32), "feeRate": 12.0}));
        assert!(parse_bump(r#"{"txid":"nope","feeRate":5}"#).is_err());
        assert!(parse_bump(&format!(r#"{{"txid":"{t}"}}"#)).is_err());
        assert!(parse_bump(&format!(r#"{{"txid":"{t}","feeRate":5,"previewId":"p1"}}"#)).is_err());
    }
}
