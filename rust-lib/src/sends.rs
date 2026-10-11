//! Sends from request to settlement, one open per wallet: preparing → awaiting_approval →
//! committing → sent, or failed, cancelled, unknown. Unknown means it may have left.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::amount::btc_of;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Preparing,
    AwaitingApproval,
    Committing,
    Sent,
    Failed,
    Cancelled,
    Unknown,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Preparing => "preparing",
            State::AwaitingApproval => "awaiting_approval",
            State::Committing => "committing",
            State::Sent => "sent",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
            State::Unknown => "unknown",
        }
    }

    pub fn is_final(self) -> bool {
        matches!(self, State::Sent | State::Failed | State::Cancelled | State::Unknown)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Send,
    FeeBump,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Send => "send",
            Kind::FeeBump => "fee_bump",
        }
    }
}

/// Why a send is being withdrawn, which decides how it settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Withdrawal {
    Caller(String),
    NoAck,
    CoinsChanged,
    WalletClosed,
}

impl Withdrawal {
    fn settles_as(&self) -> (State, String) {
        match self {
            Withdrawal::Caller(who) => (State::Cancelled, format!("cancelled by {who}")),
            Withdrawal::NoAck => (State::Failed, NO_ACK.into()),
            Withdrawal::CoinsChanged => (State::Failed, COINS_CHANGED.into()),
            Withdrawal::WalletClosed => (State::Failed, WALLET_CLOSED.into()),
        }
    }
}

/// The keystore drops an offer nobody acknowledges after this long.
pub const ACK_WINDOW: Duration = Duration::from_secs(60);
/// Past the window plus this slack, the backend withdraws an unacknowledged offer itself.
pub const ACK_SLACK: Duration = Duration::from_secs(15);

pub const NO_ACK: &str = "expired: nobody acknowledged the approval within 60 s; request the send again";
pub const REJECTED: &str = "rejected: the person declined in the signer app";
pub const WITHDRAWN: &str = "cancelled: the approval request was withdrawn before anyone approved it";
pub const COINS_CHANGED: &str =
    "the wallet's coins changed while the send waited: an input is no longer the wallet's to spend; request the send again";
pub const WALLET_CLOSED: &str = "the wallet closed before the send was approved";
pub const CORE_LOST: &str = "the wallet core stopped answering before the send was approved";
pub const MAY_HAVE_LEFT: &str = "the transaction may have left: rebroadcast it or replace it with a higher fee; never pay again from other coins while it could still confirm";

/// What the glue does next for a send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    Nothing,
    /// Ask the core to sign and broadcast this preview.
    Sign(String),
    /// Cancel this preview in the core: it withdraws the approval and unlocks the inputs.
    CancelPreview(String),
    /// Cancel the core's build job, tracked under this backend job id.
    CancelBuild(String),
}

#[derive(Clone, Debug)]
pub struct Send {
    pub id: String,
    pub kind: Kind,
    pub requester: String,
    pub network: String,
    pub group: String,
    pub state: State,
    pub error: String,
    pub request: Value,
    pub preview: Value,
    pub preview_id: String,
    pub txid: String,
    pub approval_handle: String,
    /// The signer manager's state for the approval: offered, rendered, signing or settled.
    pub approval: String,
    pub broadcast: String,
    pub via: String,
    pub build_job: Option<String>,
    pub sign_job: Option<String>,
    pub withdrawal: Option<Withdrawal>,
    /// The core was asked to cancel the preview, which releases its inputs.
    pub preview_cancelled: bool,
    pub cancel_error: String,
    pub saw_settled: bool,
    pub saw_broadcasting: bool,
    pub awaiting_since: Option<Instant>,
    pub created_at: u64,
    pub updated_at: u64,
    pub rebroadcasts: Vec<Value>,
}

/// Final sends kept for list_sends; older ones are dropped.
const KEEP_FINAL: usize = 100;

#[derive(Default)]
pub struct Sends {
    next: u64,
    map: BTreeMap<u64, Send>,
}

fn num(id: &str) -> Option<u64> {
    id.strip_prefix('s')?.parse().ok()
}

