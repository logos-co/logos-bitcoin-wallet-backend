//! Who may do what: one custodian set, the host anchor always refused, unnamed methods refused.
//! There is no approver; a send is approved in the keystore's signer app.

use serde::Deserialize;

/// The wallet app holds the custodian role until `configure` says otherwise.
pub const DEFAULT_CUSTODIAN: &str = "bitcoin_wallet_ui";

/// Every refusal, byte for byte; `json!` would sort the keys.
pub const NOT_AUTHORIZED: &str = r#"{"ok":false,"error":"not authorized"}"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    Unknown,
    HostAnchor,
    Module(String),
    Derived { parent: String, leaf: String },
    Operator(String),
}

impl Caller {
    pub fn is_module(&self, name: &str) -> bool {
        matches!(self, Caller::Module(n) if n == name)
    }

    /// Only a plainly named module has a name a request can be recorded against.
    pub fn named(&self) -> Option<&str> {
        match self {
            Caller::Module(n) if !n.is_empty() => Some(n.as_str()),
            _ => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Caller::Unknown => "unknown",
            Caller::HostAnchor => "host",
            Caller::Module(_) => "module",
            Caller::Derived { .. } => "derived",
            Caller::Operator(_) => "operator",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roles {
    pub custodians: Vec<String>,
}

impl Default for Roles {
    fn default() -> Self {
        Self { custodians: vec![DEFAULT_CUSTODIAN.into()] }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RoleWire {
    One(String),
    Many(Vec<String>),
}

impl Default for RoleWire {
    fn default() -> Self {
        RoleWire::Many(Vec::new())
    }
}

impl RoleWire {
    fn into_holders(self) -> Vec<String> {
        let raw = match self {
            RoleWire::One(s) => vec![s],
            RoleWire::Many(v) => v,
        };
        let mut out: Vec<String> = Vec::with_capacity(raw.len());
        for n in raw {
            let n = n.trim().to_string();
            if !n.is_empty() && !out.contains(&n) {
                out.push(n);
            }
        }
        out
    }
}

/// The fields `configure` takes; anything else is refused.
pub const ROLES_FIELDS: &[&str] = &["custodians"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RolesWire {
    #[serde(default)]
    custodians: RoleWire,
}

impl Roles {
    /// Replaces the custodian set. Total, not a patch: `{}` leaves nobody holding it. A
    /// malformed document or an unknown key is refused and the roles in force stay.
    pub fn configure(&mut self, doc: &str) -> Result<(), String> {
        let v: serde_json::Value = serde_json::from_str(doc).map_err(|e| format!("bad roles document: {e}"))?;
        if !v.is_object() {
            return Err("roles document must be an object".into());
        }
        let wire: RolesWire = serde_json::from_value(v).map_err(|e| format!("bad roles document: {e}"))?;
        self.custodians = wire.custodians.into_holders();
        Ok(())
    }

    /// Roles at load: no file means the defaults; an unreadable one means nobody.
    pub fn from_file(contents: Option<&str>) -> Self {
        match contents {
            None => Self::default(),
            Some(text) => {
                let mut r = Self { custodians: vec![] };
                if r.configure(text).is_err() {
                    return Self { custodians: vec![] };
                }
                r
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"custodians": self.custodians})
    }

    pub fn holds(&self, caller: &Caller) -> bool {
        self.custodians.iter().any(|h| !h.is_empty() && caller.is_module(h))
    }
}

/// Custodian only: the roles, the network, opening and closing, the public descriptors, the
/// wallet's birthday and gap limit, the device-wide source settings, and rebroadcasting.
pub const CUSTODIAN_METHODS: &[&str] = &[
    "configure",
    "set_active_network",
    "open_wallet",
    "close_wallet",
    "descriptors",
    "set_gap_limit",
    "set_sources",
    "set_source",
    "set_proxy",
    "clear_suspect",
    "rebroadcast",
    "abandon_send",
    "set_birthday",
    "set_own_peers",
    "peer_list",
];

/// Any named module may ask for a send; the person approves it in the signer app.
pub const REQUESTER_METHODS: &[&str] = &["request_send", "request_fee_bump"];

/// The requester of that send, or a custodian.
pub const CANCEL_METHODS: &[&str] = &["cancel_send"];

/// Reads and passthroughs that need no role.
pub const UNGATED_METHODS: &[&str] = &[
    "caller_identity",
    "list_networks",
    "list_wallets",
    "job_status",
    "wallet_status",
    "balances",
    "history",
    "receive_address",
    "current_address",
    "fee_rates",
    "address_valid",
    "format_btc",
    "parse_btc",
    "sources",
    "source_health",
    "probe",
    "exposure",
    "send_status",
    "list_sends",
];

pub fn custodian_admits(method: &str, roles: &Roles, caller: &Caller) -> bool {
    CUSTODIAN_METHODS.contains(&method) && roles.holds(caller)
}

/// Asking for a send: any named module, never the host anchor.
pub fn requester_admits(method: &str, caller: &Caller) -> bool {
    REQUESTER_METHODS.contains(&method) && caller.named().is_some()
}

/// Withdrawing a send: the module that asked for it, or a custodian.
pub fn canceller_admits(roles: &Roles, caller: &Caller, requester: &str) -> bool {
    roles.holds(caller) || (!requester.is_empty() && caller.named() == Some(requester))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(n: &str) -> Caller {
        Caller::Module(n.into())
    }

    #[test]
    fn defaults_admit_the_app_only() {
        let r = Roles::default();
        assert!(custodian_admits("open_wallet", &r, &m("bitcoin_wallet_ui")));
        assert!(!custodian_admits("open_wallet", &r, &m("bitcoin_wallet_cli")));
        assert!(!custodian_admits("configure", &r, &m("some_dapp")));
    }

    #[test]
    fn configure_is_total_and_refuses_unknown_keys() {
        let mut r = Roles::default();
        assert!(r.configure(r#"{"custodian":"x"}"#).is_err(), "the singular spelling is refused");
        assert!(r.configure(r#"{"approvers":["x"]}"#).is_err(), "there is no approver role here");
        assert!(r.configure("[]").is_err());
        assert!(r.configure("{").is_err());
        assert_eq!(r, Roles::default(), "a refused document leaves the roles in force");
        r.configure(r#"{"custodians":"bitcoin_wallet_cli"}"#).unwrap();
        assert_eq!(r.custodians, vec!["bitcoin_wallet_cli".to_string()]);
        assert!(!custodian_admits("open_wallet", &r, &m("bitcoin_wallet_ui")), "the app was not restated");
        r.configure(r#"{"custodians":[" a ","a","",  "b"]}"#).unwrap();
        assert_eq!(r.custodians, vec!["a".to_string(), "b".to_string()], "trimmed, deduplicated, blanks dropped");
        r.configure("{}").unwrap();
        assert!(r.custodians.is_empty(), "an empty document is nobody");
        assert!(!custodian_admits("open_wallet", &r, &m("bitcoin_wallet_ui")));
    }

    #[test]
    fn the_host_anchor_and_strangers_are_refused_everywhere() {
        let mut r = Roles::default();
        r.configure(r#"{"custodians":["bitcoin_wallet_ui","core","capability_module",""]}"#).unwrap();
        let strangers = [
            Caller::HostAnchor,
            Caller::Unknown,
            Caller::Derived { parent: "bitcoin_wallet_ui".into(), leaf: "x".into() },
            Caller::Operator("bitcoin_wallet_ui".into()),
            m(""),
        ];
        for who in &strangers {
            for method in CUSTODIAN_METHODS {
                assert!(!custodian_admits(method, &r, who), "{method} {who:?}");
            }
            for method in REQUESTER_METHODS {
                assert!(!requester_admits(method, who), "{method} {who:?}");
            }
            assert!(!canceller_admits(&r, who, "bitcoin_wallet_ui"), "{who:?}");
        }
        assert!(requester_admits("request_send", &m("some_dapp")));
        assert!(!requester_admits("open_wallet", &m("some_dapp")), "a request method only");
    }

    #[test]
    fn a_method_the_registry_does_not_name_is_refused_even_for_the_holder() {
        let r = Roles::default();
        assert!(!custodian_admits("balances", &r, &m("bitcoin_wallet_ui")));
        assert!(!custodian_admits("open_walet", &r, &m("bitcoin_wallet_ui")), "a typo fails closed");
        assert!(!custodian_admits("approve_send", &r, &m("bitcoin_wallet_ui")), "approval is the signer app's");
    }

    #[test]
    fn the_requester_or_a_custodian_may_cancel() {
        let r = Roles::default();
        assert!(canceller_admits(&r, &m("some_dapp"), "some_dapp"));
        assert!(canceller_admits(&r, &m("bitcoin_wallet_ui"), "some_dapp"));
        assert!(!canceller_admits(&r, &m("other_dapp"), "some_dapp"));
        assert!(!canceller_admits(&r, &m("other_dapp"), ""), "a send with no requester is the custodian's");
    }

    #[test]
    fn the_registries_do_not_overlap() {
        let all = [CUSTODIAN_METHODS, REQUESTER_METHODS, CANCEL_METHODS, UNGATED_METHODS];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                for x in a.iter() {
                    assert!(!b.contains(x), "{x} is in two registries");
                }
            }
        }
    }

    #[test]
    fn roles_file() {
        assert_eq!(Roles::from_file(None), Roles::default());
        assert_eq!(Roles::from_file(Some(r#"{"custodians":["a"]}"#)).custodians, vec!["a".to_string()]);
        assert!(Roles::from_file(Some("{")).custodians.is_empty(), "a torn file admits nobody");
        assert!(Roles::from_file(Some(r#"{"custodians":["a"],"x":1}"#)).custodians.is_empty());
    }

    #[test]
    fn refusal_is_exact() {
        let v: serde_json::Value = serde_json::from_str(NOT_AUTHORIZED).unwrap();
        assert_eq!(v, serde_json::json!({"ok": false, "error": "not authorized"}));
        assert!(NOT_AUTHORIZED.starts_with(r#"{"ok":false,"#));
    }
}
