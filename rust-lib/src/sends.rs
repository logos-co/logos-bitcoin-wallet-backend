//! Sends from request to settlement, one open per wallet: preparing → awaiting_approval →
//! committing → sent, or failed, cancelled, unknown. Unknown means it may have left. Each seed's
//! sends are kept in sends.sealed.json under this module's key for its "sends" data, read only
//! while the seed is unlocked.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use logos_signer_interface::sealed::SealedDocs;
use logos_signer_interface::wallets::DataKey;
use serde_json::{json, Value};

use crate::amount::btc_of;

/// What the send lists are called in their boxes.
const NAME: &str = "bitcoin_wallet_backend sends";

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

    fn parse(s: &str) -> Option<Self> {
        [State::Preparing, State::AwaitingApproval, State::Committing, State::Sent, State::Failed, State::Cancelled, State::Unknown]
            .into_iter()
            .find(|x| x.name() == s)
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
    /// `(state, error, errorCode)`.
    fn settles_as(&self) -> (State, String, String) {
        match self {
            Withdrawal::Caller(who) => (State::Cancelled, format!("cancelled by {who}"), "cancelled".into()),
            Withdrawal::NoAck => (State::Failed, NO_ACK.into(), "expired".into()),
            Withdrawal::CoinsChanged => (State::Failed, COINS_CHANGED.into(), "coins_changed".into()),
            Withdrawal::WalletClosed => (State::Failed, WALLET_CLOSED.into(), "no_wallet".into()),
        }
    }

    fn to_wire(&self) -> String {
        match self {
            Withdrawal::Caller(who) => format!("caller:{who}"),
            Withdrawal::NoAck => "no_ack".into(),
            Withdrawal::CoinsChanged => "coins_changed".into(),
            Withdrawal::WalletClosed => "wallet_closed".into(),
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        match s {
            "no_ack" => Some(Withdrawal::NoAck),
            "coins_changed" => Some(Withdrawal::CoinsChanged),
            "wallet_closed" => Some(Withdrawal::WalletClosed),
            _ => s.strip_prefix("caller:").map(|w| Withdrawal::Caller(w.into())),
        }
    }
}

/// The keystore drops an offer nobody acknowledges after this long.
pub const ACK_WINDOW: Duration = Duration::from_secs(60);
/// Past the window plus this slack, the backend withdraws an unacknowledged offer itself.
pub const ACK_SLACK: Duration = Duration::from_secs(15);
/// How long a send found unconfirmed and unreported in history waits for a source to report it.
pub const REPORT_GRACE: Duration = Duration::from_secs(30);

pub const NO_ACK: &str = "expired: nobody acknowledged the approval within 60 s; request the send again";
pub const REJECTED: &str = "rejected: the person declined in the signer app";
pub const WITHDRAWN: &str = "cancelled: the approval request was withdrawn before anyone approved it";
pub const COINS_CHANGED: &str =
    "the wallet's coins changed while the send waited: an input is no longer the wallet's to spend; request the send again";
pub const WALLET_CLOSED: &str = "the wallet closed before the send was approved";
pub const CORE_LOST: &str = "the wallet core stopped answering before the send was approved";
pub const RESTARTED: &str = "the backend restarted and the wallet core no longer has this send; nothing went out; request it again";
pub const MAY_HAVE_LEFT: &str = "the transaction may have left: rebroadcast it or replace it with a higher fee; never pay again from other coins while it could still confirm";
pub const LOCKED: &str = "this wallet's sends aren't open here yet; try again in a moment";
pub const ABANDONED: &str = "Abandoned: its coins are free to spend again. If the original transaction still confirms, a later send from those coins will fail; if a later send confirms first, it replaces the original.";

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

/// What the core says about a send this backend instance has no job for (one it loaded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreView<'a> {
    /// The core still holds the preview, in this state.
    Preview { state: &'a str, handle: Option<&'a str> },
    /// The preview is gone. `history` is the open wallet's word on the txid, or None when the
    /// send's wallet isn't open; `readable` is false while it is open but busy.
    Gone { wallet_open: bool, history: Option<Seen>, readable: bool },
}

/// How the core's send record says the broadcast went: seen, requested or unknown, and the route.
/// Both empty from a core that doesn't say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub broadcast: String,
    pub via: String,
}

/// What the wallet's history says about a txid: absent; present but unreported; vouched for by a
/// source or a block; broadcast by the core's own send record; confirmed on the authority's chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    Absent,
    Unreported,
    Vouched,
    Recorded(Outcome),
    Confirmed(Outcome),
}

/// Reads a history row: the authority's chain, then the core's record when it names the outcome.
pub fn seen_in(row: &Value) -> Seen {
    let record = row["sendState"].as_str().unwrap_or_default();
    let text = |k: &str| row[k].as_str().unwrap_or_default().to_string();
    let outcome = Outcome { broadcast: text("broadcast"), via: text("via") };
    let went_out = matches!(record, "sent" | "replaced");
    if record == "confirmed" || row["state"] == "confirmed" {
        Seen::Confirmed(outcome)
    } else if went_out && !outcome.broadcast.is_empty() {
        Seen::Recorded(outcome)
    } else if row["confirmations"].as_u64().unwrap_or(0) > 0 || !row["reportedBy"].is_null() {
        Seen::Vouched
    } else if went_out {
        Seen::Recorded(outcome)
    } else {
        Seen::Unreported
    }
}

/// Seen or requested left; unknown may have. Empty: a core that doesn't say, whose record is believed.
fn left(o: &Outcome) -> bool {
    matches!(o.broadcast.as_str(), "" | "seen" | "requested")
}

/// Takes the core's outcome and route. Confirmed, an outcome short of seen or requested reads seen.
fn fill(s: &mut Send, o: &Outcome, confirmed: bool) {
    if !o.broadcast.is_empty() {
        s.broadcast = o.broadcast.clone();
    }
    if confirmed && !matches!(s.broadcast.as_str(), "seen" | "requested") {
        s.broadcast = "seen".into();
    }
    if !o.via.is_empty() {
        s.via = o.via.clone();
    }
}

#[derive(Clone, Debug)]
pub struct Send {
    pub id: String,
    pub kind: Kind,
    pub requester: String,
    pub network: String,
    pub group: String,
    /// The signer manager's wallet the send's wallet belongs to, whose key seals it.
    pub seed: String,
    pub state: State,
    pub error: String,
    pub error_code: String,
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
    pub sign_started: bool,
    /// This instance holds the core's jobs for it; false for a send loaded from sends.json.
    pub attached: bool,
    pub resign_tried: bool,
    pub unreported_since: Option<Instant>,
    pub abandoned: bool,
    /// Settled from the wallet's history after a restart, not from the core's answer.
    pub reconciled: bool,
    pub awaiting_since: Option<Instant>,
    pub created_at: u64,
    pub updated_at: u64,
    pub rebroadcasts: Vec<Value>,
}

/// Final sends kept for list_sends; older ones are dropped.
const KEEP_FINAL: usize = 100;

pub struct Sends {
    next: u64,
    map: BTreeMap<u64, Send>,
    dirty: bool,
    /// Each seed's sends, sealed; in memory only for a backend without a directory.
    docs: SealedDocs,
}

impl Default for Sends {
    fn default() -> Self {
        Sends { next: 0, map: BTreeMap::new(), dirty: false, docs: SealedDocs::in_memory(NAME) }
    }
}

fn num(id: &str) -> Option<u64> {
    id.strip_prefix('s')?.parse().ok()
}

impl Sends {
    /// The sends kept at `path`, each seed's sealed until its key comes ([`Sends::unlock`]).
    pub fn new(path: Option<PathBuf>) -> Self {
        let docs = match path {
            Some(p) => crate::open_sealed(p, NAME),
            None => SealedDocs::in_memory(NAME),
        };
        Sends { docs, ..Default::default() }
    }

    /// New ids start past `floor`. The backend passes the clock in thousandths of a second, so a
    /// locked seed's sends, unread, can't share an id with a new one.
    pub fn start_ids_above(&mut self, floor: u64) {
        self.next = self.next.max(floor);
    }

    /// Sealed here and its key not given yet: its sends are neither read nor written.
    pub fn is_locked(&self, seed: &str) -> bool {
        self.docs.wanted(seed).is_some()
    }

    /// The seeds whose sends wait for a key, each with the generation they are sealed under, if
    /// sealed yet: the keys to ask for.
    pub fn closed(&self) -> Vec<(String, Option<u32>)> {
        let mut seeds = self.docs.seeds();
        seeds.extend(self.map.values().map(|s| s.seed.clone()));
        seeds.sort();
        seeds.dedup();
        seeds.into_iter().filter(|s| !self.docs.is_open(s)).map(|s| (s.clone(), self.docs.wanted(&s))).collect()
    }