impl Sends {
    pub fn get(&self, id: &str) -> Option<&Send> {
        self.map.get(&num(id)?)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Send> {
        self.map.get_mut(&num(id)?)
    }

    /// The open send of a wallet, if any.
    pub fn open_for(&self, network: &str, group: &str) -> Option<&str> {
        self.map.values().find(|s| !s.state.is_final() && s.network == network && s.group == group).map(|s| s.id.as_str())
    }

    pub fn any_open(&self) -> Option<&str> {
        self.map.values().find(|s| !s.state.is_final()).map(|s| s.id.as_str())
    }

    pub fn committing(&self) -> Option<&str> {
        self.map.values().find(|s| s.state == State::Committing).map(|s| s.id.as_str())
    }

    pub fn open_ids(&self) -> Vec<String> {
        self.map.values().filter(|s| !s.state.is_final()).map(|s| s.id.clone()).collect()
    }

    /// A new send in `preparing`; refused while the wallet has another open.
    pub fn begin(&mut self, kind: Kind, requester: &str, network: &str, group: &str, request: Value, now: u64) -> Result<String, String> {
        if let Some(open) = self.open_for(network, group) {
            return Err(format!("send {open} is still open: one send at a time per wallet"));
        }
        self.prune();
        self.next += 1;
        let id = format!("s{}", self.next);
        self.map.insert(
            self.next,
            Send {
                id: id.clone(),
                kind,
                requester: requester.into(),
                network: network.into(),
                group: group.into(),
                state: State::Preparing,
                error: String::new(),
                request,
                preview: Value::Null,
                preview_id: String::new(),
                txid: String::new(),
                approval_handle: String::new(),
                approval: String::new(),
                broadcast: String::new(),
                via: String::new(),
                build_job: None,
                sign_job: None,
                withdrawal: None,
                preview_cancelled: false,
                cancel_error: String::new(),
                saw_settled: false,
                saw_broadcasting: false,
                awaiting_since: None,
                created_at: now,
                updated_at: now,
                rebroadcasts: vec![],
            },
        );
        Ok(id)
    }

    fn settle(s: &mut Send, state: State, error: String) -> Option<State> {
        if s.state == state && s.error == error {
            return None;
        }
        s.state = state;
        s.error = error;
        s.updated_at = crate::now_secs();
        Some(state)
    }

    pub fn set_build_job(&mut self, id: &str, job: &str) {
        if let Some(s) = self.get_mut(id) {
            s.build_job = Some(job.into());
        }
    }

    pub fn set_sign_job(&mut self, id: &str, job: &str) {
        if let Some(s) = self.get_mut(id) {
            s.sign_job = Some(job.into());
        }
    }

    /// The core couldn't take a job for this send: nothing was built or asked.
    pub fn not_started(&mut self, id: &str, error: &str) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final())?;
        Self::settle(s, State::Failed, error.into())
    }

    /// The build finished: sign it, or cancel it when the send was withdrawn meanwhile.
    pub fn built(&mut self, id: &str, preview: Value) -> Next {
        let Some(s) = self.get_mut(id) else { return Next::Nothing };
        s.build_job = None;
        s.preview_id = preview["previewId"].as_str().unwrap_or_default().into();
        s.txid = preview["txid"].as_str().unwrap_or_default().into();
        s.preview = preview;
        s.updated_at = crate::now_secs();
        if s.preview_id.is_empty() {
            Self::settle(s, State::Failed, "the wallet core returned no preview".into());
            return Next::Nothing;
        }
        if s.state.is_final() || s.withdrawal.is_some() {
            s.preview_cancelled = true;
            return Next::CancelPreview(s.preview_id.clone());
        }
        Next::Sign(s.preview_id.clone())
    }

    pub fn build_failed(&mut self, id: &str, error: &str) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final())?;
        s.build_job = None;
        let (state, why) = match &s.withdrawal {
            Some(w) => w.settles_as(),
            None => (State::Failed, error.to_string()),
        };
        Self::settle(s, state, why)
    }

    /// Follows the core's preview state while the sign job runs. Never moves backwards.
    pub fn observe(&mut self, id: &str, preview_state: &str, handle: Option<&str>, now: Instant) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final())?;
        if let Some(h) = handle.filter(|h| !h.is_empty()) {
            s.approval_handle = h.into();
        }
        let (stage, sub) = preview_state.split_once(':').unwrap_or((preview_state, ""));
        match stage {
            "awaiting_approval" if s.state == State::Preparing || s.state == State::AwaitingApproval => {
                s.approval = if sub.is_empty() { "offered".into() } else { sub.into() };
                if s.state == State::Preparing {
                    s.awaiting_since = Some(now);
                    return Self::settle(s, State::AwaitingApproval, String::new());
                }
                None
            }
            "settled" => {
                s.saw_settled = true;
                s.approval = "settled".into();
                None
            }
            "broadcasting" => {
                s.saw_broadcasting = true;
                if s.state != State::Committing {
                    return Self::settle(s, State::Committing, String::new());
                }
                None
            }
            _ => None,
        }
    }

    /// The sign job finished: the transaction went out, or may have.
    pub fn signed(&mut self, id: &str, result: &Value) -> Option<State> {
        let s = self.get_mut(id)?;
        s.sign_job = None;
        if let Some(t) = result["txid"].as_str() {
            s.txid = t.into();
        }
        s.broadcast = result["broadcast"].as_str().unwrap_or("unknown").into();
        s.via = result["via"].as_str().unwrap_or_default().into();
        // What happened wins over a withdrawal that came too late.
        match s.broadcast.as_str() {
            "seen" | "requested" => Self::settle(s, State::Sent, String::new()),
            _ => Self::settle(s, State::Unknown, MAY_HAVE_LEFT.into()),
        }
    }

    /// The sign job failed. Returns the new state, and whether the preview's inputs should be
    /// released (they stay locked while the transaction may have left).
    pub fn sign_failed(&mut self, id: &str, error: &str) -> (Option<State>, bool) {
        let Some(s) = self.get_mut(id).filter(|s| !s.state.is_final()) else { return (None, false) };
        s.sign_job = None;
        let (state, why) = classify(error, s.saw_settled, s.saw_broadcasting, s.withdrawal.as_ref());
        let release = state != State::Unknown && !s.preview_id.is_empty() && !s.preview_cancelled;
        s.preview_cancelled |= release;
        (Self::settle(s, state, why), release)
    }

    /// The core stopped answering while this send's job ran.
    pub fn core_lost(&mut self, id: &str) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final())?;
        s.build_job = None;
        s.sign_job = None;
        if s.state == State::Committing || s.saw_settled {
            return Self::settle(s, State::Unknown, format!("{MAY_HAVE_LEFT} (the wallet core stopped answering)"));
        }
        Self::settle(s, State::Failed, CORE_LOST.into())
    }

    /// Starts withdrawing a send; it settles once the core confirms.
    pub fn withdraw(&mut self, id: &str, why: Withdrawal) -> Result<Next, String> {
        let s = self.get_mut(id).ok_or("unknown send")?;
        if s.state.is_final() {
            return Err(format!("send is already {}", s.state.name()));
        }
        if s.state == State::Committing || s.saw_broadcasting {
            return Err("too late: the transaction is going out".into());
        }
        if s.withdrawal.is_some() {
            return Ok(Next::Nothing);
        }
        s.withdrawal = Some(why);
        s.cancel_error.clear();
        if !s.preview_id.is_empty() {
            s.preview_cancelled = true;
            return Ok(Next::CancelPreview(s.preview_id.clone()));
        }
        Ok(s.build_job.clone().map(Next::CancelBuild).unwrap_or(Next::Nothing))
    }

    /// The core cancelled the preview. Settles now unless a sign job is still winding down.
    pub fn cancel_done(&mut self, id: &str) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final() && s.sign_job.is_none())?;
        let (state, why) = s.withdrawal.as_ref().map(|w| w.settles_as()).unwrap_or((State::Cancelled, String::new()));
        Self::settle(s, state, why)
    }

    /// The core refused the cancel. Too late means the transaction is going out.
    pub fn cancel_failed(&mut self, id: &str, error: &str) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final())?;
        if error.contains("too late") {
            s.withdrawal = None;
            s.cancel_error = error.into();
            return None;
        }
        if s.sign_job.is_some() || s.build_job.is_some() {
            return None;
        }
        // The preview is gone and no job runs for it: nothing can go out any more.
        let (state, why) = s.withdrawal.as_ref().map(|w| w.settles_as()).unwrap_or((State::Failed, error.to_string()));
        Self::settle(s, state, why)
    }

    /// Sends whose offer nobody acknowledged within the keystore's window plus slack.
    pub fn overdue_acks(&self, now: Instant) -> Vec<String> {
        self.map
            .values()
            .filter(|s| s.state == State::AwaitingApproval && s.withdrawal.is_none())
            .filter(|s| s.approval.is_empty() || s.approval == "offered")
            .filter(|s| s.awaiting_since.is_some_and(|t| now.saturating_duration_since(t) >= ACK_WINDOW + ACK_SLACK))
            .map(|s| s.id.clone())
            .collect()
    }

    /// Open sends with a preview, and the outpoints they spend.
    pub fn watched_inputs(&self) -> Vec<(String, Vec<String>)> {
        self.map
            .values()
            .filter(|s| matches!(s.state, State::Preparing | State::AwaitingApproval) && s.withdrawal.is_none() && !s.preview_id.is_empty())
            .map(|s| (s.id.clone(), inputs_of(&s.preview)))
            .collect()
    }

    /// Records a rebroadcast; an unknown send the network has now seen is sent.
    pub fn rebroadcast(&mut self, id: &str, result: &Value) -> Option<State> {
        let s = self.get_mut(id)?;
        s.rebroadcasts.push(json!({"at": crate::now_secs(), "broadcast": result["broadcast"], "via": result["via"]}));
        s.updated_at = crate::now_secs();
        if s.state == State::Unknown && result["broadcast"] == "seen" {
            s.broadcast = "seen".into();
            return Self::settle(s, State::Sent, String::new());
        }
        None
    }

    pub fn by_txid(&self, txid: &str) -> Option<&str> {
        self.map.values().rev().find(|s| s.txid == txid).map(|s| s.id.as_str())
    }

    pub fn status_json(&self, id: &str) -> Value {
        let Some(s) = self.get(id) else { return json!({"ok": false, "error": "unknown send"}) };
        let mut v = json!({
            "ok": true, "requestId": s.id, "kind": s.kind.name(), "state": s.state.name(),
            "requester": s.requester, "network": s.network, "group": s.group,
            "summary": summary(&s.preview), "approvalHandle": opt(&s.approval_handle), "approval": opt(&s.approval),
            "txid": opt(&s.txid), "broadcast": opt(&s.broadcast), "via": opt(&s.via), "error": opt(&s.error),
            "cancelling": s.withdrawal.is_some() && !s.state.is_final(), "createdAt": s.created_at, "updatedAt": s.updated_at,
        });
        if !s.cancel_error.is_empty() {
            v["cancelError"] = json!(s.cancel_error);
        }
        if s.state == State::Unknown {
            v["offer"] = json!(["rebroadcast", "request_fee_bump"]);
        }
        if !s.rebroadcasts.is_empty() {
            v["rebroadcasts"] = json!(s.rebroadcasts);
        }
        v
    }

    /// Newest first.
    pub fn list_json(&self) -> Value {
        let rows: Vec<Value> = self
            .map
            .values()
            .rev()
            .map(|s| {
                json!({"requestId": s.id, "kind": s.kind.name(), "state": s.state.name(), "requester": s.requester,
                       "approvalHandle": opt(&s.approval_handle), "txid": opt(&s.txid), "createdAt": s.created_at, "updatedAt": s.updated_at})
            })
            .collect();
        json!(rows)
    }

    fn prune(&mut self) {
        let finals: Vec<u64> = self.map.iter().filter(|(_, s)| s.state.is_final()).map(|(k, _)| *k).collect();
        if finals.len() >= KEEP_FINAL {
            for k in finals.iter().take(finals.len() + 1 - KEEP_FINAL) {
                self.map.remove(k);
            }
        }
    }
}

