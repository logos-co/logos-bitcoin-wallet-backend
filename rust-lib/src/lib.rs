//! bitcoin_wallet_backend: roles, the wallet registry, sends and the core's jobs, between the
//! Bitcoin wallet surfaces and the wallet core. No keys, no passwords.

pub mod amount;
pub mod gate;
pub mod jobs;
pub mod registry;
pub mod request;
pub mod sends;
pub mod view;

#[cfg(feature = "logos_module")]
mod glue;

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Writes through a temporary file and a rename, so a reader never sees half a file.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

/// No method takes a secret: no parameter or accepted field may be named like one.
#[cfg(test)]
mod contract {
    const BANNED: [&str; 12] = ["password", "passphrase", "phrase", "mnemonic", "seed", "secret", "key", "xprv", "tprv", "privkey", "wif", "pin"];

    fn banned(name: &str) -> Option<&'static str> {
        let n = name.to_ascii_lowercase();
        BANNED.iter().copied().find(|b| n.contains(b))
    }

    /// `(method, parameter names)` of a trait in `src`.
    fn trait_methods(src: &str, name: &str) -> Vec<(String, Vec<String>)> {
        let start = src.find(&format!("pub trait {name}")).expect("the trait");
        let end = start + src[start..].find("\n}\n").expect("the trait's end");
        let mut out = vec![];
        for line in src[start..end].lines().map(str::trim).filter(|l| l.starts_with("fn ")) {
            let method = line[3..line.find('(').unwrap()].trim().to_string();
            let args = &line[line.find('(').unwrap() + 1..line.rfind(')').unwrap()];
            let params = args.split(',').map(str::trim).filter(|a| !a.is_empty() && !a.contains("self")).map(|a| a.split(':').next().unwrap().trim().to_string()).collect();
            out.push((method, params));
        }
        out
    }

    const GLUE: &str = include_str!("glue.rs");

    #[test]
    fn no_parameter_or_field_is_named_like_a_secret() {
        let methods = trait_methods(GLUE, "BitcoinWalletBackendModule:");
        let events = trait_methods(GLUE, "BitcoinWalletBackendModuleEvents");
        assert!(methods.len() >= 30 && events.len() == 6, "found {} methods, {} events", methods.len(), events.len());
        for (m, params) in methods.iter().chain(&events) {
            for p in params {
                assert_eq!(banned(p), None, "{m}({p}) is named like a secret");
            }
        }
        let fields = [crate::request::OPEN_FIELDS, crate::request::SEND_FIELDS, crate::request::RECIPIENT_FIELDS, crate::request::BUMP_FIELDS, crate::request::ABANDON_FIELDS, crate::request::BIRTHDAY_FIELDS, crate::request::PROBE_FIELDS, crate::gate::ROLES_FIELDS];
        for f in fields.iter().flat_map(|f| f.iter()) {
            assert_eq!(banned(f), None, "the field {f} is named like a secret");
        }
    }

    #[test]
    fn the_check_catches_a_secret() {
        assert_eq!(banned("password"), Some("password"));
        assert_eq!(banned("databaseKey"), Some("key"));
        assert_eq!(banned("recoveryPhrase"), Some("phrase"));
        let fake = "pub trait BitcoinWalletBackendModule: Send {\n    fn unlock(&self, wallet_password: String) -> String;\n}\n";
        let m = trait_methods(fake, "BitcoinWalletBackendModule:");
        assert_eq!(m, vec![("unlock".to_string(), vec!["wallet_password".to_string()])]);
        assert_eq!(banned(&m[0].1[0]), Some("password"));
    }

    #[test]
    fn every_method_is_in_exactly_one_registry() {
        use crate::gate::{CANCEL_METHODS, CUSTODIAN_METHODS, REQUESTER_METHODS, UNGATED_METHODS};
        let methods: Vec<String> = trait_methods(GLUE, "BitcoinWalletBackendModule:").into_iter().map(|(m, _)| m).filter(|m| m != "on_context_ready").collect();
        let all: Vec<&str> = [CUSTODIAN_METHODS, REQUESTER_METHODS, CANCEL_METHODS, UNGATED_METHODS].concat();
        for m in &methods {
            assert_eq!(all.iter().filter(|x| **x == m.as_str()).count(), 1, "{m} must be in exactly one registry");
        }
        for x in &all {
            assert!(methods.iter().any(|m| m == x), "the registry names {x}, which is no method");
        }
    }
}