    /// The seeds whose sends can be read now.
    pub fn readable_seeds(&self) -> Vec<String> {
        self.docs.readable().into_iter().map(|(s, _)| s).collect()
    }

    /// A seed's key came: its sends load, sealed again under `current`. A send this instance holds
    /// already is newer than its stored copy and stays; an open one loaded is reconciled.
    pub fn unlock(&mut self, seed: &str, box_key: &DataKey, current: &DataKey) -> Result<(), String> {
        self.docs.unlock(seed, box_key, current).map_err(|e| e.message)?;
        let doc: Value = match self.docs.get(seed).filter(|b| !b.is_empty()) {
            Some(b) => serde_json::from_slice(b).map_err(|e| format!("sends.sealed.json: {e}"))?,
            None => Value::Null,
        };
        self.merge(&doc, Some(seed), Instant::now());
        self.dirty = true;
        Ok(())
    }

    /// Wallets were locked: every key goes. Sends in memory stay until [`Sends::drop_locked`].
    pub fn lock(&mut self) {
        self.docs.lock();
    }

    /// Open sends of seeds locked here that can still be withdrawn: none going out or withdrawn.
    pub fn locked_waiting(&self) -> Vec<String> {
        self.map
            .values()
            .filter(|s| !s.state.is_final() && s.state != State::Committing && !s.saw_broadcasting && s.withdrawal.is_none())
            .filter(|s| self.is_locked(&s.seed))
            .map(|s| s.id.clone())
            .collect()
    }

    /// Forgets the settled sends of seeds locked here, but those a job still runs for. Their stored
    /// copies come back once the seed unlocks. The number dropped.
    pub fn drop_locked(&mut self, busy: &HashSet<String>) -> usize {
        let gone: Vec<u64> = self.map.iter().filter(|(_, s)| s.state.is_final() && !busy.contains(&s.id) && self.is_locked(&s.seed)).map(|(k, _)| *k).collect();
        for k in &gone {
            self.map.remove(k);
        }
        gone.len()
    }

    /// Seals each seed's sends whose stored copy differs. A seed locked here is skipped: it is
    /// written once its key comes. One whose key never came yet waits in memory.
    pub fn save(&mut self) -> Result<(), String> {
        let mut seeds: Vec<String> = self.map.values().map(|s| s.seed.clone()).collect();
        seeds.extend(self.readable_seeds());
        seeds.sort();
        seeds.dedup();
        seeds.retain(|s| !s.is_empty() && !self.is_locked(s));
        for seed in &seeds {
            let body = serde_json::to_vec(&self.doc(seed)).map_err(|e| e.to_string())?;
            if self.docs.get(seed) != Some(body.as_slice()) {
                self.docs.put(seed, body).map_err(|e| format!("sends.sealed.json: {}", e.message))?;
            }
        }
        Ok(())
    }

    /// One seed's sends, as its document holds them.
    fn doc(&self, seed: &str) -> Value {
        let sends: Vec<Value> = self.map.values().filter(|s| s.seed == seed).map(wire).collect();
        json!({"version": 1, "sends": sends})
    }

    pub fn get(&self, id: &str) -> Option<&Send> {
        self.map.get(&num(id)?)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Send> {
        self.dirty = true;
        self.map.get_mut(&num(id)?)
    }

    /// True once since the last call if anything worth saving changed.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
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

    /// Open sends loaded from disk that no job of this instance follows.
    pub fn orphans(&self) -> Vec<String> {
        self.map.values().filter(|s| !s.state.is_final() && !s.attached).map(|s| s.id.clone()).collect()
    }

    /// A new send in `preparing`; refused while the wallet has another open, or while its seed's
    /// sends are sealed here, where one could be.
    #[allow(clippy::too_many_arguments)]
    pub fn begin(&mut self, kind: Kind, requester: &str, network: &str, group: &str, seed: &str, request: Value, now: u64) -> Result<String, String> {
        if seed.is_empty() {
            return Err("the wallet core names no seed for the open wallet".into());
        }
        if self.is_locked(seed) {
            return Err(LOCKED.into());
        }
        if let Some(open) = self.open_for(network, group) {
            return Err(format!("send {open} is still open: one send at a time per wallet"));
        }
        self.prune();
        self.next += 1;
        self.dirty = true;
        let id = format!("s{}", self.next);
        self.map.insert(
            self.next,
            Send {
                id: id.clone(),
                kind,
                requester: requester.into(),
                network: network.into(),
                group: group.into(),
                seed: seed.into(),
                state: State::Preparing,
                error: String::new(),
                error_code: String::new(),
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
                sign_started: false,
                attached: true,
                resign_tried: false,
                unreported_since: None,
                abandoned: false,
                reconciled: false,
                awaiting_since: None,
                created_at: now,
                updated_at: now,
                rebroadcasts: vec![],
            },
        );
        Ok(id)
    }

    fn settle(s: &mut Send, state: State, error: String, code: String) -> Option<State> {
        if s.state == state && s.error == error && s.error_code == code {
            return None;
        }
        s.state = state;
        s.error = error;
        s.error_code = code;
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
            s.sign_started = true;
            s.attached = true;
        }
    }