fn opt(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        json!(s)
    }
}

/// How a failed sign job settles. Unknown whenever the transaction may have left.
pub fn classify(error: &str, saw_settled: bool, saw_broadcasting: bool, withdrawal: Option<&Withdrawal>) -> (State, String) {
    let e = error.trim();
    // A source refused it, or none was there to try: the core kept no record of it as sent.
    if e.contains("refused the transaction") || e.contains("no source is enabled for broadcast") {
        return (State::Failed, e.into());
    }
    let after_broadcast = e.contains("the wallet is busy") || e.contains("the wallet closed") || e.contains("no wallet is open");
    if saw_broadcasting || (saw_settled && after_broadcast) {
        return (State::Unknown, format!("{MAY_HAVE_LEFT} ({e})"));
    }
    if let Some(w) = withdrawal {
        return w.settles_as();
    }
    if e.starts_with("rejected") {
        return (State::Failed, REJECTED.into());
    }
    if e.starts_with("expired_no_ack") {
        return (State::Failed, NO_ACK.into());
    }
    if e == "cancelled" {
        return (State::Failed, WITHDRAWN.into());
    }
    (State::Failed, e.into())
}

/// The outpoints a preview spends.
pub fn inputs_of(preview: &Value) -> Vec<String> {
    preview["inputs"].as_array().map(|a| a.iter().filter_map(|i| i["outpoint"].as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Inputs no longer among the wallet's confirmed coins.
pub fn gone_inputs(inputs: &[String], coins: &HashSet<String>) -> Vec<String> {
    inputs.iter().filter(|o| !coins.contains(*o)).cloned().collect()
}

/// The app's summary of a preview: who is paid, the change, the fee and what leaves the wallet.
pub fn summary(preview: &Value) -> Value {
    if preview.is_null() {
        return Value::Null;
    }
    let outputs = preview["outputs"].as_array().cloned().unwrap_or_default();
    let line = |o: &Value| {
        let sat = o["amountSat"].as_str().unwrap_or_default();
        json!({"address": o["address"], "amountSat": sat, "amountBtc": btc_of(sat)})
    };
    let sat = |o: &Value| o["amountSat"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let paid: Vec<&Value> = outputs.iter().filter(|o| o["change"] != true).collect();
    let change: Vec<&Value> = outputs.iter().filter(|o| o["change"] == true).collect();
    let fee = preview["feeSat"].as_str().unwrap_or_default();
    let total = paid.iter().map(|o| sat(o)).sum::<u64>().saturating_add(fee.parse::<u64>().unwrap_or(0));
    let inputs = preview["inputs"].as_array().cloned().unwrap_or_default();
    let input_sat: u64 = inputs.iter().map(sat).sum();
    json!({
        "kind": preview["kind"], "recipients": paid.iter().map(|o| line(o)).collect::<Vec<_>>(),
        "change": change.iter().map(|o| line(o)).collect::<Vec<_>>(),
        "inputs": inputs.len(), "inputSat": input_sat.to_string(), "inputBtc": crate::amount::format_btc(input_sat),
        "feeSat": fee, "feeBtc": btc_of(fee), "feeRate": preview["feeRate"], "vsize": preview["vsize"],
        "totalSat": total.to_string(), "totalBtc": crate::amount::format_btc(total),
        "txid": preview["txid"], "replaces": preview["replaces"], "warnings": preview["warnings"].clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(id: &str) -> Value {
        json!({"previewId": id, "kind": "send", "txid": "ab".repeat(32),
               "inputs": [{"outpoint": format!("{}:0", "cd".repeat(32)), "amountSat": "100000"}],
               "outputs": [{"address": "bcrt1qpay", "amountSat": "60000", "change": false},
                           {"address": "bcrt1qchange", "amountSat": "39859", "change": true}],
               "feeSat": "141", "feeRate": 1.0, "vsize": 141, "warnings": [], "replaces": null, "state": "built"})
    }

    fn started(s: &mut Sends) -> String {
        let id = s.begin(Kind::Send, "some_dapp", "regtest", "g_1", json!({}), 1).unwrap();
        s.set_build_job(&id, "b1");
        assert_eq!(s.built(&id, preview("p1")), Next::Sign("p1".into()));
        s.set_sign_job(&id, "b2");
        id
    }

    #[test]
    fn one_open_send_per_wallet() {
        let mut s = Sends::default();
        let a = s.begin(Kind::Send, "x", "regtest", "g_1", json!({}), 1).unwrap();
        assert!(s.begin(Kind::Send, "y", "regtest", "g_1", json!({}), 1).unwrap_err().contains(&a));
        assert!(s.begin(Kind::FeeBump, "y", "regtest", "g_1", json!({}), 1).is_err(), "a fee bump is a send too");
        assert!(s.begin(Kind::Send, "y", "regtest", "g_2", json!({}), 1).is_ok(), "another wallet");
        s.not_started(&a, "no wallet is open");
        assert_eq!(s.get(&a).unwrap().state, State::Failed);
        assert!(s.begin(Kind::Send, "y", "regtest", "g_1", json!({}), 1).is_ok(), "a final send frees the wallet");
    }

    #[test]
    fn an_approved_send_goes_preparing_awaiting_committing_sent() {
        let mut s = Sends::default();
        let id = started(&mut s);
        let t0 = Instant::now();
        assert_eq!(s.observe(&id, "built", None, t0), None);
        assert_eq!(s.observe(&id, "awaiting_approval", Some("smh_1"), t0), Some(State::AwaitingApproval));
        assert_eq!(s.observe(&id, "awaiting_approval:rendered", None, t0), None);
        assert_eq!(s.get(&id).unwrap().approval, "rendered");
        assert_eq!(s.observe(&id, "settled", None, t0), None);
        assert_eq!(s.observe(&id, "broadcasting", None, t0), Some(State::Committing));
        assert_eq!(s.observe(&id, "awaiting_approval", None, t0), None, "never backwards");
        assert_eq!(s.signed(&id, &json!({"txid": "ab".repeat(32), "broadcast": "seen", "via": "electrum-0"})), Some(State::Sent));
        let st = s.status_json(&id);
        assert_eq!((st["state"].clone(), st["approvalHandle"].clone(), st["via"].clone()), (json!("sent"), json!("smh_1"), json!("electrum-0")));
        assert_eq!(st["summary"]["totalSat"], "60141");
        assert_eq!(st["summary"]["recipients"][0]["amountBtc"], "0.00060000");
        assert_eq!(st["summary"]["change"][0]["address"], "bcrt1qchange");
    }

    #[test]
    fn a_broadcast_nobody_confirmed_is_unknown_not_sent() {
        let mut s = Sends::default();
        let id = started(&mut s);
        assert_eq!(s.signed(&id, &json!({"txid": "x", "broadcast": "unknown", "via": "electrum-1"})), Some(State::Unknown));
        let st = s.status_json(&id);
        assert_eq!(st["offer"], json!(["rebroadcast", "request_fee_bump"]));
        assert!(st["error"].as_str().unwrap().contains("never pay again"));
        assert_eq!(s.rebroadcast(&id, &json!({"broadcast": "requested"})), None);
        assert_eq!(s.rebroadcast(&id, &json!({"broadcast": "seen", "via": "electrum-0"})), Some(State::Sent));
        assert_eq!(s.status_json(&id)["rebroadcasts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn failures_say_why_and_release_the_inputs() {
        for (err, want) in [
            ("rejected: the person declined", REJECTED),
            ("expired_no_ack: nobody showed the request in time; ask again", NO_ACK),
            ("cancelled", WITHDRAWN),
            ("signer_manager_module: no answer", "signer_manager_module: no answer"),
        ] {
            let mut s = Sends::default();
            let id = started(&mut s);
            s.observe(&id, "awaiting_approval", Some("smh_1"), Instant::now());
            assert_eq!(s.sign_failed(&id, err), (Some(State::Failed), true), "{err}");
            assert_eq!(s.get(&id).unwrap().error, want);
        }
    }

    #[test]
    fn unknown_is_never_failed() {
        // Seen broadcasting: whatever the error, the transaction may have left.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "settled", None, Instant::now());
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.sign_failed(&id, "the wallet is busy; try again"), (Some(State::Unknown), false), "keep the inputs locked");
        // Approved, and an error the core can raise after broadcasting, without seeing it.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "settled", None, Instant::now());
        assert_eq!(s.sign_failed(&id, "the wallet closed").0, Some(State::Unknown));
        // A refusal is definitive: the core kept no record of it as sent.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.sign_failed(&id, "electrum-0 refused the transaction: min relay fee not met"), (Some(State::Failed), true));
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.sign_failed(&id, "no source is enabled for broadcast"), (Some(State::Failed), true), "nothing was tried");
        // The core vanished mid-commit.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.core_lost(&id), Some(State::Unknown));
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", None, Instant::now());
        assert_eq!(s.core_lost(&id), Some(State::Failed), "before approval nothing could have left");
    }

    #[test]
    fn cancelling_waits_for_the_core() {
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", Some("smh_1"), Instant::now());
        assert_eq!(s.withdraw(&id, Withdrawal::Caller("some_dapp".into())), Ok(Next::CancelPreview("p1".into())));
        assert_eq!(s.withdraw(&id, Withdrawal::Caller("some_dapp".into())), Ok(Next::Nothing), "once");
        assert_eq!(s.status_json(&id)["cancelling"], true);
        assert_eq!(s.cancel_done(&id), None, "the sign job is still winding down");
        assert_eq!(s.sign_failed(&id, "cancelled"), (Some(State::Cancelled), false), "already released");
        assert_eq!(s.get(&id).unwrap().error, "cancelled by some_dapp");
        assert!(s.withdraw(&id, Withdrawal::Caller("x".into())).unwrap_err().contains("already cancelled"));
    }

    #[test]
    fn a_send_going_out_cannot_be_cancelled() {
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.withdraw(&id, Withdrawal::Caller("x".into())).unwrap_err(), "too late: the transaction is going out");
        // The core's refusal arrives for a cancel sent just before broadcasting.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.withdraw(&id, Withdrawal::Caller("x".into())).unwrap();
        assert_eq!(s.cancel_failed(&id, "too late: the transaction is going out"), None);
        assert!(s.get(&id).unwrap().withdrawal.is_none());
        assert_eq!(s.signed(&id, &json!({"broadcast": "seen"})), Some(State::Sent), "what happened wins");
    }

    #[test]
    fn a_build_withdrawn_midway_is_cancelled_once_built() {
        let mut s = Sends::default();
        let id = s.begin(Kind::Send, "some_dapp", "regtest", "g_1", json!({}), 1).unwrap();
        s.set_build_job(&id, "b1");
        assert_eq!(s.withdraw(&id, Withdrawal::Caller("some_dapp".into())), Ok(Next::CancelBuild("b1".into())));
        assert_eq!(s.built(&id, preview("p1")), Next::CancelPreview("p1".into()), "its inputs were locked: release them");
        assert_eq!(s.cancel_done(&id), Some(State::Cancelled));
        // A build cancelled while queued settles straight away.
        let mut s = Sends::default();
        let id = s.begin(Kind::Send, "some_dapp", "regtest", "g_1", json!({}), 1).unwrap();
        s.set_build_job(&id, "b1");
        s.withdraw(&id, Withdrawal::WalletClosed).unwrap();
        assert_eq!(s.build_failed(&id, "cancelled"), Some(State::Failed));
        assert_eq!(s.get(&id).unwrap().error, WALLET_CLOSED);
    }

    #[test]
    fn an_offer_nobody_acknowledges_is_withdrawn_after_the_window() {
        let mut s = Sends::default();
        let id = started(&mut s);
        let t0 = Instant::now();
        s.observe(&id, "awaiting_approval:offered", Some("smh_1"), t0);
        assert!(s.overdue_acks(t0 + ACK_WINDOW).is_empty(), "the keystore's own expiry comes first");
        assert_eq!(s.overdue_acks(t0 + ACK_WINDOW + ACK_SLACK), vec![id.clone()]);
        s.withdraw(&id, Withdrawal::NoAck).unwrap();
        assert!(s.overdue_acks(t0 + ACK_WINDOW + ACK_SLACK).is_empty(), "withdrawn once");
        assert_eq!(s.sign_failed(&id, "cancelled").0, Some(State::Failed));
        assert_eq!(s.get(&id).unwrap().error, NO_ACK);
        // Once acknowledged, the person has no timeout.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval:offered", Some("smh_1"), t0);
        s.observe(&id, "awaiting_approval:rendered", None, t0);
        assert!(s.overdue_acks(t0 + Duration::from_secs(3600)).is_empty());
    }

    #[test]
    fn coins_that_change_underneath_withdraw_the_send() {
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", None, Instant::now());
        let watched = s.watched_inputs();
        assert_eq!(watched.len(), 1);
        let input = watched[0].1[0].clone();
        let mut coins: HashSet<String> = [input.clone(), "ef:1".to_string()].into();
        assert!(gone_inputs(&watched[0].1, &coins).is_empty(), "a new incoming coin changes nothing");
        coins.remove(&input);
        assert_eq!(gone_inputs(&watched[0].1, &coins), vec![input]);
        s.withdraw(&id, Withdrawal::CoinsChanged).unwrap();
        assert!(s.watched_inputs().is_empty());
        assert_eq!(s.sign_failed(&id, "cancelled").0, Some(State::Failed));
        assert_eq!(s.get(&id).unwrap().error, COINS_CHANGED);
    }

    #[test]
    fn listing_is_newest_first_and_bounded() {
        let mut s = Sends::default();
        for i in 0..(KEEP_FINAL + 5) {
            let id = s.begin(Kind::Send, "x", "regtest", "g_1", json!({}), i as u64).unwrap();
            s.not_started(&id, "no");
        }
        let list = s.list_json();
        assert!(list.as_array().unwrap().len() <= KEEP_FINAL);
        assert_eq!(list[0]["requestId"], format!("s{}", KEEP_FINAL + 5));
        assert_eq!(s.status_json("s1")["ok"], false, "the oldest final sends are dropped");
    }
}
