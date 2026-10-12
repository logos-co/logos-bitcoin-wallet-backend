//! Wallets opened before, per (network, group), kept in wallets.json for the app to offer a
//! reopen. Never a secret: no descriptor, key, fingerprint or password.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletEntry {
    pub group: String,
    pub family: String,
    pub network: String,
    #[serde(default)]
    pub label: String,
    /// `{ height, source }` as the wallet core resolved it, or null before the first open.
    #[serde(default)]
    pub birthday: Value,
    /// The wallet core's gap limit for it, once known.
    #[serde(default)]
    pub gap_limit: Option<u64>,
    #[serde(default)]
    pub first_opened: u64,
    #[serde(default)]
    pub last_opened: u64,
}

pub struct Registry {
    entries: Vec<WalletEntry>,
    path: Option<PathBuf>,
}

impl Registry {
    /// Loads the file; a missing or unreadable one starts empty.
    pub fn new(path: Option<PathBuf>) -> Self {
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Vec<WalletEntry>>(&s).ok())
            .unwrap_or_default();
        Self { entries, path }
    }

    fn persist(&self) -> Result<(), String> {
        let Some(p) = self.path.as_ref() else { return Ok(()) };
        let body = serde_json::to_string_pretty(&self.entries).map_err(|e| e.to_string())?;
        crate::write_atomic(p, body.as_bytes()).map_err(|e| format!("wallets.json: {e}"))
    }

    pub fn get(&self, network: &str, group: &str) -> Option<&WalletEntry> {
        self.entries.iter().find(|e| e.network == network && e.group == group)
    }

    /// Networks a group is known on.
    pub fn networks_of(&self, group: &str) -> Vec<String> {
        self.entries.iter().filter(|e| e.group == group).map(|e| e.network.clone()).collect()
    }

    /// Records an open. A label or birthday left out keeps the one already stored.
    pub fn record_open(&mut self, network: &str, group: &str, family: &str, label: Option<&str>, birthday: Value, now: u64) -> Result<(), String> {
        match self.entries.iter_mut().find(|e| e.network == network && e.group == group) {
            Some(e) => {
                if !family.is_empty() {
                    e.family = family.into();
                }
                if let Some(l) = label {
                    e.label = l.into();
                }
                if !birthday.is_null() {
                    e.birthday = birthday;
                }
                e.last_opened = now;
            }
            None => self.entries.push(WalletEntry {
                group: group.into(),
                family: family.into(),
                network: network.into(),
                label: label.unwrap_or_default().into(),
                birthday,
                gap_limit: None,
                first_opened: now,
                last_opened: now,
            }),
        }
        self.persist()
    }

    /// A birthday set later; the wallet's other fields stay.
    pub fn set_birthday(&mut self, network: &str, group: &str, birthday: Value) -> Result<(), String> {
        match self.entries.iter_mut().find(|e| e.network == network && e.group == group) {
            Some(e) => e.birthday = birthday,
            None => return Ok(()),
        }
        self.persist()
    }

    /// The gap limit the core holds for a known wallet; written only when it changed.
    pub fn set_gap_limit(&mut self, network: &str, group: &str, gap_limit: u64) -> Result<bool, String> {
        match self.entries.iter_mut().find(|e| e.network == network && e.group == group) {
            Some(e) if e.gap_limit != Some(gap_limit) => e.gap_limit = Some(gap_limit),
            _ => return Ok(false),
        }
        self.persist().map(|_| true)
    }

    /// Adds a wallet the core has on disk but this registry never saw; true if added.
    pub fn discover(&mut self, network: &str, group: &str, family: &str) -> Result<bool, String> {
        if group.is_empty() || self.get(network, group).is_some() {
            return Ok(false);
        }
        self.entries.push(WalletEntry { group: group.into(), family: family.into(), network: network.into(), ..Default::default() });
        self.persist().map(|_| true)
    }

    /// Newest open first.
    pub fn list_json(&self) -> Value {
        let mut v = self.entries.clone();
        v.sort_by(|a, b| b.last_opened.cmp(&a.last_opened).then(a.group.cmp(&b.group)));
        json!(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_never_holds_a_secret() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.json");
        {
            let mut r = Registry::new(Some(p.clone()));
            r.record_open("signet", "g_1", "bitcoin", Some("Savings"), json!({"height": 290000, "source": "keystore"}), 100).unwrap();
            r.record_open("regtest", "g_1", "bitcoin", None, Value::Null, 200).unwrap();
        }
        let text = std::fs::read_to_string(&p).unwrap().to_lowercase();
        for word in ["password", "phrase", "seed", "xprv", "tprv", "tpub", "xpub", "databasekey", "privkey", "fingerprint", "descriptor", "wpkh("] {
            assert!(!text.contains(word), "wallets.json holds {word}");
        }
        let r = Registry::new(Some(p));
        assert_eq!(r.get("signet", "g_1").unwrap().label, "Savings");
        assert_eq!(r.get("regtest", "g_1").unwrap().label, "", "an entry per network");
        assert_eq!(r.networks_of("g_1").len(), 2);
        assert_eq!(r.list_json()[0]["network"], "regtest", "newest open first");
    }

    #[test]
    fn a_reopen_keeps_what_it_does_not_restate() {
        let mut r = Registry::new(None);
        r.record_open("signet", "g_1", "bitcoin", Some("Savings"), json!({"height": 5, "source": "keystore"}), 1).unwrap();
        r.record_open("signet", "g_1", "bitcoin", None, Value::Null, 9).unwrap();
        let e = r.get("signet", "g_1").unwrap();
        assert_eq!((e.label.as_str(), e.birthday["height"].as_u64(), e.first_opened, e.last_opened), ("Savings", Some(5), 1, 9));
        assert!(!r.discover("signet", "g_1", "bitcoin").unwrap(), "already known");
        r.set_birthday("signet", "g_1", json!({"height": 7, "source": "set"})).unwrap();
        let e = r.get("signet", "g_1").unwrap();
        assert_eq!((e.birthday["source"].as_str(), e.last_opened, e.label.as_str()), (Some("set"), 9, "Savings"));
        assert!(r.discover("signet", "g_2", "bitcoin_taproot").unwrap());
    }

    #[test]
    fn the_gap_limit_is_kept_per_wallet() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.json");
        let mut r = Registry::new(Some(p.clone()));
        r.record_open("signet", "g_1", "bitcoin", None, Value::Null, 1).unwrap();
        assert_eq!(r.list_json()[0]["gapLimit"], Value::Null, "unknown until the core says");
        assert!(r.set_gap_limit("signet", "g_1", 20).unwrap());
        assert!(!r.set_gap_limit("signet", "g_1", 20).unwrap(), "unchanged, nothing written");
        assert!(r.set_gap_limit("signet", "g_1", 200).unwrap());
        assert!(!r.set_gap_limit("signet", "g_9", 50).unwrap(), "an unknown wallet stays unknown");
        r.record_open("signet", "g_1", "bitcoin", None, Value::Null, 2).unwrap();
        let r = Registry::new(Some(p));
        assert_eq!(r.get("signet", "g_1").unwrap().gap_limit, Some(200), "a reopen keeps it");
        assert_eq!(r.list_json()[0]["gapLimit"], 200);
    }

    #[test]
    fn a_torn_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.json");
        std::fs::write(&p, "[{").unwrap();
        assert_eq!(Registry::new(Some(p)).list_json(), json!([]));
    }
}
