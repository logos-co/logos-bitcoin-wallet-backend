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

const BALANCE_KEYS: [&str; 6] = ["confirmed", "waiting", "unchecked", "pending", "sent", "immature"];

pub fn balance_view(mut b: Value) -> Value {
    for k in BALANCE_KEYS {
        let btc = btc_of(b[k].as_str().unwrap_or_default());
        b[format!("{k}Btc")] = json!(btc);
    }
    if b["tooSmall"].is_object() {
        let btc = btc_of(b["tooSmall"]["sat"].as_str().unwrap_or_default());
        b["tooSmall"]["btc"] = json!(btc);
    }
    // Coins past the gap limit: outside confirmed, waiting and pending, and still spendable.
    b["beyondGapBtc"] = json!(btc_of(b["beyondGap"]["sat"].as_str().unwrap_or_default()));
    b["beyondGapConfirmedBtc"] = json!(btc_of(b["beyondGap"]["confirmedSat"].as_str().unwrap_or_default()));
    b
}

/// The amounts balance_changed compares, read or not.
pub fn balance_digest(b: &Value) -> Option<String> {
    if b["ok"] != true || b["busy"] == true {
        return None;
    }
    Some(json!([b["confirmed"], b["waiting"], b["unchecked"], b["pending"], b["sent"], b["immature"], b["tooSmall"], b["beyondGap"]]).to_string())
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
    fn restore_fields_pass_through_and_unchecked_gains_its_twin() {
        let b = balance_view(json!({"ok": true, "busy": false, "confirmed": "100", "waiting": "0", "unchecked": "2500", "pending": "0",
                                    "sent": "0", "immature": "0", "tooSmall": {"count": 0, "sat": "0"}}));
        assert_eq!((b["unchecked"].clone(), b["uncheckedBtc"].clone()), (json!("2500"), json!("0.00002500")));
        let before = balance_digest(&b);
        let mut more = b.clone();
        more["unchecked"] = json!("2600");
        assert_ne!(balance_digest(&more), before, "a change in unchecked is a balance change");
        let busy = balance_view(json!({"ok": true, "busy": true, "unchecked": ""}));
        assert_eq!(busy["uncheckedBtc"], "", "unread stays unread");
        let st = json!({"state": "ready", "authority": {"height": 10, "kind": "kyoto", "base": 0},
                        "kyoto": {"rescans": 1, "window": {"from": 0, "to": 10}, "lastConflict": {"at": 5, "peers": 2}, "quorum": 2, "onePeerDecides": false},
                        "node": {"rescans": 0, "window": null}, "walletTip": 10});
        let v = status_view(st.clone(), "regtest", None, None);
        for (k, sub) in [("authority", "base"), ("kyoto", "rescans"), ("kyoto", "window"), ("kyoto", "lastConflict"), ("kyoto", "quorum"), ("kyoto", "onePeerDecides"), ("node", "rescans"), ("node", "window")] {
            assert_eq!(v[k][sub], st[k][sub], "{k}.{sub} passes unchanged");
        }
        let h = history_view(json!({"ok": true, "rows": [{"txid": "a", "state": "unchecked", "amountSat": "2500", "feeSat": null, "confirmedVia": "b", "height": null}]}));
        assert_eq!((h["rows"][0]["state"].clone(), h["rows"][0]["confirmedVia"].clone(), h["rows"][0]["height"].clone()), (json!("unchecked"), json!("b"), Value::Null));
        assert_eq!(h["rows"][0]["amountBtc"], "0.00002500");
    }

    #[test]
    fn coins_beyond_the_gap_pass_through_with_their_twin() {
        let b = balance_view(json!({"ok": true, "busy": false, "confirmed": "100000", "waiting": "0", "pending": "0", "sent": "0", "immature": "0",
                                    "tooSmall": {"count": 0, "sat": "0"}, "beyondGap": {"count": 2, "sat": "12345"}}));
        assert_eq!((b["beyondGap"].clone(), b["beyondGapBtc"].clone()), (json!({"count": 2, "sat": "12345"}), json!("0.00012345")));
        let before = balance_digest(&b);
        let mut more = b.clone();
        more["beyondGap"] = json!({"count": 3, "sat": "22345"});
        assert_ne!(balance_digest(&more), before, "a change beyond the gap is a balance change");
        let busy = balance_view(json!({"ok": true, "busy": true, "beyondGap": {"count": null, "sat": ""}}));
        assert_eq!((busy["beyondGapBtc"].clone(), busy["beyondGap"]["count"].clone()), (json!(""), Value::Null), "unread stays unread");
        assert_eq!(balance_view(json!({"ok": true, "confirmed": "1"}))["beyondGapBtc"], "", "a core that doesn't say");
        let v = status_view(json!({"state": "ready", "gapLimit": 200, "authority": {"height": 10}, "walletTip": 10}), "signet", None, None);
        assert_eq!(v["gapLimit"], 200);
        let h = history_view(json!({"ok": true, "rows": [{"txid": "a", "amountSat": "5000", "feeSat": null, "beyondGap": true}, {"txid": "b", "amountSat": "1"}]}));
        assert_eq!((h["rows"][0]["beyondGap"].clone(), h["rows"][1].get("beyondGap")), (json!(true), None));
    }

    #[test]
    fn the_confirmed_part_beyond_the_gap_gains_its_twin() {
        let b = balance_view(json!({"ok": true, "busy": false, "confirmed": "100000", "beyondGap": {"count": 3, "sat": "30000", "confirmedSat": "20000"}}));
        assert_eq!(b["beyondGap"]["confirmedSat"], "20000", "passed through");
        assert_eq!((b["beyondGapBtc"].clone(), b["beyondGapConfirmedBtc"].clone()), (json!("0.00030000"), json!("0.00020000")));
        let mut more = b.clone();
        more["beyondGap"]["confirmedSat"] = json!("30000");
        assert_ne!(balance_digest(&more), balance_digest(&b), "a confirmation beyond the gap is a balance change");
        // The core's busy reply, as it gives it.
        let busy = balance_view(json!({"ok": true, "busy": true, "confirmed": "", "waiting": "", "pending": "", "sent": "", "immature": "",
                                       "tooSmall": {"count": null, "sat": ""}, "beyondGap": {"count": null, "sat": "", "confirmedSat": ""}}));
        assert_eq!((busy["beyondGapBtc"].clone(), busy["beyondGapConfirmedBtc"].clone()), (json!(""), json!("")), "unread stays unread");
        assert_eq!(busy["beyondGap"], json!({"count": null, "sat": "", "confirmedSat": ""}));
        assert_eq!(balance_digest(&busy), None);
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
