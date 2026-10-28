//! Wallets opened before, per (network, group), for the app to offer a reopen: one list per seed in
//! wallets.sealed.json, sealed under this module's key for the seed's "wallets" data and read only
//! while the seed is unlocked. Never a secret: no descriptor, key, fingerprint or password.

use std::path::PathBuf;

use logos_signer_interface::sealed::SealedDocs;
use logos_signer_interface::wallets::DataKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// What the lists are called in their boxes.
const NAME: &str = "bitcoin_wallet_backend wallets";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletEntry {
    pub group: String,
    /// The signer manager's wallet it belongs to, whose key seals it.
    #[serde(default)]
    pub seed: String,
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

/// An open, as the wallet core's result names it. A label left out keeps the stored one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Opened {
    pub network: String,
    pub group: String,
    pub seed: String,
    pub family: String,
    pub label: Option<String>,
    pub birthday: Value,
    pub gap_limit: Option<u64>,
    pub now: u64,
}

pub struct Registry {
    docs: SealedDocs,
    /// The entries of the seeds whose list can be read.
    entries: Vec<WalletEntry>,
    /// Opens of a seed whose list is still sealed here, recorded once it opens.
    deferred: Vec<Opened>,
}

impl Registry {
    /// The lists kept at `path`, each sealed until its seed's key comes; in memory only without one.
    pub fn new(path: Option<PathBuf>) -> Self {
        let docs = match path {
            Some(p) => crate::open_sealed(p, NAME),
            None => SealedDocs::in_memory(NAME),
        };
        Self { docs, entries: vec![], deferred: vec![] }
    }

    /// Seals the seed's list, or keeps it in memory while no key for it has come.
    fn persist(&mut self, seed: &str) -> Result<(), String> {
        let list: Vec<&WalletEntry> = self.entries.iter().filter(|e| e.seed == seed).collect();
        let body = serde_json::to_vec(&list).map_err(|e| e.to_string())?;
        self.docs.put(seed, body).map_err(|e| format!("wallets.sealed.json: {}", e.message))
    }

    /// Sealed here and its key not given yet.
    fn is_locked(&self, seed: &str) -> bool {
        self.docs.wanted(seed).is_some()
    }

    pub fn get(&self, network: &str, group: &str) -> Option<&WalletEntry> {
        self.entries.iter().find(|e| e.network == network && e.group == group)
    }

    /// Networks a group is known on.
    pub fn networks_of(&self, group: &str) -> Vec<String> {
        self.entries.iter().filter(|e| e.group == group).map(|e| e.network.clone()).collect()
    }

    /// Records an open. A label or birthday left out keeps the one already stored. An open of a
    /// seed whose list is still sealed here is recorded once its key comes.
    pub fn record_open(&mut self, o: Opened) -> Result<(), String> {
        if o.seed.is_empty() {
            return Err("the wallet core named no seed for the wallet it opened; not recorded".into());
        }
        if self.is_locked(&o.seed) {
            self.deferred.push(o);
            return Ok(());
        }
        let moved = self.apply(&o);
        if let Some(old) = moved {
            self.persist(&old)?;
        }
        self.persist(&o.seed)
    }

    /// The seed the entry was kept under, when the open moved it to another.
    fn apply(&mut self, o: &Opened) -> Option<String> {
        match self.entries.iter_mut().find(|e| e.network == o.network && e.group == o.group) {
            Some(e) => {
                let moved = (e.seed != o.seed).then(|| std::mem::replace(&mut e.seed, o.seed.clone()));
                if !o.family.is_empty() {
                    e.family = o.family.clone();
                }
                if let Some(l) = &o.label {
                    e.label = l.clone();
                }
                if !o.birthday.is_null() {
                    e.birthday = o.birthday.clone();
                }
                if o.gap_limit.is_some() {
                    e.gap_limit = o.gap_limit;
                }
                e.last_opened = o.now;
                moved
            }
            None => {
                self.entries.push(WalletEntry {
                    group: o.group.clone(),
                    seed: o.seed.clone(),
                    family: o.family.clone(),
                    network: o.network.clone(),
                    label: o.label.clone().unwrap_or_default(),
                    birthday: o.birthday.clone(),
                    gap_limit: o.gap_limit,
                    first_opened: o.now,
                    last_opened: o.now,
                });
                None
            }
        }
    }

