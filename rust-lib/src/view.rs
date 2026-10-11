//! The core's replies restated for the app: display amounts and a sync percent added, and
//! nothing invented. An amount the core didn't read stays empty, never zero.

use serde_json::{json, Value};

use crate::amount::btc_of;
use crate::registry::WalletEntry;

/// Core states in which a wallet is open, or being opened or closed.
pub const OPEN_STATES: &[&str] = &["awaiting_open", "opening", "syncing", "ready", "closing"];

pub fn wallet_open(status: &Value) -> bool {
    status["state"].as_str().is_some_and(|s| OPEN_STATES.contains(&s))
}

/// The network of the wallet the core has open, if any.
pub fn open_network(status: &Value) -> Option<String> {
    if !wallet_open(status) {
        return None;
    }
    status["network"].as_str().map(String::from)
}

/// An open wallet owns the session's network; otherwise the active one applies.
pub fn effective_network(status: Option<&Value>, active: &str) -> String {
    status.and_then(open_network).unwrap_or_else(|| active.to_string())
}

/// Progress to the authority's tip from the birthday, or from `rescanFrom` while a rescan runs.
/// None while a height is unknown: a percentage of a chain nobody read is no answer.
pub fn sync_percent(status: &Value) -> Option<u64> {
    let tip = status["authority"]["height"].as_u64().filter(|t| *t > 0)?;
    if status["rescanFrom"].as_u64().is_some() {
        // Kyoto restarts at rescanFrom and reports its share done; other scans report no height.
        let kyoto = status["authority"]["kind"] == "kyoto";
        return status["kyoto"]["progress"].as_f64().filter(|p| kyoto && (0.0..=1.0).contains(p)).map(|p| (p * 100.0).floor() as u64);
    }
    let wallet = status["walletTip"].as_u64()?;
    if wallet >= tip {
        return Some(100);
    }
    let start = status["birthday"].as_u64().filter(|b| *b < tip).unwrap_or(0);
    Some(wallet.saturating_sub(start) * 100 / (tip - start))
}

pub fn status_view(mut status: Value, active: &str, entry: Option<&WalletEntry>, open_send: Option<&str>) -> Value {
    if let Some(p) = sync_percent(&status) {
        status["syncPercent"] = json!(p);
    }
    status["activeNetwork"] = json!(active);
    status["label"] = entry.map(|e| json!(e.label)).unwrap_or(Value::Null);
    status["openSend"] = open_send.map(|s| json!(s)).unwrap_or(Value::Null);
    status
}

/// What sync_progress carries.
pub fn sync_view(status: &Value) -> Value {
    json!({
        "state": status["state"], "network": status["network"], "walletTip": status["walletTip"],
        "authorityHeight": status["authority"]["height"], "tipAgeSecs": status["authority"]["tipAgeSecs"],
        "percent": sync_percent(status), "synced": status["synced"],
    })
}

const BALANCE_KEYS: [&str; 5] = ["confirmed", "waiting", "pending", "sent", "immature"];

pub fn balance_view(mut b: Value) -> Value {
    for k in BALANCE_KEYS {
        let btc = btc_of(b[k].as_str().unwrap_or_default());
        b[format!("{k}Btc")] = json!(btc);
    }
    if b["tooSmall"].is_object() {
        let btc = btc_of(b["tooSmall"]["sat"].as_str().unwrap_or_default());
        b["tooSmall"]["btc"] = json!(btc);
    }
    b
}

/// The amounts balance_changed compares, read or not.
pub fn balance_digest(b: &Value) -> Option<String> {
    if b["ok"] != true || b["busy"] == true {
        return None;
    }
    Some(json!([b["confirmed"], b["waiting"], b["pending"], b["sent"], b["immature"], b["tooSmall"]]).to_string())
}