    /// The core couldn't take a job for this send: nothing was built or asked.
    pub fn not_started(&mut self, id: &str, error: &str, code: Option<&str>) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final())?;
        Self::settle(s, State::Failed, error.into(), code.map(String::from).unwrap_or_else(|| code_of(error).into()))
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
            Self::settle(s, State::Failed, "the wallet core returned no preview".into(), "failed".into());
            return Next::Nothing;
        }
        if s.state.is_final() || s.withdrawal.is_some() {
            s.preview_cancelled = true;
            return Next::CancelPreview(s.preview_id.clone());
        }
        Next::Sign(s.preview_id.clone())
    }

    pub fn build_failed(&mut self, id: &str, error: &str, code: Option<&str>) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final())?;
        s.build_job = None;
        let (state, why, c) = match &s.withdrawal {
            Some(w) => w.settles_as(),
            None => (State::Failed, error.to_string(), code.map(String::from).unwrap_or_else(|| code_of(error).into())),
        };
        Self::settle(s, state, why, c)
    }

    /// Follows the core's preview state while the sign job runs. Never moves backwards.
    pub fn observe(&mut self, id: &str, preview_state: &str, handle: Option<&str>, now: Instant) -> Option<State> {
        let s = self.map.get_mut(&num(id)?).filter(|s| !s.state.is_final())?;
        if let Some(h) = handle.filter(|h| !h.is_empty() && *h != s.approval_handle) {
            s.approval_handle = h.into();
            self.dirty = true;
        }
        let (stage, sub) = preview_state.split_once(':').unwrap_or((preview_state, ""));
        let changed = match stage {
            "awaiting_approval" if s.state == State::Preparing || s.state == State::AwaitingApproval => {
                let a = if sub.is_empty() { "offered" } else { sub };
                let moved = s.approval != a;
                s.approval = a.into();
                if s.state == State::Preparing {
                    s.awaiting_since = Some(now);
                    Self::settle(s, State::AwaitingApproval, String::new(), String::new())
                } else {
                    self.dirty |= moved;
                    None
                }
            }
            "settled" if !s.saw_settled => {
                s.saw_settled = true;
                s.approval = "settled".into();
                self.dirty = true;
                None
            }
            "broadcasting" if s.state != State::Committing => {
                s.saw_broadcasting = true;
                Self::settle(s, State::Committing, String::new(), String::new())
            }
            _ => None,
        };
        self.dirty |= changed.is_some();
        changed
    }

    /// The sign job finished: seen or requested is sent; unknown may have left.
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
            "seen" | "requested" => Self::settle(s, State::Sent, String::new(), String::new()),
            _ => Self::settle(s, State::Unknown, MAY_HAVE_LEFT.into(), String::new()),
        }
    }

    /// The sign job failed. Returns the new state, and whether the preview's inputs should be
    /// released (they stay locked while the transaction may have left).
    pub fn sign_failed(&mut self, id: &str, error: &str, code: Option<&str>) -> (Option<State>, bool) {
        let Some(s) = self.get_mut(id).filter(|s| !s.state.is_final()) else { return (None, false) };
        s.sign_job = None;
        let (state, why, c) = classify(error, code, s.saw_settled, s.saw_broadcasting, s.withdrawal.as_ref());
        let release = state != State::Unknown && !s.preview_id.is_empty() && !s.preview_cancelled;
        s.preview_cancelled |= release;
        (Self::settle(s, state, why, c), release)
    }

    /// The core stopped answering while this send's job ran.
    pub fn core_lost(&mut self, id: &str) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final())?;
        s.build_job = None;
        s.sign_job = None;
        if s.state == State::Committing || s.saw_settled {
            return Self::settle(s, State::Unknown, format!("{MAY_HAVE_LEFT} (the wallet core stopped answering)"), String::new());
        }
        Self::settle(s, State::Failed, CORE_LOST.into(), "core_lost".into())
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
        let (state, why, c) = s.withdrawal.as_ref().map(|w| w.settles_as()).unwrap_or((State::Cancelled, String::new(), "cancelled".into()));
        Self::settle(s, state, why, c)
    }

    /// The core refused the cancel. Too late means the transaction is going out.
    pub fn cancel_failed(&mut self, id: &str, error: &str) -> Option<State> {
        let s = self.get_mut(id).filter(|s| !s.state.is_final())?;
        if error.contains("too late") {
            s.withdrawal = None;
            s.cancel_error = error.into();
            return None;
        }
        // A loaded send whose preview is gone may have been signed: reconciling decides.
        if s.sign_job.is_some() || s.build_job.is_some() || (!s.attached && s.sign_started) {
            return None;
        }
        // The preview is gone and no job runs for it: nothing can go out any more.
        let (state, why, c) = s.withdrawal.as_ref().map(|w| w.settles_as()).unwrap_or((State::Failed, error.to_string(), code_of(error).into()));
        Self::settle(s, state, why, c)
    }

    /// One reconciling step for a loaded send, from what the core says now.
    pub fn reconcile(&mut self, id: &str, view: CoreView, now: Instant) -> (Option<State>, Next) {
        let Some(s) = self.map.get(&num(id).unwrap_or(0)).filter(|s| !s.state.is_final() && !s.attached) else { return (None, Next::Nothing) };
        let (pid, withdrawn) = (s.preview_id.clone(), s.withdrawal.is_some());
        if pid.is_empty() {
            // Lost mid-build: a preview the core made since dies with its session.
            let s = self.get_mut(id).expect("present");
            return (Self::settle(s, State::Failed, RESTARTED.into(), "backend_restarted".into()), Next::Nothing);
        }
        match view {
            CoreView::Preview { state, handle } => {
                if let Some(e) = state.strip_prefix("failed:") {
                    let (st, release) = self.sign_failed(id, e.trim(), None);
                    return (st, if release { Next::CancelPreview(pid) } else { Next::Nothing });
                }
                if withdrawn && state != "broadcasting" {
                    let s = self.get_mut(id).expect("present");
                    if !std::mem::replace(&mut s.resign_tried, true) {
                        s.preview_cancelled = true;
                        return (None, Next::CancelPreview(pid));
                    }
                    return (None, Next::Nothing);
                }
                if state == "built" {
                    let s = self.get_mut(id).expect("present");
                    if !std::mem::replace(&mut s.resign_tried, true) {
                        return (None, Next::Sign(pid));
                    }
                    return (None, Next::Nothing);
                }
                (self.observe(id, state, handle, now), Next::Nothing)
            }
            CoreView::Gone { wallet_open, history, readable } => {
                let s = self.get_mut(id).expect("present");
                let settled = match (wallet_open, history) {
                    (true, None) | (true, Some(_)) if !readable => return (None, Next::Nothing),
                    (true, Some(Seen::Confirmed(o))) => {
                        fill(s, &o, true);
                        Some((State::Sent, String::new(), String::new()))
                    }
                    // The core's record decides, as its answer would have: Kyoto reports nothing to wait for.
                    (true, Some(Seen::Recorded(o))) => {
                        fill(s, &o, false);
                        Some(if left(&o) { (State::Sent, String::new(), String::new()) } else { (State::Unknown, MAY_HAVE_LEFT.to_string(), String::new()) })
                    }
                    (true, Some(Seen::Vouched)) => {
                        s.broadcast = "seen".into();
                        Some((State::Sent, String::new(), String::new()))
                    }
                    // A core without send records: a source must report it.
                    (true, Some(Seen::Unreported)) => {
                        let since = *s.unreported_since.get_or_insert(now);
                        (now.saturating_duration_since(since) >= REPORT_GRACE).then(|| (State::Unknown, MAY_HAVE_LEFT.to_string(), String::new()))
                    }
                    (true, _) => Some(if s.saw_broadcasting {
                        (State::Unknown, MAY_HAVE_LEFT.to_string(), String::new())
                    } else if let Some(w) = &s.withdrawal {
                        w.settles_as()
                    } else {
                        (State::Failed, RESTARTED.to_string(), "backend_restarted".to_string())
                    }),
                    (false, _) => Some(if s.saw_broadcasting || (s.sign_started && s.withdrawal.is_none()) {
                        (State::Unknown, format!("{MAY_HAVE_LEFT} (the backend restarted while it was signed; reopen the wallet to see whether it went out)"), String::new())
                    } else if let Some(w) = &s.withdrawal {
                        w.settles_as()
                    } else {
                        (State::Failed, RESTARTED.to_string(), "backend_restarted".to_string())
                    }),
                };
                match settled {
                    Some((st, why, c)) => {
                        s.reconciled = true;
                        (Self::settle(s, st, why, c), Next::Nothing)
                    }
                    None => (None, Next::Nothing),
                }
            }
        }
    }

    /// Final sends of this wallet the history may still show went out: unknown ones, and those a
    /// restart settled without the core's answer. `(id, txid)`; a newer send with the txid wins.
    pub fn unsettled(&self, network: &str, group: &str) -> Vec<(String, String)> {
        self.map
            .values()
            .filter(|s| s.state.is_final() && s.state != State::Sent && (s.state == State::Unknown || s.reconciled))
            .filter(|s| !s.txid.is_empty() && s.network == network && s.group == group)
            .filter(|s| self.by_txid(&s.txid) == Some(s.id.as_str()))
            .map(|s| (s.id.clone(), s.txid.clone()))
            .collect()
    }

    /// Confirmed is sent, whatever the send read before. The core's record or a source makes it sent
    /// too, unless the core said unknown or the send was abandoned: those wait for a block.
    pub fn upgrade(&mut self, id: &str, seen: &Seen) -> Option<State> {
        let s = self.map.get(&num(id)?).filter(|s| s.state.is_final() && s.state != State::Sent)?;
        let believed = !s.abandoned && s.broadcast != "unknown";
        let sent = match seen {
            Seen::Confirmed(_) => true,
            Seen::Recorded(o) => believed && left(o),
            Seen::Vouched => believed,
            Seen::Absent | Seen::Unreported => false,
        };
        if !sent {
            return None;
        }
        let s = self.get_mut(id)?;
        match seen {
            Seen::Confirmed(o) => fill(s, o, true),
            Seen::Recorded(o) => fill(s, o, false),
            _ => s.broadcast = "seen".into(),
        }
        Self::settle(s, State::Sent, String::new(), String::new())
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

    /// Open sends with a preview, and the outpoints they spend. Not fee bumps: they spend the coins of
    /// the unconfirmed send they replace, never among the wallet's coins; if it confirms, broadcast refuses.
    pub fn watched_inputs(&self) -> Vec<(String, Vec<String>)> {
        self.map
            .values()
            .filter(|s| matches!(s.state, State::Preparing | State::AwaitingApproval) && s.withdrawal.is_none() && !s.preview_id.is_empty())
            .filter(|s| s.kind != Kind::FeeBump)
            .map(|s| (s.id.clone(), inputs_of(&s.preview)))
            .collect()
    }

    /// Records a rebroadcast; an unknown send the network has now seen is sent.
    pub fn rebroadcast(&mut self, id: &str, result: &Value) -> Option<State> {
        let s = self.get_mut(id)?;
        s.rebroadcasts.push(json!({"at": crate::now_secs(), "broadcast": result["broadcast"], "via": result["via"]}));
        s.updated_at = crate::now_secs();
        if s.state == State::Unknown && !s.abandoned && result["broadcast"] == "seen" {
            s.broadcast = "seen".into();
            return Self::settle(s, State::Sent, String::new(), String::new());
        }
        None
    }

    /// The core gave up an unknown send's transaction: its coins are free. The state stays unknown.
    pub fn abandoned(&mut self, id: &str) {
        if let Some(s) = self.get_mut(id) {
            s.abandoned = true;
            s.updated_at = crate::now_secs();
        }
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
            "errorCode": opt(&s.error_code), "cancelling": s.withdrawal.is_some() && !s.state.is_final(),
            "createdAt": s.created_at, "updatedAt": s.updated_at,
        });
        if !s.cancel_error.is_empty() {
            v["cancelError"] = json!(s.cancel_error);
        }
        if s.state == State::Unknown {
            v["abandoned"] = json!(s.abandoned);
            v["offer"] = if s.abandoned { json!([]) } else { json!(["rebroadcast", "request_fee_bump", "abandon_send"]) };
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
                       "approvalHandle": opt(&s.approval_handle), "txid": opt(&s.txid), "errorCode": opt(&s.error_code),
                       "createdAt": s.created_at, "updatedAt": s.updated_at})
            })
            .collect();
        json!(rows)
    }

    /// Every send as a seed's document holds it, with the id counter: no receipt, key or password;
    /// the core's job ids stay in memory.
    pub fn to_json(&self) -> Value {
        let sends: Vec<Value> = self.map.values().map(wire).collect();
        json!({"version": 1, "next": self.next, "sends": sends})
    }

    /// Sends as [`Sends::to_json`] wrote them. An open one comes back unattached, to be reconciled
    /// with the core.
    pub fn load(v: &Value, now: Instant) -> Self {
        let mut out = Sends { next: v["next"].as_u64().unwrap_or(0), ..Default::default() };
        out.merge(v, None, now);
        out
    }

    /// Adds the sends of a document that this instance doesn't hold. With `seed`, it is that
    /// seed's document; a send without a seed is dropped.
    fn merge(&mut self, v: &Value, seed: Option<&str>, now: Instant) {
        let text = |s: &Value, k: &str| s[k].as_str().unwrap_or_default().to_string();
        for s in v["sends"].as_array().into_iter().flatten() {
            let (Some(n), Some(state)) = (num(s["id"].as_str().unwrap_or_default()), State::parse(s["state"].as_str().unwrap_or_default())) else { continue };
            let seed = seed.map(String::from).unwrap_or_else(|| text(s, "seed"));
            if seed.is_empty() || self.map.contains_key(&n) {
                continue;
            }
            self.next = self.next.max(n);
            self.map.insert(
                n,
                Send {
                    id: text(s, "id"),
                    kind: if s["kind"] == "fee_bump" { Kind::FeeBump } else { Kind::Send },
                    requester: text(s, "requester"),
                    network: text(s, "network"),
                    group: text(s, "group"),
                    seed,
                    state,
                    error: text(s, "error"),
                    error_code: text(s, "errorCode"),
                    request: s["request"].clone(),
                    preview: s["preview"].clone(),
                    preview_id: text(s, "previewId"),
                    txid: text(s, "txid"),
                    approval_handle: text(s, "approvalHandle"),
                    approval: text(s, "approval"),
                    broadcast: text(s, "broadcast"),
                    via: text(s, "via"),
                    build_job: None,
                    sign_job: None,
                    withdrawal: s["withdrawal"].as_str().and_then(Withdrawal::from_wire),
                    preview_cancelled: s["previewCancelled"] == true,
                    cancel_error: text(s, "cancelError"),
                    saw_settled: s["sawSettled"] == true,
                    saw_broadcasting: s["sawBroadcasting"] == true,
                    sign_started: s["signStarted"] == true,
                    attached: state.is_final(),
                    resign_tried: false,
                    unreported_since: None,
                    abandoned: s["abandoned"] == true,
                    reconciled: s["reconciled"] == true,
                    awaiting_since: (state == State::AwaitingApproval).then_some(now),
                    created_at: s["createdAt"].as_u64().unwrap_or(0),
                    updated_at: s["updatedAt"].as_u64().unwrap_or(0),
                    rebroadcasts: s["rebroadcasts"].as_array().cloned().unwrap_or_default(),
                },
            );
        }
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

/// A send as its seed's document holds it.
fn wire(s: &Send) -> Value {
    json!({
        "id": s.id, "kind": s.kind.name(), "requester": s.requester, "network": s.network, "group": s.group, "seed": s.seed,
        "state": s.state.name(), "error": s.error, "errorCode": s.error_code, "request": s.request,
        "preview": s.preview, "previewId": s.preview_id, "txid": s.txid, "approvalHandle": s.approval_handle,
        "approval": s.approval, "broadcast": s.broadcast, "via": s.via,
        "withdrawal": s.withdrawal.as_ref().map(Withdrawal::to_wire), "previewCancelled": s.preview_cancelled,
        "cancelError": s.cancel_error, "sawSettled": s.saw_settled, "sawBroadcasting": s.saw_broadcasting,
        "signStarted": s.sign_started, "abandoned": s.abandoned, "reconciled": s.reconciled, "createdAt": s.created_at,
        "updatedAt": s.updated_at, "rebroadcasts": s.rebroadcasts,
    })
}

fn opt(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        json!(s)
    }
}