    /// A birthday set later; the wallet's other fields stay.
    pub fn set_birthday(&mut self, network: &str, group: &str, birthday: Value) -> Result<(), String> {
        let seed = match self.entries.iter_mut().find(|e| e.network == network && e.group == group) {
            Some(e) => {
                e.birthday = birthday;
                e.seed.clone()
            }
            None => return Ok(()),
        };
        self.persist(&seed)
    }

    /// The gap limit the core holds for a known wallet; written only when it changed.
    pub fn set_gap_limit(&mut self, network: &str, group: &str, gap_limit: u64) -> Result<bool, String> {
        let seed = match self.entries.iter_mut().find(|e| e.network == network && e.group == group) {
            Some(e) if e.gap_limit != Some(gap_limit) => {
                e.gap_limit = Some(gap_limit);
                e.seed.clone()
            }
            _ => return Ok(false),
        };
        self.persist(&seed).map(|_| true)
    }

    /// Adds a wallet the core lists but this registry never saw; true if added. Not while its
    /// seed's list is sealed here: it may be in there.
    pub fn discover(&mut self, network: &str, group: &str, seed: &str, family: &str) -> Result<bool, String> {
        if group.is_empty() || seed.is_empty() || self.is_locked(seed) || self.get(network, group).is_some() {
            return Ok(false);
        }
        self.entries.push(WalletEntry { group: group.into(), seed: seed.into(), family: family.into(), network: network.into(), ..Default::default() });
        self.persist(seed).map(|_| true)
    }

    /// Newest open first, each naming its seed.
    pub fn list_json(&self) -> Value {
        let mut v = self.entries.clone();
        v.sort_by(|a, b| b.last_opened.cmp(&a.last_opened).then(a.group.cmp(&b.group)));
        json!(v)
    }

    /// The seeds whose list waits for a key, each with the generation it is sealed under, if it
    /// is sealed yet: the keys to ask for.
    pub fn closed(&self) -> Vec<(String, Option<u32>)> {
        let mut out: Vec<(String, Option<u32>)> = self.docs.seeds().into_iter().filter(|s| !self.docs.is_open(s)).map(|s| (s.clone(), self.docs.wanted(&s))).collect();
        for o in &self.deferred {
            if !out.iter().any(|(s, _)| *s == o.seed) {
                out.push((o.seed.clone(), None));
            }
        }
        out
    }

    /// The seeds whose list can be read now.
    pub fn readable_seeds(&self) -> Vec<String> {
        self.docs.readable().into_iter().map(|(s, _)| s).collect()
    }

    /// A seed's key came: its list opens, sealed again under `current`, with the opens that waited.
    pub fn unlock(&mut self, seed: &str, box_key: &DataKey, current: &DataKey) -> Result<(), String> {
        self.docs.unlock(seed, box_key, current).map_err(|e| e.message)?;
        let mut list: Vec<WalletEntry> = match self.docs.get(seed).filter(|b| !b.is_empty()) {
            Some(b) => serde_json::from_slice(b).map_err(|e| format!("wallets.sealed.json: {e}"))?,
            None => vec![],
        };
        list.retain(|e| e.seed == seed);
        self.entries.retain(|e| e.seed != seed);
        self.entries.extend(list);
        let (waited, rest): (Vec<Opened>, Vec<Opened>) = std::mem::take(&mut self.deferred).into_iter().partition(|o| o.seed == seed);
        self.deferred = rest;
        if waited.is_empty() {
            return Ok(());
        }
        for o in &waited {
            if let Some(old) = self.apply(o) {
                self.persist(&old)?;
            }
        }
        self.persist(seed)
    }