pub fn history_view(mut h: Value) -> Value {
    if let Some(rows) = h["rows"].as_array_mut() {
        for r in rows {
            let amount = btc_of(r["amountSat"].as_str().unwrap_or_default());
            let fee = btc_of(r["feeSat"].as_str().unwrap_or_default());
            r["amountBtc"] = json!(amount);
            r["feeBtc"] = json!(fee);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_heights_are_no_percentage() {
        assert_eq!(sync_percent(&json!({"authority": {"height": null}, "walletTip": 5})), None);
        assert_eq!(sync_percent(&json!({"authority": {"height": 100}})), None);
        assert_eq!(sync_percent(&json!({"authority": {"height": 0}, "walletTip": 0})), None);
        assert_eq!(sync_percent(&json!({"authority": {"height": 200}, "walletTip": 200})), Some(100));
        assert_eq!(sync_percent(&json!({"authority": {"height": 200}, "walletTip": 50})), Some(25));
        // Counted from the birthday, not from genesis.
        assert_eq!(sync_percent(&json!({"authority": {"height": 1000}, "walletTip": 950, "birthday": 900})), Some(50));
        assert_eq!(sync_percent(&json!({"authority": {"height": 1000}, "walletTip": 0, "birthday": 900})), Some(0));
        // A rescan: the wallet's tip stays put, so only Kyoto's share done, counted from rescanFrom, says anything.
        let k = json!({"authority": {"height": 1000, "kind": "kyoto"}, "walletTip": 1000, "rescanFrom": 700, "kyoto": {"progress": 0.42}});
        assert_eq!(sync_percent(&k), Some(42));
        let event = json!({"authority": {"height": 1000, "kind": "kyoto"}, "rescanFrom": 700, "kyoto": {"progress": 1.0}});
        assert_eq!(sync_percent(&event), Some(100), "a state event carries no walletTip");
        let e = json!({"authority": {"height": 1000, "kind": "electrum_headers"}, "walletTip": 1000, "rescanFrom": 700});
        assert_eq!(sync_percent(&e), None, "a full scan reports no height");
        let done = json!({"authority": {"height": 1000, "kind": "electrum_headers"}, "walletTip": 1000, "rescanFrom": null});
        assert_eq!(sync_percent(&done), Some(100));
    }

    #[test]
    fn the_open_wallet_owns_the_network() {
        let open = json!({"state": "syncing", "network": "signet"});
        assert!(wallet_open(&open));
        assert_eq!(effective_network(Some(&open), "mainnet"), "signet");
        for st in ["no_wallet", "failed"] {
            assert_eq!(effective_network(Some(&json!({"state": st, "network": "signet"})), "mainnet"), "mainnet");
        }
        assert!(wallet_open(&json!({"state": "awaiting_open"})), "an open the person is deciding locks too");
        assert_eq!(effective_network(None, "mainnet"), "mainnet");
    }

    #[test]
    fn a_busy_balance_stays_unread() {
        let b = balance_view(json!({"ok": true, "busy": true, "confirmed": "", "waiting": "", "pending": "", "sent": "", "immature": "",
                                    "tooSmall": {"count": null, "sat": ""}}));
        for k in BALANCE_KEYS {
            assert_eq!(b[format!("{k}Btc")], "", "{k}");
        }
        assert_eq!(b["tooSmall"]["btc"], "");
        assert_eq!(balance_digest(&b), None);
        let b = balance_view(json!({"ok": true, "busy": false, "confirmed": "150000000", "waiting": "0", "pending": "2100",
                                    "sent": "0", "immature": "0", "tooSmall": {"count": 1, "sat": "300"}}));
        assert_eq!((b["confirmedBtc"].clone(), b["pendingBtc"].clone(), b["tooSmall"]["btc"].clone()),
                   (json!("1.50000000"), json!("0.00002100"), json!("0.00000300")));
        assert!(balance_digest(&b).is_some());
    }

    #[test]
    fn history_rows_gain_display_amounts() {
        let h = history_view(json!({"ok": true, "rows": [{"txid": "a", "amountSat": "60000", "feeSat": "141"}, {"txid": "b", "amountSat": "5", "feeSat": null}]}));
        assert_eq!(h["rows"][0]["amountBtc"], "0.00060000");
        assert_eq!(h["rows"][0]["feeBtc"], "0.00000141");
        assert_eq!(h["rows"][1]["feeBtc"], "", "a fee the core couldn't compute stays empty");
    }

    #[test]
    fn status_gains_percent_label_and_the_open_send() {
        let e = WalletEntry { label: "Savings".into(), ..Default::default() };
        let v = status_view(json!({"state": "ready", "authority": {"height": 10}, "walletTip": 10}), "signet", Some(&e), Some("s3"));
        assert_eq!((v["syncPercent"].clone(), v["label"].clone(), v["openSend"].clone()), (json!(100), json!("Savings"), json!("s3")));
        let v = status_view(json!({"state": "no_wallet", "authority": {"height": null}}), "signet", None, None);
        assert!(v.get("syncPercent").is_none());
    }
}