/// A failure's code from its text, for a core that sends none; the core's own rules.
pub fn code_of(e: &str) -> &'static str {
    let l = e.to_ascii_lowercase();
    if l.starts_with("rejected") {
        "rejected"
    } else if l.starts_with("expired_no_ack") || l.starts_with("expired") {
        "expired"
    } else if l.starts_with("cancelled") {
        "cancelled"
    } else if l.contains("the smallest amount") && l.contains("can receive") {
        "dust"
    } else if l.contains("insufficient funds") {
        "insufficient_funds"
    } else if l.contains("no source is enabled for broadcast") || l.contains("no broadcast source") || l.contains("no history server") || l.contains("no tip") {
        "no_route"
    } else if l.contains("is busy") {
        "busy"
    } else if l.contains("refused the transaction") {
        "refused"
    } else if l.contains("no wallet is open") || l.contains("the wallet closed") {
        "no_wallet"
    } else if l.starts_with("signer_manager_module:") || l.contains("signer manager") {
        "signer_unavailable"
    } else if l.starts_with("params") || l.contains("unknown field") || l.contains("is required") || l.contains("must be") || l.contains("unknown or") {
        "invalid"
    } else {
        "failed"
    }
}

/// How a failed sign job settles: `(state, error, errorCode)`. A core that sends a code never
/// fails a job once a broadcast may have happened; for one that doesn't, the text decides.
pub fn classify(error: &str, code: Option<&str>, saw_settled: bool, saw_broadcasting: bool, withdrawal: Option<&Withdrawal>) -> (State, String, String) {
    let e = error.trim();
    let code = match code.filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        None => {
            let after_broadcast = e.contains("the wallet is busy") || e.contains("the wallet closed") || e.contains("no wallet is open");
            let definitive = e.contains("refused the transaction") || e.contains("no source is enabled for broadcast");
            if !definitive && (saw_broadcasting || (saw_settled && after_broadcast)) {
                return (State::Unknown, format!("{MAY_HAVE_LEFT} ({e})"), String::new());
            }
            code_of(e).to_string()
        }
    };
    if let Some(w) = withdrawal {
        return w.settles_as();
    }
    match code.as_str() {
        "rejected" => (State::Failed, REJECTED.into(), code),
        "expired" => (State::Failed, NO_ACK.into(), code),
        "cancelled" => (State::Failed, WITHDRAWN.into(), code),
        _ => (State::Failed, e.into(), code),
    }
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
        let id = s.begin(Kind::Send, "some_dapp", "regtest", "g_1", "seed_1", json!({}), 1).unwrap();
        s.set_build_job(&id, "b1");
        assert_eq!(s.built(&id, preview("p1")), Next::Sign("p1".into()));
        s.set_sign_job(&id, "b2");
        id
    }

    fn told(broadcast: &str, via: &str) -> Outcome {
        Outcome { broadcast: broadcast.into(), via: via.into() }
    }

    /// The same sends, as a restarted backend loads them.
    fn reloaded(s: &Sends) -> Sends {
        let text = s.to_json().to_string();
        Sends::load(&serde_json::from_str(&text).unwrap(), Instant::now())
    }

    #[test]
    fn one_open_send_per_wallet() {
        let mut s = Sends::default();
        let a = s.begin(Kind::Send, "x", "regtest", "g_1", "seed_1", json!({}), 1).unwrap();
        assert!(s.begin(Kind::Send, "y", "regtest", "g_1", "seed_1", json!({}), 1).unwrap_err().contains(&a));
        assert!(s.begin(Kind::FeeBump, "y", "regtest", "g_1", "seed_1", json!({}), 1).is_err(), "a fee bump is a send too");
        assert!(s.begin(Kind::Send, "y", "regtest", "g_2", "seed_1", json!({}), 1).is_ok(), "another wallet");
        s.not_started(&a, "no wallet is open", None);
        assert_eq!((s.get(&a).unwrap().state, s.get(&a).unwrap().error_code.as_str()), (State::Failed, "no_wallet"));
        assert!(s.begin(Kind::Send, "y", "regtest", "g_1", "seed_1", json!({}), 1).is_ok(), "a final send frees the wallet");
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
        assert_eq!((st["errorCode"].clone(), st["summary"]["totalSat"].clone()), (Value::Null, json!("60141")));
        assert_eq!(st["summary"]["recipients"][0]["amountBtc"], "0.00060000");
        assert_eq!(st["summary"]["change"][0]["address"], "bcrt1qchange");
        let mut s = Sends::default();
        let id = started(&mut s);
        assert_eq!(s.signed(&id, &json!({"broadcast": "requested"})), Some(State::Sent), "requested is sent");
    }

    #[test]
    fn a_broadcast_nobody_confirmed_is_unknown_not_sent() {
        let mut s = Sends::default();
        let id = started(&mut s);
        assert_eq!(s.signed(&id, &json!({"txid": "x", "broadcast": "unknown", "via": "electrum-1"})), Some(State::Unknown));
        let st = s.status_json(&id);
        assert_eq!(st["offer"], json!(["rebroadcast", "request_fee_bump", "abandon_send"]));
        assert!(st["error"].as_str().unwrap().contains("never pay again"));
        assert_eq!(s.rebroadcast(&id, &json!({"broadcast": "requested"})), None);
        assert_eq!(s.rebroadcast(&id, &json!({"broadcast": "seen", "via": "electrum-0"})), Some(State::Sent));
        assert_eq!(s.status_json(&id)["rebroadcasts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn an_abandoned_send_stays_unknown_and_offers_nothing() {
        let mut s = Sends::default();
        let id = started(&mut s);
        s.signed(&id, &json!({"txid": "x", "broadcast": "unknown"}));
        s.abandoned(&id);
        let st = s.status_json(&id);
        assert_eq!((st["state"].clone(), st["abandoned"].clone(), st["offer"].clone()), (json!("unknown"), json!(true), json!([])));
        assert_eq!(s.rebroadcast(&id, &json!({"broadcast": "seen"})), None, "it was given up");
    }

    #[test]
    fn codes_decide_and_text_is_the_fallback() {
        for (err, code, want, want_code) in [
            ("rejected: the person declined", Some("rejected"), REJECTED, "rejected"),
            ("expired_no_ack: nobody showed the request in time; ask again", Some("expired"), NO_ACK, "expired"),
            ("cancelled", Some("cancelled"), WITHDRAWN, "cancelled"),
            ("cannot build the send: Insufficient funds", Some("insufficient_funds"), "cannot build the send: Insufficient funds", "insufficient_funds"),
            ("electrum-0 refused the transaction", Some("refused"), "electrum-0 refused the transaction", "refused"),
            ("rejected: the person declined", None, REJECTED, "rejected"),
            ("signer_manager_module: no answer", None, "signer_manager_module: no answer", "signer_unavailable"),
            ("no source is enabled for broadcast", None, "no source is enabled for broadcast", "no_route"),
        ] {
            let mut s = Sends::default();
            let id = started(&mut s);
            s.observe(&id, "awaiting_approval", Some("smh_1"), Instant::now());
            assert_eq!(s.sign_failed(&id, err, code), (Some(State::Failed), true), "{err}");
            let st = s.status_json(&id);
            assert_eq!((st["error"].as_str().unwrap(), st["errorCode"].as_str().unwrap()), (want, want_code), "{err}");
        }
    }

    #[test]
    fn unknown_is_never_failed() {
        // An older core, seen broadcasting: whatever the error, the transaction may have left.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "settled", None, Instant::now());
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.sign_failed(&id, "the wallet is busy; try again", None), (Some(State::Unknown), false), "keep the inputs locked");
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "settled", None, Instant::now());
        assert_eq!(s.sign_failed(&id, "the wallet closed", None).0, Some(State::Unknown));
        // A refusal, or nowhere to send it, is definitive.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.sign_failed(&id, "electrum-0 refused the transaction: min relay fee not met", None), (Some(State::Failed), true));
        // A core with codes never fails a job that may have broadcast, so its codes are believed.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.sign_failed(&id, "no source is enabled for broadcast", Some("no_route")), (Some(State::Failed), true));
        // The core vanished mid-commit.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.core_lost(&id), Some(State::Unknown));
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", None, Instant::now());
        assert_eq!(s.core_lost(&id), Some(State::Failed), "before approval nothing could have left");
        assert_eq!(s.get(&id).unwrap().error_code, "core_lost");
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
        assert_eq!(s.sign_failed(&id, "cancelled", Some("cancelled")), (Some(State::Cancelled), false), "already released");
        assert_eq!((s.get(&id).unwrap().error.as_str(), s.get(&id).unwrap().error_code.as_str()), ("cancelled by some_dapp", "cancelled"));
        assert!(s.withdraw(&id, Withdrawal::Caller("x".into())).unwrap_err().contains("already cancelled"));
    }

    #[test]
    fn a_send_going_out_cannot_be_cancelled() {
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.withdraw(&id, Withdrawal::Caller("x".into())).unwrap_err(), "too late: the transaction is going out");
        let mut s = Sends::default();
        let id = started(&mut s);
        s.withdraw(&id, Withdrawal::Caller("x".into())).unwrap();
        assert_eq!(s.cancel_failed(&id, "too late: the transaction is going out"), None);
        assert!(s.get(&id).unwrap().withdrawal.is_none());
        assert_eq!(s.signed(&id, &json!({"broadcast": "seen"})), Some(State::Sent), "what happened wins");
    }

    #[test]
    fn a_build_refused_as_dust_keeps_the_cores_words() {
        const DUST: &str = "The smallest amount this address can receive is 294 sats.";
        for code in [Some("dust"), None] {
            let mut s = Sends::default();
            let id = s.begin(Kind::Send, "some_dapp", "regtest", "g_1", "seed_1", json!({}), 1).unwrap();
            s.set_build_job(&id, "b1");
            assert_eq!(s.build_failed(&id, DUST, code), Some(State::Failed));
            let st = s.status_json(&id);
            assert_eq!((st["error"].as_str(), st["errorCode"].as_str()), (Some(DUST), Some("dust")), "{code:?}");
        }
    }

    #[test]
    fn a_build_withdrawn_midway_is_cancelled_once_built() {
        let mut s = Sends::default();
        let id = s.begin(Kind::Send, "some_dapp", "regtest", "g_1", "seed_1", json!({}), 1).unwrap();
        s.set_build_job(&id, "b1");
        assert_eq!(s.withdraw(&id, Withdrawal::Caller("some_dapp".into())), Ok(Next::CancelBuild("b1".into())));
        assert_eq!(s.built(&id, preview("p1")), Next::CancelPreview("p1".into()), "its inputs were locked: release them");
        assert_eq!(s.cancel_done(&id), Some(State::Cancelled));
        let mut s = Sends::default();
        let id = s.begin(Kind::Send, "some_dapp", "regtest", "g_1", "seed_1", json!({}), 1).unwrap();
        s.set_build_job(&id, "b1");
        s.withdraw(&id, Withdrawal::WalletClosed).unwrap();
        assert_eq!(s.build_failed(&id, "cancelled", Some("cancelled")), Some(State::Failed));
        assert_eq!((s.get(&id).unwrap().error.as_str(), s.get(&id).unwrap().error_code.as_str()), (WALLET_CLOSED, "no_wallet"));
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
        assert_eq!(s.sign_failed(&id, "cancelled", Some("cancelled")).0, Some(State::Failed));
        assert_eq!((s.get(&id).unwrap().error.as_str(), s.get(&id).unwrap().error_code.as_str()), (NO_ACK, "expired"));
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval:offered", Some("smh_1"), t0);
        s.observe(&id, "awaiting_approval:rendered", None, t0);
        assert!(s.overdue_acks(t0 + Duration::from_secs(3600)).is_empty(), "once acknowledged, no timeout");
    }

    #[test]
    fn coins_that_change_underneath_withdraw_the_send() {
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", None, Instant::now());
        let watched = s.watched_inputs();
        let input = watched[0].1[0].clone();
        let mut coins: HashSet<String> = [input.clone(), "ef:1".to_string()].into();
        assert!(gone_inputs(&watched[0].1, &coins).is_empty(), "a new incoming coin changes nothing");
        coins.remove(&input);
        assert_eq!(gone_inputs(&watched[0].1, &coins), vec![input]);
        s.withdraw(&id, Withdrawal::CoinsChanged).unwrap();
        assert!(s.watched_inputs().is_empty());
        assert_eq!(s.sign_failed(&id, "cancelled", Some("cancelled")).0, Some(State::Failed));
        assert_eq!(s.get(&id).unwrap().error_code, "coins_changed");
    }

    #[test]
    fn a_fee_bump_is_not_withdrawn_for_spending_the_send_it_replaces() {
        let mut s = Sends::default();
        let id = s.begin(Kind::FeeBump, "bitcoin_wallet_ui", "regtest", "g_1", "seed_1", json!({"txid": "ab".repeat(32)}), 1).unwrap();
        s.set_build_job(&id, "b1");
        assert_eq!(s.built(&id, preview("p1")), Next::Sign("p1".into()));
        s.set_sign_job(&id, "b2");
        s.observe(&id, "awaiting_approval", None, Instant::now());
        assert!(s.watched_inputs().is_empty(), "its inputs are spent by the unconfirmed original, by design");
    }

    #[test]
    fn sends_survive_a_restart_and_hold_no_secret() {
        let mut s = Sends::default();
        let done = started(&mut s);
        s.signed(&done, &json!({"txid": "t1", "broadcast": "unknown", "via": "electrum-1"}));
        let open = s.begin(Kind::Send, "some_dapp", "regtest", "g_2", "seed_1", json!({"recipients": []}), 5).unwrap();
        s.set_build_job(&open, "b9");
        s.built(&open, preview("p2"));
        s.set_sign_job(&open, "b10");
        s.observe(&open, "awaiting_approval", Some("smh_2"), Instant::now());
        assert!(s.take_dirty() && !s.take_dirty(), "dirty until taken");
        let text = s.to_json().to_string().to_lowercase();
        for word in ["receipt", "password", "phrase", "databasekey", "xprv", "tprv", "\"b10\"", "\"b9\""] {
            assert!(!text.contains(word), "a send's document holds {word}");
        }
        let r = reloaded(&s);
        assert_eq!((r.get(&done).unwrap().state, r.get(&done).unwrap().attached), (State::Unknown, true));
        let o = r.get(&open).unwrap();
        assert_eq!((o.state, o.attached, o.sign_started, o.approval_handle.as_str()), (State::AwaitingApproval, false, true, "smh_2"));
        assert!(o.sign_job.is_none() && o.awaiting_since.is_some());
        assert_eq!(r.orphans(), vec![open.clone()]);
        let mut r = r;
        let next = r.begin(Kind::Send, "x", "regtest", "g_3", "seed_1", json!({}), 9).unwrap();
        assert_eq!(next, "s3", "ids go on after a restart");
    }

    /// The sends kept at `p` as a restarted backend reads them, with these seeds' keys given.
    fn reopened(p: &std::path::Path, keys: &[(&str, u8)]) -> Sends {
        let mut s = Sends::new(Some(p.to_path_buf()));
        for (seed, byte) in keys {
            s.unlock(seed, &key(*byte, 0), &key(*byte, 0)).unwrap();
        }
        s
    }

    /// A send of `seed`'s wallet being signed.
    fn paying(s: &mut Sends, seed: &str, group: &str) -> String {
        let request = json!({"recipients": [{"address": "bcrt1qpay", "amountSat": "60000"}]});
        let id = s.begin(Kind::Send, "some_dapp", "regtest", group, seed, request, 1).unwrap();
        s.set_build_job(&id, "b1");
        s.built(&id, preview("p1"));
        s.set_sign_job(&id, "b2");
        id
    }

    /// The box a seed's sends are sealed in, as the file holds it.
    fn sealed(p: &std::path::Path, seed: &str) -> Value {
        serde_json::from_str::<Value>(&std::fs::read_to_string(p).unwrap()).unwrap()["seeds"][seed].clone()
    }

    fn ids(s: &Sends) -> Vec<Value> {
        s.list_json().as_array().unwrap().iter().map(|r| r["requestId"].clone()).collect()
    }

    use crate::test_key as key;

    #[test]
    fn a_locked_seeds_sends_are_neither_listed_nor_written() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sends.sealed.json");
        let mut s = reopened(&p, &[("seed_1", 1), ("seed_2", 2)]);
        let a = paying(&mut s, "seed_1", "g_1");
        let b = paying(&mut s, "seed_2", "g_2");
        s.signed(&a, &json!({"txid": "ab".repeat(32), "broadcast": "seen"}));
        s.signed(&b, &json!({"txid": "cd".repeat(32), "broadcast": "seen"}));
        s.save().unwrap();
        // A restart with only seed_1's key: seed_2's sends aren't read, and none starts for it.
        let mut r = reopened(&p, &[("seed_1", 1)]);
        assert_eq!(ids(&r), vec![json!(a)]);
        assert_eq!(r.status_json(&b)["ok"], false);
        assert_eq!(r.begin(Kind::Send, "x", "regtest", "g_2", "seed_2", json!({}), 2).unwrap_err(), LOCKED);
        assert_eq!(r.closed(), vec![("seed_2".to_string(), Some(0))]);
        // Locked while a send of seed_2 is open: it keeps working in memory, unwritten.
        let c = s.begin(Kind::Send, "some_dapp", "regtest", "g_2", "seed_2", json!({}), 3).unwrap();
        s.set_build_job(&c, "b3");
        s.save().unwrap();
        let before = sealed(&p, "seed_2");
        s.lock();
        s.unlock("seed_1", &key(1, 0), &key(1, 0)).unwrap();
        assert_eq!(s.readable_seeds(), vec!["seed_1".to_string()]);
        assert_eq!(s.locked_waiting(), vec![c.clone()], "withdrawn as a close would");
        assert_eq!(s.drop_locked(&HashSet::new()), 1, "the settled one goes at once");
        assert_eq!(s.build_failed(&c, "the wallet closed", Some("no_wallet")), Some(State::Failed));
        s.mark_dirty();
        s.save().unwrap();
        assert_eq!(sealed(&p, "seed_2"), before, "seed_2's sends are not written while it is locked");
        let busy: HashSet<String> = [c.clone()].into();
        assert_eq!(s.drop_locked(&busy), 0, "kept while a job runs for it");
        assert_eq!(ids(&s), vec![json!(c), json!(a)]);
        assert_eq!(s.drop_locked(&HashSet::new()), 1);
        assert_eq!(ids(&s), vec![json!(a)]);
        // Its key again: the copy stored before the lock comes back, to be reconciled.
        s.unlock("seed_2", &key(2, 0), &key(2, 0)).unwrap();
        assert_eq!((s.get(&c).unwrap().state, s.get(&b).unwrap().state), (State::Preparing, State::Sent));
        assert_eq!(s.orphans(), vec![c]);
    }

    #[test]
    fn a_send_before_its_seeds_key_waits_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sends.sealed.json");
        let mut s = Sends::new(Some(p.clone()));
        let a = paying(&mut s, "seed_3", "g_3");
        s.save().unwrap();
        assert!(!p.exists(), "nothing written before the key");
        assert_eq!((ids(&s), s.closed()), (vec![json!(a)], vec![("seed_3".to_string(), None)]));
        s.unlock("seed_3", &key(3, 0), &key(3, 0)).unwrap();
        s.save().unwrap();
        assert_eq!(reopened(&p, &[("seed_3", 3)]).get(&a).unwrap().seed, "seed_3");
    }

    #[test]
    fn a_reopened_store_reads_only_with_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sends.sealed.json");
        let mut s = reopened(&p, &[("seed_1", 1)]);
        let a = paying(&mut s, "seed_1", "g_1");
        s.save().unwrap();
        let mut r = Sends::new(Some(p.clone()));
        assert!(ids(&r).is_empty() && r.get(&a).is_none(), "sealed until the key comes");
        assert!(r.unlock("seed_1", &key(2, 0), &key(2, 0)).is_err(), "another key");
        assert!(r.get(&a).is_none());
        r.unlock("seed_1", &key(1, 0), &key(1, 0)).unwrap();
        let o = r.get(&a).unwrap();
        assert_eq!((o.state, o.attached, o.seed.as_str()), (State::Preparing, false, "seed_1"));
        assert_eq!(r.orphans(), vec![a]);
    }

    #[test]
    fn the_sealed_file_names_no_wallet_address_amount_or_txid() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sends.sealed.json");
        let mut s = reopened(&p, &[("seed_1", 1)]);
        let a = paying(&mut s, "seed_1", "g_1");
        s.observe(&a, "awaiting_approval", Some("smh_1"), Instant::now());
        s.signed(&a, &json!({"txid": "ab".repeat(32), "broadcast": "seen", "via": "electrum-0"}));
        s.save().unwrap();
        let text = crate::clear_text_of(&p);
        for word in ["g_1", "bcrt1q", "60000", "39859", "abab", "cdcd", "smh_1", "electrum", "some_dapp", "regtest", "txid", "receipt", "password"] {
            assert!(!text.contains(word), "sends.sealed.json holds {word}: {text}");
        }
        assert!(text.contains("seed_1"), "only the seed names its box");
    }

    #[test]
    fn a_generation_change_reseals_the_sends() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sends.sealed.json");
        let mut s = reopened(&p, &[("seed_1", 1)]);
        let a = paying(&mut s, "seed_1", "g_1");
        s.save().unwrap();
        let mut r = Sends::new(Some(p.clone()));
        assert_eq!(r.closed(), vec![("seed_1".to_string(), Some(0))]);
        r.unlock("seed_1", &key(1, 0), &key(7, 3)).unwrap();
        assert!(r.get(&a).is_some());
        let mut r = Sends::new(Some(p));
        assert_eq!(r.closed(), vec![("seed_1".to_string(), Some(3))], "sealed under the current generation");
        assert!(r.unlock("seed_1", &key(1, 0), &key(1, 0)).is_err(), "the old key no longer opens it");
        r.unlock("seed_1", &key(7, 3), &key(7, 3)).unwrap();
        assert!(r.get(&a).is_some());
    }

    #[test]
    fn ids_start_above_the_floor() {
        let mut s = Sends::default();
        s.start_ids_above(5_000);
        assert_eq!(s.begin(Kind::Send, "x", "regtest", "g_1", "seed_1", json!({}), 1).unwrap(), "s5001");
        s.start_ids_above(10);
        assert_eq!(s.begin(Kind::Send, "x", "regtest", "g_2", "seed_1", json!({}), 1).unwrap(), "s5002", "never back");
    }

    #[test]
    fn a_loaded_send_follows_the_core_preview() {
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", Some("smh_1"), Instant::now());
        let now = Instant::now();
        // Still awaiting approval in the core: followed, nothing re-asked.
        let mut r = reloaded(&s);
        assert_eq!(r.reconcile(&id, CoreView::Preview { state: "awaiting_approval:rendered", handle: Some("smh_1") }, now), (None, Next::Nothing));
        assert_eq!(r.reconcile(&id, CoreView::Preview { state: "broadcasting", handle: None }, now), (Some(State::Committing), Next::Nothing));
        // Gone from the core, and the wallet's history vouches for it: sent.
        let g = |h| CoreView::Gone { wallet_open: true, history: Some(h), readable: true };
        assert_eq!(r.reconcile(&id, g(Seen::Vouched), now).0, Some(State::Sent));
        // Built but nothing signs it: sign it again, once.
        let mut s2 = Sends::default();
        let id2 = s2.begin(Kind::Send, "x", "regtest", "g_1", "seed_1", json!({}), 1).unwrap();
        s2.built(&id2, preview("p9"));
        let mut r = reloaded(&s2);
        assert_eq!(r.reconcile(&id2, CoreView::Preview { state: "built", handle: None }, now), (None, Next::Sign("p9".into())));
        assert_eq!(r.reconcile(&id2, CoreView::Preview { state: "built", handle: None }, now), (None, Next::Nothing));
        r.set_sign_job(&id2, "b1");
        assert!(r.orphans().is_empty(), "attached again");
        // The sign job failed while the backend was down: failed, and the inputs released.
        let mut r = reloaded(&s);
        assert_eq!(r.reconcile(&id, CoreView::Preview { state: "failed: rejected: the person declined", handle: None }, now), (Some(State::Failed), Next::CancelPreview("p1".into())));
        assert_eq!(r.get(&id).unwrap().error_code, "rejected");
    }

    #[test]
    fn a_loaded_send_the_core_no_longer_has() {
        let now = Instant::now();
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", Some("smh_1"), now);
        let g = |open, h, readable| CoreView::Gone { wallet_open: open, history: h, readable };
        // The core's record says it went out, or the chain has it: sent at once.
        for seen in [Seen::Recorded(told("requested", "kyoto")), Seen::Recorded(Outcome::default()), Seen::Confirmed(Outcome::default()), Seen::Vouched] {
            let mut r = reloaded(&s);
            assert_eq!(r.reconcile(&id, g(true, Some(seen.clone()), true), now).0, Some(State::Sent), "{seen:?}");
            assert!(r.get(&id).unwrap().reconciled);
        }
        // A core without send records, and no source reports it: wait for one, then unknown.
        let mut r = reloaded(&s);
        assert_eq!(r.reconcile(&id, g(true, Some(Seen::Unreported), true), now).0, None);
        assert_eq!(r.reconcile(&id, g(true, Some(Seen::Unreported), true), now + REPORT_GRACE).0, Some(State::Unknown));
        // Not in history, never seen broadcasting: nothing went out.
        let mut r = reloaded(&s);
        assert_eq!(r.reconcile(&id, g(true, Some(Seen::Absent), true), now).0, Some(State::Failed));
        assert_eq!(r.get(&id).unwrap().error_code, "backend_restarted");
        // A busy wallet: ask again later.
        let mut r = reloaded(&s);
        assert_eq!(r.reconcile(&id, g(true, None, false), now).0, None);
        // The wallet is closed and the send was being signed: it may have left.
        let mut r = reloaded(&s);
        assert_eq!(r.reconcile(&id, g(false, None, true), now).0, Some(State::Unknown));
        // Seen broadcasting before the restart: unknown even if history lacks it.
        let mut s3 = Sends::default();
        let id3 = started(&mut s3);
        s3.observe(&id3, "broadcasting", None, now);
        let mut r = reloaded(&s3);
        assert_eq!(r.reconcile(&id3, g(true, Some(Seen::Absent), true), now).0, Some(State::Unknown));
        // Lost mid-build.
        let mut s4 = Sends::default();
        let id4 = s4.begin(Kind::Send, "x", "regtest", "g_1", "seed_1", json!({}), 1).unwrap();
        let mut r = reloaded(&s4);
        assert_eq!(r.reconcile(&id4, g(true, Some(Seen::Absent), true), now).0, Some(State::Failed));
        // A cancel the restart interrupted is asked again, once.
        let mut s5 = Sends::default();
        let id5 = started(&mut s5);
        s5.observe(&id5, "awaiting_approval", Some("smh_5"), now);
        s5.withdraw(&id5, Withdrawal::Caller("x".into())).unwrap();
        let mut r = reloaded(&s5);
        assert_eq!(r.reconcile(&id5, CoreView::Preview { state: "awaiting_approval:rendered", handle: None }, now).1, Next::CancelPreview("p1".into()));
        assert_eq!(r.cancel_failed(&id5, "unknown or settled preview"), None, "it may have been signed: reconciling decides");
        assert_eq!(r.reconcile(&id5, g(true, Some(Seen::Absent), true), now).0, Some(State::Cancelled));
    }

    #[test]
    fn history_rows_are_read_core_record_first() {
        let seen = |v: Value| seen_in(&v);
        let none = Outcome::default();
        assert_eq!(
            seen(json!({"state": "confirmed", "confirmations": 3, "sendState": "sent", "broadcast": "requested", "via": "kyoto"})),
            Seen::Confirmed(told("requested", "kyoto"))
        );
        assert_eq!(seen(json!({"state": "sent", "confirmations": 0, "sendState": "confirmed"})), Seen::Confirmed(none.clone()));
        assert_eq!(
            seen(json!({"state": "confirmed", "confirmations": 1, "sendState": "abandoned", "broadcast": "unknown", "via": "kyoto"})),
            Seen::Confirmed(told("unknown", "kyoto")),
            "it confirmed anyway"
        );
        assert_eq!(
            seen(json!({"state": "sent", "confirmations": 0, "reportedBy": null, "sendState": "sent", "broadcast": "requested", "via": "kyoto"})),
            Seen::Recorded(told("requested", "kyoto"))
        );
        assert_eq!(
            seen(json!({"state": "sent", "confirmations": 0, "reportedBy": ["electrum-0"], "sendState": "sent", "broadcast": "unknown", "via": "electrum-1"})),
            Seen::Recorded(told("unknown", "electrum-1")),
            "the core's word over a source's"
        );
        assert_eq!(seen(json!({"state": "waiting", "confirmations": 1, "sendState": "sent"})), Seen::Vouched, "above the authority's tip: a server's word");
        assert_eq!(seen(json!({"state": "sent", "confirmations": 0, "reportedBy": ["electrum-0"], "sendState": "sent"})), Seen::Vouched);
        assert_eq!(seen(json!({"state": "sent", "confirmations": 0, "reportedBy": null, "sendState": "sent"})), Seen::Recorded(none.clone()), "a core that names no outcome");
        assert_eq!(seen(json!({"state": "replaced", "confirmations": 0, "sendState": "replaced"})), Seen::Recorded(none));
        assert_eq!(seen(json!({"state": "sent", "confirmations": 0, "sendState": "broadcasting", "broadcast": null})), Seen::Unreported, "the core can't tell either");
        assert_eq!(seen(json!({"state": "sent", "confirmations": 0})), Seen::Unreported, "a core without send records");
    }

    #[test]
    fn a_send_approved_after_a_restart_is_sent_on_the_core_record() {
        // Kyoto as the authority: it broadcasts `requested` and no source ever reports the transaction.
        let now = Instant::now();
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", Some("smh_1"), now);
        let mut r = reloaded(&s);
        assert_eq!(r.reconcile(&id, CoreView::Preview { state: "awaiting_approval:rendered", handle: Some("smh_1") }, now), (None, Next::Nothing));
        assert_eq!(r.reconcile(&id, CoreView::Preview { state: "broadcasting", handle: None }, now).0, Some(State::Committing));
        let row = json!({"txid": "ab".repeat(32), "state": "sent", "confirmations": 0, "reportedBy": null, "sendState": "sent",
                         "broadcast": "requested", "via": "kyoto"});
        let gone = CoreView::Gone { wallet_open: true, history: Some(seen_in(&row)), readable: true };
        assert_eq!(r.reconcile(&id, gone, now).0, Some(State::Sent), "no wait for a source");
        let st = r.status_json(&id);
        assert_eq!((st["state"].clone(), st["error"].clone(), st["errorCode"].clone()), (json!("sent"), Value::Null, Value::Null));
        assert_eq!((st["broadcast"].clone(), st["via"].clone()), (json!("requested"), json!("kyoto")));
        assert!(r.unsettled("regtest", "g_1").is_empty());
    }

    #[test]
    fn a_restart_keeps_an_unknown_broadcast_unknown() {
        let now = Instant::now();
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", Some("smh_1"), now);
        let mut r = reloaded(&s);
        let row = json!({"state": "sent", "confirmations": 0, "reportedBy": ["electrum-0"], "sendState": "sent", "broadcast": "unknown", "via": "electrum-1"});
        let gone = CoreView::Gone { wallet_open: true, history: Some(seen_in(&row)), readable: true };
        assert_eq!(r.reconcile(&id, gone, now).0, Some(State::Unknown), "as the core's answer would have been");
        let st = r.status_json(&id);
        assert_eq!((st["broadcast"].clone(), st["via"].clone()), (json!("unknown"), json!("electrum-1")));
        assert_eq!(st["offer"], json!(["rebroadcast", "request_fee_bump", "abandon_send"]));
        // Neither its record nor a source settles it later; a block does.
        assert_eq!(r.upgrade(&id, &seen_in(&row)), None);
        assert_eq!(r.upgrade(&id, &Seen::Vouched), None);
        let mined = json!({"state": "confirmed", "confirmations": 1, "sendState": "confirmed", "broadcast": "unknown", "via": "electrum-1"});
        assert_eq!(r.upgrade(&id, &seen_in(&mined)), Some(State::Sent));
        assert_eq!(r.status_json(&id)["broadcast"], "seen");
    }

    #[test]
    fn an_unknown_or_reconciled_send_is_sent_once_it_confirms() {
        let ab = "ab".repeat(32);
        let mut s = Sends::default();
        let id = started(&mut s);
        s.signed(&id, &json!({"txid": ab, "broadcast": "unknown", "via": "kyoto"}));
        assert_eq!(s.unsettled("regtest", "g_1"), vec![(id.clone(), ab.clone())]);
        assert!(s.unsettled("regtest", "g_2").is_empty(), "another wallet's history says nothing of it");
        // The core said unknown: neither its record nor a source settles that, only the chain.
        for seen in [Seen::Absent, Seen::Unreported, Seen::Recorded(told("unknown", "kyoto")), Seen::Recorded(Outcome::default()), Seen::Vouched] {
            assert_eq!(s.upgrade(&id, &seen), None, "{seen:?}");
        }
        assert_eq!(s.upgrade(&id, &Seen::Confirmed(told("unknown", "kyoto"))), Some(State::Sent));
        let st = s.status_json(&id);
        assert_eq!((st["state"].clone(), st["error"].clone(), st["broadcast"].clone()), (json!("sent"), Value::Null, json!("seen")));
        assert!(st.get("offer").is_none() && s.unsettled("regtest", "g_1").is_empty());
        // Abandoned, then confirmed anyway: it went out.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.signed(&id, &json!({"txid": ab, "broadcast": "unknown"}));
        s.abandoned(&id);
        assert_eq!(s.upgrade(&id, &Seen::Recorded(told("requested", "kyoto"))), None, "given up: only a block says otherwise");
        assert_eq!(s.upgrade(&id, &Seen::Confirmed(Outcome::default())), Some(State::Sent));
        // The core stopped answering mid-commit: its record says how it went.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "broadcasting", None, Instant::now());
        assert_eq!(s.core_lost(&id), Some(State::Unknown));
        assert_eq!(s.upgrade(&id, &Seen::Recorded(told("requested", "kyoto"))), Some(State::Sent));
        // Settled unknown by a restart while its wallet was closed: the core's record decides once it is open.
        let now = Instant::now();
        let mut s = Sends::default();
        let id = started(&mut s);
        s.observe(&id, "awaiting_approval", Some("smh_1"), now);
        let mut r = reloaded(&s);
        assert_eq!(r.reconcile(&id, CoreView::Gone { wallet_open: false, history: None, readable: true }, now).0, Some(State::Unknown));
        let mut r = reloaded(&r);
        assert_eq!(r.unsettled("regtest", "g_1").len(), 1, "it survives another restart");
        assert_eq!(r.upgrade(&id, &Seen::Unreported), None);
        assert_eq!(r.upgrade(&id, &Seen::Recorded(told("unknown", "kyoto"))), None, "the core says unknown");
        assert_eq!(r.upgrade(&id, &Seen::Recorded(told("requested", "kyoto"))), Some(State::Sent));
        assert_eq!(r.status_json(&id)["via"], "kyoto");
        // Failed by a restart, then confirmed: it went out after all.
        let mut r = reloaded(&s);
        assert_eq!(r.reconcile(&id, CoreView::Gone { wallet_open: true, history: Some(Seen::Absent), readable: true }, now).0, Some(State::Failed));
        assert_eq!(r.upgrade(&id, &Seen::Confirmed(Outcome::default())), Some(State::Sent));
        // A failure the core reported is not followed, nor a send whose txid a newer send took.
        let mut s = Sends::default();
        let id = started(&mut s);
        s.sign_failed(&id, "rejected: the person declined", Some("rejected"));
        assert!(s.unsettled("regtest", "g_1").is_empty());
        let mut s = Sends::default();
        let id = started(&mut s);
        s.signed(&id, &json!({"txid": ab, "broadcast": "unknown"}));
        let again = s.begin(Kind::Send, "x", "regtest", "g_1", "seed_1", json!({}), 2).unwrap();
        s.built(&again, preview("p2"));
        assert!(s.unsettled("regtest", "g_1").is_empty(), "the newer send follows that transaction");
    }

    #[test]
    fn a_preview_spending_beyond_the_gap_says_so() {
        let mut p = preview("p1");
        p["warnings"] = json!([{"kind": "beyond_gap_inputs", "count": 1, "sat": "100000", "message": "spends coins past the gap limit"}]);
        let w = &summary(&p)["warnings"];
        assert_eq!((w[0]["kind"].clone(), w[0]["sat"].clone()), (json!("beyond_gap_inputs"), json!("100000")), "as the core gives it");
    }

    #[test]
    fn listing_is_newest_first_and_bounded() {
        let mut s = Sends::default();
        for i in 0..(KEEP_FINAL + 5) {
            let id = s.begin(Kind::Send, "x", "regtest", "g_1", "seed_1", json!({}), i as u64).unwrap();
            s.not_started(&id, "no", None);
        }
        let list = s.list_json();
        assert!(list.as_array().unwrap().len() <= KEEP_FINAL);
        assert_eq!(list[0]["requestId"], format!("s{}", KEEP_FINAL + 5));
        assert_eq!(list[0]["errorCode"], "failed");
        assert_eq!(s.status_json("s1")["ok"], false, "the oldest final sends are dropped");
    }
}