    /// Wallets were locked: no entry of a locked seed is listed or read. Opens still waiting stay.
    pub fn lock(&mut self) {
        self.docs.lock();
        let readable = self.readable_seeds();
        self.entries.retain(|e| readable.contains(&e.seed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_key as key;

    fn opened(network: &str, group: &str, seed: &str, label: Option<&str>, birthday: Value, now: u64) -> Opened {
        Opened { network: network.into(), group: group.into(), seed: seed.into(), family: "bitcoin".into(), label: label.map(String::from), birthday, gap_limit: None, now }
    }

    /// The registry as a restarted backend finds it, with these seeds' keys given.
    fn reopened(p: &std::path::Path, keys: &[(&str, u8)]) -> Registry {
        let mut r = Registry::new(Some(p.to_path_buf()));
        for (seed, byte) in keys {
            r.unlock(seed, &key(*byte, 0), &key(*byte, 0)).unwrap();
        }
        r
    }

    #[test]
    fn round_trips_and_never_holds_a_secret() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.sealed.json");
        {
            let mut r = Registry::new(Some(p.clone()));
            r.record_open(opened("signet", "g_1", "seed_a", Some("Savings"), json!({"height": 290000, "source": "keystore"}), 100)).unwrap();
            r.record_open(opened("regtest", "g_1", "seed_a", None, Value::Null, 200)).unwrap();
            assert!(!p.exists(), "nothing written before the seed's key");
            r.unlock("seed_a", &key(1, 0), &key(1, 0)).unwrap();
        }
        let text = crate::clear_text_of(&p);
        for word in ["password", "phrase", "xprv", "tprv", "tpub", "xpub", "databasekey", "privkey", "fingerprint", "descriptor", "wpkh(", "g_1", "savings", "290000", "signet", "label"] {
            assert!(!text.contains(word), "wallets.sealed.json holds {word}: {text}");
        }
        let r = reopened(&p, &[("seed_a", 1)]);
        assert_eq!(r.get("signet", "g_1").unwrap().label, "Savings");
        assert_eq!(r.get("regtest", "g_1").unwrap().label, "", "an entry per network");
        assert_eq!(r.networks_of("g_1").len(), 2);
        assert_eq!(r.list_json()[0]["network"], "regtest", "newest open first");
        assert_eq!(r.list_json()[0]["seed"], "seed_a");
    }

    #[test]
    fn a_reopened_store_reads_only_with_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.sealed.json");
        let mut r = Registry::new(Some(p.clone()));
        r.unlock("seed_a", &key(1, 0), &key(1, 0)).unwrap();
        r.record_open(opened("signet", "g_1", "seed_a", Some("Savings"), Value::Null, 1)).unwrap();
        let mut r = Registry::new(Some(p.clone()));
        assert_eq!(r.list_json(), json!([]), "sealed until the key comes");
        assert_eq!(r.closed(), vec![("seed_a".to_string(), Some(0))]);
        assert!(r.unlock("seed_a", &key(2, 0), &key(2, 0)).is_err(), "another key");
        assert_eq!(r.list_json(), json!([]));
        r.unlock("seed_a", &key(1, 0), &key(1, 0)).unwrap();
        assert_eq!(r.get("signet", "g_1").unwrap().label, "Savings");
        assert!(r.closed().is_empty());
    }

    #[test]
    fn a_locked_seeds_wallets_are_neither_listed_nor_written() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.sealed.json");
        let mut r = reopened(&p, &[("seed_a", 1), ("seed_b", 2)]);
        r.record_open(opened("signet", "g_a", "seed_a", Some("A"), Value::Null, 1)).unwrap();
        r.record_open(opened("signet", "g_b", "seed_b", Some("B"), Value::Null, 2)).unwrap();
        r.lock();
        assert_eq!(r.list_json(), json!([]), "a lock drops what was read");
        r.unlock("seed_a", &key(1, 0), &key(1, 0)).unwrap();
        let listed = r.list_json();
        assert_eq!((listed.as_array().unwrap().len(), listed[0]["group"].clone()), (1, json!("g_a")), "only the unlocked seed's");
        let sealed_b = |p: &std::path::Path| serde_json::from_str::<Value>(&std::fs::read_to_string(p).unwrap()).unwrap()["seeds"]["seed_b"].clone();
        let before = sealed_b(&p);
        r.record_open(opened("signet", "g_b", "seed_b", Some("Renamed"), Value::Null, 3)).unwrap();
        assert!(!r.discover("signet", "g_b2", "seed_b", "bitcoin").unwrap(), "not while its list is sealed");
        assert!(!r.set_gap_limit("signet", "g_b", 50).unwrap());
        assert_eq!(sealed_b(&p), before, "seed_b's list is not written while it is locked");
        assert!(r.get("signet", "g_b").is_none() && r.list_json().as_array().unwrap().len() == 1);
        assert_eq!(r.closed(), vec![("seed_b".to_string(), Some(0))]);
        // Its key comes: the open that waited is recorded over what the list held.
        r.unlock("seed_b", &key(2, 0), &key(2, 0)).unwrap();
        let b = r.get("signet", "g_b").unwrap();
        assert_eq!((b.label.as_str(), b.first_opened, b.last_opened), ("Renamed", 2, 3));
        assert_ne!(sealed_b(&p), before);
        assert_eq!(reopened(&p, &[("seed_b", 2)]).get("signet", "g_b").unwrap().label, "Renamed");
        assert!(r.record_open(opened("signet", "g_c", "", None, Value::Null, 4)).is_err(), "an open without a seed isn't kept");
    }

    #[test]
    fn a_generation_change_reseals() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.sealed.json");
        let mut r = reopened(&p, &[("seed_a", 1)]);
        r.record_open(opened("signet", "g_1", "seed_a", Some("Savings"), Value::Null, 1)).unwrap();
        let mut r = Registry::new(Some(p.clone()));
        assert_eq!(r.closed(), vec![("seed_a".to_string(), Some(0))]);
        // The box's generation opens it; the current one seals it again.
        r.unlock("seed_a", &key(1, 0), &key(9, 1)).unwrap();
        assert_eq!(r.get("signet", "g_1").unwrap().label, "Savings");
        let mut r = Registry::new(Some(p.clone()));
        assert_eq!(r.closed(), vec![("seed_a".to_string(), Some(1))], "sealed under the current generation");
        assert!(r.unlock("seed_a", &key(1, 0), &key(1, 0)).is_err(), "the old key no longer opens it");
        r.unlock("seed_a", &key(9, 1), &key(9, 1)).unwrap();
        assert_eq!(r.get("signet", "g_1").unwrap().label, "Savings");
    }

    #[test]
    fn a_reopen_keeps_what_it_does_not_restate() {
        let mut r = Registry::new(None);
        r.record_open(opened("signet", "g_1", "seed_a", Some("Savings"), json!({"height": 5, "source": "keystore"}), 1)).unwrap();
        r.record_open(opened("signet", "g_1", "seed_a", None, Value::Null, 9)).unwrap();
        let e = r.get("signet", "g_1").unwrap();
        assert_eq!((e.label.as_str(), e.birthday["height"].as_u64(), e.first_opened, e.last_opened), ("Savings", Some(5), 1, 9));
        assert!(!r.discover("signet", "g_1", "seed_a", "bitcoin").unwrap(), "already known");
        r.set_birthday("signet", "g_1", json!({"height": 7, "source": "set"})).unwrap();
        let e = r.get("signet", "g_1").unwrap();
        assert_eq!((e.birthday["source"].as_str(), e.last_opened, e.label.as_str()), (Some("set"), 9, "Savings"));
        assert!(r.discover("signet", "g_2", "seed_a", "bitcoin_taproot").unwrap());
        assert!(!r.discover("signet", "g_3", "", "bitcoin").unwrap(), "a wallet without a seed isn't kept");
    }

    #[test]
    fn the_gap_limit_is_kept_per_wallet() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.sealed.json");
        let mut r = reopened(&p, &[("seed_a", 1)]);
        r.record_open(opened("signet", "g_1", "seed_a", None, Value::Null, 1)).unwrap();
        assert_eq!(r.list_json()[0]["gapLimit"], Value::Null, "unknown until the core says");
        assert!(r.set_gap_limit("signet", "g_1", 20).unwrap());
        assert!(!r.set_gap_limit("signet", "g_1", 20).unwrap(), "unchanged, nothing written");
        assert!(r.set_gap_limit("signet", "g_1", 200).unwrap());
        assert!(!r.set_gap_limit("signet", "g_9", 50).unwrap(), "an unknown wallet stays unknown");
        r.record_open(opened("signet", "g_1", "seed_a", None, Value::Null, 2)).unwrap();
        let r = reopened(&p, &[("seed_a", 1)]);
        assert_eq!(r.get("signet", "g_1").unwrap().gap_limit, Some(200), "a reopen keeps it");
        assert_eq!(r.list_json()[0]["gapLimit"], 200);
    }

    #[test]
    fn a_torn_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.sealed.json");
        std::fs::write(&p, "[{").unwrap();
        let mut r = Registry::new(Some(p.clone()));
        assert_eq!(r.list_json(), json!([]));
        assert!(p.with_extension("json.unreadable").exists(), "set aside, not overwritten unread");
        r.unlock("seed_a", &key(1, 0), &key(1, 0)).unwrap();
        r.record_open(opened("signet", "g_1", "seed_a", None, Value::Null, 1)).unwrap();
        assert_eq!(reopened(&p, &[("seed_a", 1)]).list_json().as_array().unwrap().len(), 1);
    }
}
