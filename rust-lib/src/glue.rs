//! Logos glue for `bitcoin_wallet_backend` (rust-first, `concurrency: "multi"`). A reactor
//! follows the core's jobs and sends, and tells the node module which network has a wallet open.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use logos_rust_sdk::{AboutToUnload, LogosCaller, LogosError, Shutdown};
use serde_json::{json, Value};

use crate::gate::{self, Caller, Roles};
use crate::jobs::{Jobs, Purpose};
use crate::registry::Registry;
use crate::request;
use crate::sends::{self, CoreView, Kind, Next, Seen, Sends, Withdrawal};
use crate::switch::{self, Switch};
use crate::view;
use crate::{amount, now_secs, write_atomic};

pub trait BitcoinWalletBackendModule: Send + Sync + 'static {
    /// CUSTODIAN. Replaces the custodian set, `{ custodians }`: total (`{}` is nobody), unknown
    /// keys refused, saved to roles.json. `{ ok, custodians }`.
    fn configure(&self, roles_json: String) -> String;
    /// `{ ok, kind, identity, custodians, rolesFile }`: who this call came from, who holds the
    /// role, and the file a headless operator writes to enrol a module.
    fn caller_identity(&self) -> String;

    /// `{ ok, networks, active, pending }`; `pending` is a switch waiting for a close to settle.
    fn list_networks(&self) -> String;
    /// CUSTODIAN. While a close settles the switch is queued: `{ ok, active, pending }`, and opens are
    /// refused with code busy until it applies. Refusals carry `code`: busy, wallet_open, send_open.
    fn set_active_network(&self, network: String) -> String;
    /// `{ ok, active, wallets: [{ group, family, network, label, birthday, firstOpened, lastOpened }] }`:
    /// wallets opened before, newest first. No secrets.
    fn list_wallets(&self) -> String;
    /// CUSTODIAN. `{ network?, group?, family?, birthday?, label? }` on the active network. `{ ok,
    /// jobId }`; wallet_status carries `openHandle` while the person decides in the keys app.
    fn open_wallet(&self, params_json: String) -> String;
    /// CUSTODIAN. Withdraws open sends first; refused while one is going out. `{ ok, jobId }`.
    fn close_wallet(&self) -> String;
    /// `{ ok, jobId, kind, state: queued|running|done|failed|cancelled, result?, error? }`.
    fn job_status(&self, job_id: String) -> String;
    /// CUSTODIAN. The open wallet's public descriptors, as a call result only, never an event.
    fn descriptors(&self) -> String;
    /// CUSTODIAN. `{ date: "YYYY-MM-DD" }` or `{ height }` for the open wallet; the core scans again from
    /// it. `{ ok, jobId }`; the job's result is `{ birthday: { height, source }, rescanFrom }`.
    fn set_birthday(&self, params_json: String) -> String;

    /// The core's status plus `syncPercent`, `activeNetwork`, `label` and `openSend`.
    fn wallet_status(&self) -> String;
    /// `{ ok, busy, confirmed, waiting, pending, sent, immature, tooSmall }` in satoshis, each with
    /// a `…Btc` twin. Empty while busy, never zero.
    fn balances(&self) -> String;
    /// The core's history page, newest first; rows gain `amountBtc` and `feeBtc`.
    fn history(&self, page: i64) -> String;
    /// `{ ok, address, index }`: a fresh receive address.
    fn receive_address(&self) -> String;
    /// `{ ok, fast, normal, slow, minimum, sources }` in sat/vB.
    fn fee_rates(&self) -> String;
    /// An address or BIP21 URI on the wallet's network: `{ ok, valid, address, type, amountSat?, reason? }`.
    fn address_valid(&self, text: String) -> String;
    /// Satoshis, a decimal string, as BTC: `{ ok, sat, btc }`.
    fn format_btc(&self, sat: String) -> String;
    /// BTC as satoshis, exactly; more than 8 decimals is refused. `{ ok, btc, sat }`.
    fn parse_btc(&self, btc: String) -> String;

    /// The node module's sources for the wallet's network.
    fn sources(&self) -> String;
    /// The node module's source health, plus `wallet`, the open wallet's own view of them.
    fn source_health(&self) -> String;
    /// `{ network?, source }`: the node module probes that source now, or every enabled server for "",
    /// within 15 s; a TLS server's row carries `certificate`.
    fn probe(&self, params_json: String) -> String;
    /// The servers that have seen the open wallet's addresses, and since when.
    fn exposure(&self) -> String;
    /// CUSTODIAN. Replaces the network's sources, a JSON array. Refused while a wallet is open.
    fn set_sources(&self, list_json: String) -> String;
    /// CUSTODIAN. `{ use?, history?, tor? }` for one source. Refused while a wallet is open.
    fn set_source(&self, source_id: String, changes_json: String) -> String;
    /// CUSTODIAN. The Tor proxy for every source, socks5h://host:port. Refused while a wallet is open.
    fn set_proxy(&self, proxy: String) -> String;
    /// CUSTODIAN. Clears a suspect mark in the node module and, with a wallet open, in the core.
    fn clear_suspect(&self, source_id: String) -> String;
    /// CUSTODIAN. `{ network, peers: [host[:port]] }`, at most 64: Kyoto's own peers, which replace the
    /// curated list (`[]` restores it). Refused while a wallet is open. Answers as peer_list.
    fn set_own_peers(&self, params_json: String) -> String;
    /// CUSTODIAN. `{ network? }`: the node module's peer list, with `ownPeers` and `usable`.
    fn peer_list(&self, params_json: String) -> String;

    /// Any named module. `{ recipients: [{ address, amountSat | amountBtc }], feeRate? | target?,
    /// sendMax? }` → `{ ok, requestId }`. One open send per wallet.
    fn request_send(&self, request_json: String) -> String;
    /// Any named module. `{ txid, feeRate }` replaces an unconfirmed send with RBF, approved like
    /// any send. `{ ok, requestId }`.
    fn request_fee_bump(&self, request_json: String) -> String;
    /// `{ ok, requestId, kind, state, requester, summary, approvalHandle, txid, broadcast, via, error, errorCode,
    /// cancelling }`; states preparing, awaiting_approval, committing, sent, failed, cancelled, unknown.
    fn send_status(&self, request_id: String) -> String;
    /// `{ ok, sends: [{ requestId, kind, state, requester, approvalHandle, txid, errorCode, createdAt, updatedAt }] }`, newest first.
    fn list_sends(&self) -> String;
    /// The requester or a custodian. Withdraws the approval request and unlocks the inputs;
    /// refused once the transaction is going out. `{ ok, requestId, state, cancelling }`.
    fn cancel_send(&self, request_id: String) -> String;
    /// CUSTODIAN. Broadcasts a send's transaction again, named by request id or txid. `{ ok, jobId }`.
    fn rebroadcast(&self, request_or_txid: String) -> String;
    /// CUSTODIAN. `{ txid }` of a send in `unknown`: the core frees its coins. `{ ok, jobId, requestId,
    /// txid, note }`; if the original still confirms, a later send from them fails, or replaces it first.
    fn abandon_send(&self, params_json: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

#[allow(dead_code)] // Read by logos-lidl-gen, which generates the emitters.
pub trait BitcoinWalletBackendModuleEvents {
    /// wallet_status()'s shape, whenever the core's state changes.
    fn wallet_state_changed(&self, payload: String);
    /// `{ state, network, walletTip, authorityHeight, tipAgeSecs, percent, synced }`, on change.
    fn sync_progress(&self, payload: String);
    /// balances()'s shape, when an amount the core read changes.
    fn balance_changed(&self, payload: String);
    fn send_status_changed(&self, request_id: String, state: String);
    /// Opens, closes and rebroadcasts, under the backend's job ids.
    fn job_finished(&self, job_id: String, state: String);
    /// The core's alert about a source: `{ source, kind, detail, at, network, group }`.
    fn source_alert(&self, payload: String);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

use bitcoin_node_module::BitcoinNodeModuleClient as NodeClient;
use bitcoin_wallet_core_module::BitcoinWalletCoreModuleClient as CoreClient;

/// Reads answer well inside the host's 20 s deadline; so do the writes that follow them.
const READ: Duration = Duration::from_secs(6);
const WRITE: Duration = Duration::from_secs(8);
/// The node module answers a probe within 15 s.
const PROBE: Duration = Duration::from_secs(17);
const TICK: Duration = Duration::from_millis(500);

static UNLOADING: AtomicBool = AtomicBool::new(false);
/// Calls that got no answer; the reactor backs off while they climb.
static UNANSWERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Waits for an async reply on this thread. A blocking call would wait in a nested event loop
/// on the main thread, where concurrent calls return innermost first. Unload answers at once.
fn wait<T>(rx: Receiver<Result<T, LogosError>>, budget: Duration) -> Result<T, String> {
    let start = Instant::now();
    loop {
        if UNLOADING.load(Ordering::SeqCst) {
            return Err("unloading".into());
        }
        let r = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(RecvTimeoutError::Timeout) if start.elapsed() < budget + Duration::from_secs(1) => continue,
            Err(_) => Err("no answer".into()),
        };
        if r.is_err() && !UNLOADING.load(Ordering::SeqCst) {
            UNANSWERED.fetch_add(1, Ordering::SeqCst);
        }
        return r;
    }
}

macro_rules! ipc {
    ($client:expr, $method:ident, $budget:expr $(, $arg:expr)* $(,)?) => {{
        let (tx, rx) = sync_channel(1);
        $client.$method($($arg,)* $budget, move |r| {
            let _ = tx.send(r);
        });
        wait(rx, $budget)
    }};
}

/// One client per dependency, made on first use and kept.
fn core() -> &'static CoreClient {
    static C: OnceLock<CoreClient> = OnceLock::new();
    C.get_or_init(CoreClient::new)
}

fn node() -> &'static NodeClient {
    static N: OnceLock<NodeClient> = OnceLock::new();
    N.get_or_init(NodeClient::new)
}

fn refused() -> String {
    gate::NOT_AUTHORIZED.into()
}

/// A refusal with a code the app can act on.
fn coded(code: &str, e: impl std::fmt::Display) -> String {
    json!({"ok": false, "error": e.to_string(), "code": code}).to_string()
}

/// Applies a switch queued behind a close, once nothing is open or settling.
fn apply_pending_switch(inner: &Shared, status: &Value) {
    let applied = {
        let mut g = lock(inner);
        let Some(net) = g.pending_network.clone() else { return };
        if !switch::applies(view::wallet_open(status), g.jobs.lifecycle_pending().is_some()) {
            return;
        }
        g.active = net.clone();
        g.pending_network = None;
        if let Some(dir) = g.dir.clone() {
            if let Err(e) = write_atomic(&dir.join("settings.json"), json!({"activeNetwork": net}).to_string().as_bytes()) {
                eprintln!("bitcoin_wallet_backend: settings.json: {e}");
            }
        }
        view::status_view(status.clone(), &g.active, None, g.sends.any_open())
    };
    emit_wallet_state_changed(&applied.to_string());
}

fn err(e: impl std::fmt::Display) -> String {
    json!({"ok": false, "error": e.to_string()}).to_string()
}

fn ok(mut v: Value) -> String {
    v["ok"] = json!(true);
    v.to_string()
}

fn caller() -> Caller {
    match logos_rust_sdk::current_caller() {
        LogosCaller::Unknown => Caller::Unknown,
        LogosCaller::HostAnchor => Caller::HostAnchor,
        LogosCaller::Module { name, .. } => Caller::Module(name),
        LogosCaller::Derived { parent, leaf } => Caller::Derived { parent, leaf },
        LogosCaller::Operator { name } => Caller::Operator(name),
    }
}

fn parse(raw: String, who: &str) -> Result<Value, String> {
    serde_json::from_str(&raw).map_err(|e| format!("{who}: {e}"))
}

/// A reply passed through as it came, or why it didn't come.
fn relay(r: Result<String, String>, who: &str) -> String {
    r.unwrap_or_else(|e| err(format!("{who} did not answer: {e}")))
}

fn core_status() -> Result<Value, String> {
    let v = ipc!(core(), status_async_with_timeout, READ).and_then(|s| parse(s, "wallet core"))?;
    if v["ok"] != true {
        return Err(v["error"].as_str().unwrap_or("the wallet core refused").to_string());
    }
    Ok(v)
}

/// The reply's error, or the reply itself when it is `{ ok: true }`.
fn checked(v: Value, who: &str) -> Result<Value, String> {
    if v["ok"] == true {
        Ok(v)
    } else {
        Err(v["error"].as_str().map(String::from).unwrap_or_else(|| format!("{who} refused")))
    }
}

struct Inner {
    jobs: Jobs,
    sends: Sends,
    registry: Registry,
    active: String,
    dir: Option<PathBuf>,
    regtest: bool,
    /// The network the node module was last told has a wallet open.
    asserted: Option<String>,
    /// A switch asked for while a close settled; applied once it has.
    pending_network: Option<String>,
    node_ready: bool,
    last_sync: String,
    last_balance: Option<String>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            jobs: Jobs::default(),
            sends: Sends::default(),
            registry: Registry::new(None),
            active: "mainnet".into(),
            dir: None,
            regtest: false,
            asserted: None,
            pending_network: None,
            node_ready: false,
            last_sync: String::new(),
            last_balance: None,
        }
    }
}

type Shared = Arc<std::sync::Mutex<Inner>>;

#[derive(Default)]
pub struct BitcoinWalletBackendModuleImpl {
    inner: Shared,
    roles: std::sync::Mutex<Roles>,
    threads: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

fn lock(inner: &Shared) -> std::sync::MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts a core job and tracks it under a backend id.
fn start_core_job(inner: &Shared, kind: &str, params: &Value, purpose: Purpose) -> Result<String, String> {
    let p = params.to_string();
    let v = ipc!(core(), start_job_async_with_timeout, WRITE, kind, &p).and_then(|s| parse(s, "wallet core"))?;
    let v = checked(v, "the wallet core")?;
    let (Some(id), Some(receipt)) = (v["jobId"].as_str(), v["receipt"].as_str()) else {
        return Err("the wallet core returned no job id".into());
    };
    Ok(lock(inner).jobs.track(kind, id, receipt, purpose))
}

fn set_wallet_open(inner: &Shared, network: &str, open: bool) -> bool {
    let r = ipc!(node(), set_wallet_open_async_with_timeout, WRITE, network, open).and_then(|s| parse(s, "node module"));
    let done = r.as_ref().is_ok_and(|v| v["ok"] == true);
    if done {
        let mut g = lock(inner);
        if open {
            g.asserted = Some(network.into());
        } else if g.asserted.as_deref() == Some(network) {
            g.asserted = None;
        }
    }
    done
}

fn emit_changed(id: &str, st: Option<sends::State>) {
    if let Some(s) = st {
        emit_send_status_changed(id, s.name());
    }
}

/// Withdraws a send; it settles when the core confirms.
fn withdraw(inner: &Shared, id: &str, why: Withdrawal) -> Result<(), String> {
    let next = lock(inner).sends.withdraw(id, why)?;
    match next {
        Next::CancelPreview(pid) => cancel_preview(inner, id, &pid),
        Next::CancelBuild(jid) => {
            let ids = lock(inner).jobs.get(&jid).map(|j| (j.core_id.clone(), j.receipt.clone()));
            if let Some((cid, receipt)) = ids {
                let _ = ipc!(core(), cancel_job_async_with_timeout, WRITE, &cid, &receipt);
            }
        }
        Next::Sign(_) | Next::Nothing => {}
    }
    Ok(())
}

/// Asks the core to cancel a preview: it withdraws the approval and unlocks the inputs.
fn cancel_preview(inner: &Shared, id: &str, preview_id: &str) {
    let purpose = Purpose::CancelSend { request: id.into() };
    if let Err(e) = start_core_job(inner, "cancel_send", &json!({"previewId": preview_id}), purpose) {
        let st = lock(inner).sends.cancel_failed(id, &e);
        emit_changed(id, st);
    }
}

/// Follows every running core job; reads a finished one once, then acknowledges it.
fn follow_jobs(inner: &Shared) {
    let pending = lock(inner).jobs.pending();
    for (jid, cid, receipt) in pending {
        let status = ipc!(core(), job_status_async_with_timeout, READ, &cid, &receipt).and_then(|s| parse(s, "wallet core"));
        let st = match status {
            Ok(v) if v["ok"] == true => v,
            Ok(v) if v["error"] == "unknown job" => {
                lock(inner).jobs.finish(&jid, "failed", Value::Null, "the wallet core lost this job".into(), None);
                job_lost(inner, &jid, false);
                continue;
            }
            _ => {
                if lock(inner).jobs.miss(&jid) {
                    job_lost(inner, &jid, false);
                }
                continue;
            }
        };
        let state = st["state"].as_str().unwrap_or_default().to_string();
        match state.as_str() {
            "done" => {
                let r = ipc!(core(), job_result_async_with_timeout, READ, &cid, &receipt).and_then(|s| parse(s, "wallet core"));
                match r {
                    Ok(v) if v["ok"] == true => {
                        let _ = ipc!(core(), ack_job_async_with_timeout, READ, &cid, &receipt);
                        finish(inner, &jid, "done", v["result"].clone(), String::new(), None);
                    }
                    // Done but unread: keep asking. For a broadcast, done means it went out.
                    _ => {
                        if lock(inner).jobs.miss(&jid) {
                            job_lost(inner, &jid, true);
                        }
                    }
                }
            }
            "failed" | "cancelled" => {
                let _ = ipc!(core(), ack_job_async_with_timeout, READ, &cid, &receipt);
                let e = st["error"].as_str().unwrap_or("the job failed").to_string();
                finish(inner, &jid, &state, Value::Null, e, st["code"].as_str().map(String::from));
            }
            s => lock(inner).jobs.answered(&jid, s),
        }
    }
}

/// A job the core no longer answers for. `done_unread`: it finished, its result unread.
fn job_lost(inner: &Shared, jid: &str, done_unread: bool) {
    let purpose = lock(inner).jobs.get(jid).map(|j| j.purpose.clone());
    match purpose {
        Some(Purpose::Sign { request }) => {
            let st = if done_unread {
                lock(inner).sends.signed(&request, &json!({"broadcast": "unknown"}))
            } else {
                lock(inner).sends.core_lost(&request)
            };
            emit_changed(&request, st);
        }
        Some(Purpose::Build { request }) | Some(Purpose::CancelSend { request }) => {
            let st = lock(inner).sends.core_lost(&request);
            emit_changed(&request, st);
        }
        Some(p) if p.public() => emit_job_finished(jid, "failed"),
        _ => {}
    }
}

/// Moves whatever a finished job belongs to. `code` is the core's stable failure code, if it sends one.
fn finish(inner: &Shared, jid: &str, state: &str, result: Value, error: String, code: Option<String>) {
    let purpose = {
        let mut g = lock(inner);
        g.jobs.finish(jid, state, result.clone(), error.clone(), code.as_deref());
        g.jobs.get(jid).map(|j| j.purpose.clone())
    };
    let Some(purpose) = purpose else { return };
    let done = state == "done";
    match purpose {
        Purpose::Open { network, label } => {
            if done {
                let mut g = lock(inner);
                let net = result["network"].as_str().unwrap_or(network.as_str()).to_string();
                let (group, family) = (result["group"].as_str().unwrap_or_default(), result["family"].as_str().unwrap_or_default());
                if let Err(e) = g.registry.record_open(&net, group, family, label.as_deref(), result["birthday"].clone(), now_secs()) {
                    eprintln!("bitcoin_wallet_backend: {e}");
                }
            } else {
                set_wallet_open(inner, &network, false);
            }
            emit_job_finished(jid, state);
        }
        Purpose::Close { network } => {
            set_wallet_open(inner, &network, false);
            if let Ok(st) = core_status() {
                apply_pending_switch(inner, &st);
            }
            emit_job_finished(jid, state);
        }
        Purpose::Build { request } => {
            if !done {
                let st = lock(inner).sends.build_failed(&request, &error, code.as_deref());
                emit_changed(&request, st);
                return;
            }
            let next = lock(inner).sends.built(&request, result);
            match next {
                Next::Sign(pid) => match start_core_job(inner, "sign_and_broadcast", &json!({"previewId": pid}), Purpose::Sign { request: request.clone() }) {
                    Ok(j) => lock(inner).sends.set_sign_job(&request, &j),
                    Err(e) => {
                        let st = lock(inner).sends.not_started(&request, &e, None);
                        emit_changed(&request, st);
                        cancel_preview(inner, &request, &pid);
                    }
                },
                Next::CancelPreview(pid) => cancel_preview(inner, &request, &pid),
                _ => {
                    let st = lock(inner).sends.get(&request).map(|s| s.state);
                    emit_changed(&request, st);
                }
            }
        }
        Purpose::Sign { request } => {
            if done {
                let st = lock(inner).sends.signed(&request, &result);
                emit_changed(&request, st);
                return;
            }
            let (st, release) = lock(inner).sends.sign_failed(&request, &error, code.as_deref());
            emit_changed(&request, st);
            if release {
                let pid = lock(inner).sends.get(&request).map(|s| s.preview_id.clone()).unwrap_or_default();
                cancel_preview(inner, &request, &pid);
            }
        }
        Purpose::CancelSend { request } => {
            let st = if done { lock(inner).sends.cancel_done(&request) } else { lock(inner).sends.cancel_failed(&request, &error) };
            emit_changed(&request, st);
        }
        Purpose::Rebroadcast { request, .. } => {
            if let (true, Some(rid)) = (done, request.as_deref()) {
                let st = lock(inner).sends.rebroadcast(rid, &result);
                emit_changed(rid, st);
            }
            emit_job_finished(jid, state);
        }
        Purpose::Birthday { network, group } => {
            if done {
                if let Err(e) = lock(inner).registry.set_birthday(&network, &group, result["birthday"].clone()) {
                    eprintln!("bitcoin_wallet_backend: {e}");
                }
            }
            emit_job_finished(jid, state);
        }
        Purpose::Abandon { request, .. } => {
            if done {
                lock(inner).sends.abandoned(&request);
                // The state stays unknown; the event tells surfaces to read the send again.
                emit_send_status_changed(&request, "unknown");
            }
            emit_job_finished(jid, state);
        }
    }
}

/// Saves sends.json when a send changed; a failed write is tried again next tick.
fn save_sends(inner: &Shared) {
    let mut g = lock(inner);
    if !g.sends.take_dirty() {
        return;
    }
    let Some(dir) = g.dir.clone() else { return };
    if let Err(e) = write_atomic(&dir.join("sends.json"), g.sends.to_json().to_string().as_bytes()) {
        eprintln!("bitcoin_wallet_backend: sends.json: {e}");
        g.sends.mark_dirty();
    }
}

/// What the open wallet's history says of each txid, from the newest 200 rows; None while it is busy.
fn history_of(txids: &[&str]) -> Option<BTreeMap<String, Seen>> {
    let mut out: BTreeMap<String, Seen> = txids.iter().map(|t| (t.to_string(), Seen::Absent)).collect();
    let mut left = out.len();
    for page in 0..4i64 {
        let h = ipc!(core(), history_async_with_timeout, READ, page).and_then(|s| parse(s, "wallet core")).ok()?;
        if h["ok"] != true || h["busy"] == true {
            return None;
        }
        let rows = h["rows"].as_array().cloned().unwrap_or_default();
        for r in &rows {
            if let Some(seen) = r["txid"].as_str().and_then(|t| out.get_mut(t)).filter(|s| **s == Seen::Absent) {
                *seen = sends::seen_in(r);
                left -= 1;
            }
        }
        if left == 0 || rows.len() < 50 {
            break;
        }
    }
    Some(out)
}

/// Reconciles the sends loaded from sends.json, which no job of this instance follows, with the core.
fn follow_orphans(inner: &Shared) {
    let orphans = lock(inner).sends.orphans();
    let mut status: Option<Option<Value>> = None;
    for id in orphans {
        let Some((pid, net, group, txid)) = lock(inner).sends.get(&id).map(|s| (s.preview_id.clone(), s.network.clone(), s.group.clone(), s.txid.clone())) else {
            continue;
        };
        let preview = if pid.is_empty() { None } else { ipc!(core(), preview_async_with_timeout, READ, &pid).and_then(|s| parse(s, "wallet core")).ok() };
        if !pid.is_empty() && preview.is_none() {
            continue; // The core didn't answer; ask again next tick.
        }
        let now = Instant::now();
        let (st, next) = match preview.filter(|p| p["ok"] == true) {
            Some(p) => {
                let view = CoreView::Preview { state: p["state"].as_str().unwrap_or_default(), handle: p["approvalHandle"].as_str() };
                lock(inner).sends.reconcile(&id, view, now)
            }
            None => {
                let Some(st) = status.get_or_insert_with(|| core_status().ok()).clone() else { continue };
                let open = matches!(st["state"].as_str(), Some("syncing" | "ready")) && st["network"] == net.as_str() && st["group"] == group.as_str();
                let (history, readable) = match (open && !txid.is_empty()).then(|| history_of(&[txid.as_str()])) {
                    Some(Some(h)) => (h.get(&txid).copied(), true),
                    Some(None) => (None, false),
                    None => (Some(Seen::Absent), true),
                };
                let view = CoreView::Gone { wallet_open: open, history: if open { history } else { None }, readable };
                lock(inner).sends.reconcile(&id, view, now)
            }
        };
        emit_changed(&id, st);
        match next {
            Next::Sign(pid) => {
                // Refused when a sign job from before the restart still runs: the preview is followed instead.
                if let Ok(j) = start_core_job(inner, "sign_and_broadcast", &json!({"previewId": pid}), Purpose::Sign { request: id.clone() }) {
                    lock(inner).sends.set_sign_job(&id, &j);
                }
            }
            Next::CancelPreview(pid) => cancel_preview(inner, &id, &pid),
            _ => {}
        }
    }
}

/// Moves an unknown or reconciled send of the open wallet to sent once its history shows it went out.
fn follow_confirmations(inner: &Shared, status: &Value) {
    let (Some(net), Some(group)) = (status["network"].as_str(), status["group"].as_str()) else { return };
    let unsettled = lock(inner).sends.unsettled(net, group);
    if unsettled.is_empty() {
        return;
    }
    let txids: Vec<&str> = unsettled.iter().map(|(_, t)| t.as_str()).collect();
    let Some(seen) = history_of(&txids) else { return };
    for (id, txid) in &unsettled {
        let st = seen.get(txid).and_then(|s| lock(inner).sends.upgrade(id, *s));
        emit_changed(id, st);
    }
}

/// Follows the core's preview while a send is signed, and the keystore's 60 s for an offer.
fn follow_sends(inner: &Shared) {
    let watched: Vec<(String, String)> = {
        let g = lock(inner);
        g.sends
            .open_ids()
            .into_iter()
            .filter_map(|id| g.sends.get(&id).filter(|s| s.sign_job.is_some()).map(|s| (id.clone(), s.preview_id.clone())))
            .collect()
    };
    for (id, pid) in watched {
        let Ok(p) = ipc!(core(), preview_async_with_timeout, READ, &pid).and_then(|s| parse(s, "wallet core")) else { continue };
        if p["ok"] != true {
            continue;
        }
        let st = lock(inner).sends.observe(&id, p["state"].as_str().unwrap_or_default(), p["approvalHandle"].as_str(), Instant::now());
        emit_changed(&id, st);
    }
    let overdue = lock(inner).sends.overdue_acks(Instant::now());
    for id in overdue {
        let _ = withdraw(inner, &id, Withdrawal::NoAck);
    }
}

/// Fills the node module's absent settings once: what is already set is never changed.
fn ensure_node(inner: &Shared) {
    if lock(inner).node_ready {
        return;
    }
    let Ok(st) = ipc!(node(), config_status_async_with_timeout, READ).and_then(|s| parse(s, "node module")) else { return };
    let ready = match st["state"].as_str() {
        Some("configured") => true,
        Some("unconfigured") | Some("unready") => {
            ipc!(node(), init_defaults_async_with_timeout, WRITE).and_then(|s| parse(s, "node module")).is_ok_and(|v| v["ok"] == true)
        }
        _ => false,
    };
    lock(inner).node_ready = ready;
}

/// Tells the node module which network has a wallet open, and again after it restarts.
fn assert_wallet_open(inner: &Shared, status: &Value, check_node: bool) {
    let (desired, asserted) = {
        let g = lock(inner);
        (view::open_network(status).or_else(|| g.jobs.lifecycle_pending()), g.asserted.clone())
    };
    if desired != asserted {
        if let Some(prev) = asserted.as_deref().filter(|p| Some(*p) != desired.as_deref()) {
            set_wallet_open(inner, prev, false);
        }
        if let Some(n) = desired.as_deref() {
            set_wallet_open(inner, n, true);
        }
    }
    if !check_node {
        return;
    }
    let Ok(cfg) = ipc!(node(), config_status_async_with_timeout, READ).and_then(|s| parse(s, "node module")) else { return };
    let open: Vec<String> = cfg["walletOpen"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    if let Some(n) = desired.as_deref().filter(|n| !open.iter().any(|o| o == n)) {
        set_wallet_open(inner, n, true);
    }
    for stale in open.iter().filter(|o| Some(o.as_str()) != desired.as_deref()) {
        set_wallet_open(inner, stale, false);
    }
}

/// Withdraws a send whose inputs left the wallet's confirmed coins while it waited.
fn check_coins(inner: &Shared) {
    let watched = lock(inner).sends.watched_inputs();
    if watched.is_empty() {
        return;
    }
    let Ok(u) = ipc!(core(), utxos_async_with_timeout, READ).and_then(|s| parse(s, "wallet core")) else { return };
    let Some(rows) = u["utxos"].as_array().filter(|_| u["ok"] == true && u["busy"] != true) else { return };
    let coins: HashSet<String> = rows.iter().filter_map(|r| r["outpoint"].as_str().map(String::from)).collect();
    for (id, inputs) in watched {
        if !sends::gone_inputs(&inputs, &coins).is_empty() {
            let _ = withdraw(inner, &id, Withdrawal::CoinsChanged);
        }
    }
}

fn slow_tick(inner: &Shared, check_node: bool) {
    ensure_node(inner);
    let Ok(status) = core_status() else { return };
    assert_wallet_open(inner, &status, check_node);
    apply_pending_switch(inner, &status);
    let sync = view::sync_view(&status);
    let digest = json!([sync["state"], sync["network"], sync["walletTip"], sync["authorityHeight"], sync["percent"], sync["synced"]]).to_string();
    let changed = {
        let mut g = lock(inner);
        let c = g.last_sync != digest;
        g.last_sync = digest;
        c
    };
    if changed {
        emit_sync_progress(&sync.to_string());
    }
    if matches!(status["state"].as_str(), Some("syncing" | "ready")) {
        if let Ok(b) = ipc!(core(), balance_async_with_timeout, READ).and_then(|s| parse(s, "wallet core")) {
            if let Some(d) = view::balance_digest(&b) {
                let changed = {
                    let mut g = lock(inner);
                    let c = g.last_balance.as_deref() != Some(d.as_str());
                    g.last_balance = Some(d);
                    c
                };
                if changed {
                    emit_balance_changed(&view::balance_view(b).to_string());
                }
            }
        }
        check_coins(inner);
        follow_confirmations(inner, &status);
    }
}

fn nap(d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until && !UNLOADING.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn reactor(inner: Shared) {
    // Calls out wait until the host has finished wiring this module.
    nap(Duration::from_millis(500));
    let mut tick: u64 = 0;
    let mut pause = None;
    while !UNLOADING.load(Ordering::SeqCst) {
        let before = UNANSWERED.load(Ordering::SeqCst);
        follow_jobs(&inner);
        follow_sends(&inner);
        follow_orphans(&inner);
        // Every 3 s; the node module's own view every 9 s, to catch a restart.
        if tick % 6 == 0 {
            slow_tick(&inner, (tick / 6) % 3 == 0);
        }
        save_sends(&inner);
        tick += 1;
        // A module that stopped answering is asked again after 100 ms, doubling to 5 s.
        pause = switch::backoff(pause, UNANSWERED.load(Ordering::SeqCst) > before);
        nap(pause.unwrap_or(TICK).max(TICK));
    }
}

/// Relays one of the core's events while this module is loaded, resubscribing if it drops.
fn listen(name: &'static str, inner: Shared) {
    nap(Duration::from_millis(700));
    let mut pause = None;
    while !UNLOADING.load(Ordering::SeqCst) {
        let mut c = CoreClient::new();
        let sub = match name {
            "walletStateChanged" => c.on_wallet_state_changed(),
            _ => c.on_source_alert(),
        };
        let Ok(sub) = sub else {
            pause = switch::backoff(pause, true);
            nap(pause.unwrap_or_default());
            continue;
        };
        pause = None;
        while !UNLOADING.load(Ordering::SeqCst) && !sub.is_dead() {
            let Ok(ev) = sub.receiver().recv_timeout(Duration::from_millis(200)) else { continue };
            if UNLOADING.load(Ordering::SeqCst) {
                break;
            }
            if name == "walletStateChanged" {
                if let Some(e) = CoreClient::decode_wallet_state_changed(&ev) {
                    let Ok(st) = serde_json::from_str::<Value>(&e.payload) else { continue };
                    let v = {
                        let g = lock(&inner);
                        let entry = g.registry.get(st["network"].as_str().unwrap_or_default(), st["group"].as_str().unwrap_or_default());
                        view::status_view(st.clone(), &g.active, entry, g.sends.any_open())
                    };
                    emit_wallet_state_changed(&v.to_string());
                }
            } else if let Some(e) = CoreClient::decode_source_alert(&ev) {
                emit_source_alert(&e.payload);
            }
        }
    }
}

impl BitcoinWalletBackendModuleImpl {
    fn custodian(&self, method: &str, c: &Caller) -> bool {
        gate::custodian_admits(method, &self.roles.lock().unwrap_or_else(|e| e.into_inner()), c)
    }

    /// Source settings are locked while a wallet is open, or while it can't be told.
    fn settings_unlocked(&self) -> Result<String, String> {
        let st = core_status().map_err(|e| format!("can't tell whether a wallet is open: {e}"))?;
        let g = lock(&self.inner);
        if view::wallet_open(&st) || g.jobs.lifecycle_pending().is_some() {
            return Err("source settings are locked while a wallet is open; close it first".into());
        }
        Ok(g.active.clone())
    }

    fn effective_network(&self) -> (String, Option<Value>) {
        let st = core_status().ok();
        let active = lock(&self.inner).active.clone();
        (view::effective_network(st.as_ref(), &active), st)
    }

    /// Starts a send: checks the wallet, files the request and starts the core's build.
    fn start_send(&self, kind: Kind, requester: &str, core_kind: &str, params: Value) -> String {
        let st = match core_status() {
            Ok(s) => s,
            Err(e) => return err(format!("the wallet core did not answer: {e}")),
        };
        if !matches!(st["state"].as_str(), Some("syncing" | "ready")) {
            return err("no wallet is open");
        }
        let (network, group) = (st["network"].as_str().unwrap_or_default().to_string(), st["group"].as_str().unwrap_or_default().to_string());
        let rid = match lock(&self.inner).sends.begin(kind, requester, &network, &group, params.clone(), now_secs()) {
            Ok(r) => r,
            Err(e) => return err(e),
        };
        match start_core_job(&self.inner, core_kind, &params, Purpose::Build { request: rid.clone() }) {
            Ok(j) => {
                lock(&self.inner).sends.set_build_job(&rid, &j);
                save_sends(&self.inner);
                emit_send_status_changed(&rid, "preparing");
                ok(json!({"requestId": rid}))
            }
            Err(e) => {
                let changed = lock(&self.inner).sends.not_started(&rid, &e, None);
                save_sends(&self.inner);
                emit_changed(&rid, changed);
                json!({"ok": false, "requestId": rid, "error": e}).to_string()
            }
        }
    }
}

impl BitcoinWalletBackendModule for BitcoinWalletBackendModuleImpl {
    fn configure(&self, roles_json: String) -> String {
        if !self.custodian("configure", &caller()) {
            return refused();
        }
        let mut next = self.roles.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Err(e) = next.configure(&roles_json) {
            return err(e);
        }
        if let Some(dir) = lock(&self.inner).dir.clone() {
            if let Err(e) = write_atomic(&dir.join("roles.json"), next.to_json().to_string().as_bytes()) {
                return err(format!("roles not saved: {e}"));
            }
        }
        let v = next.to_json();
        *self.roles.lock().unwrap_or_else(|e| e.into_inner()) = next;
        ok(v)
    }

    fn caller_identity(&self) -> String {
        let c = caller();
        let identity = logos_rust_sdk::current_caller().identity();
        let roles = self.roles.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let file = lock(&self.inner).dir.as_ref().map(|d| d.join("roles.json").display().to_string());
        ok(json!({"kind": c.kind(), "identity": identity, "custodians": roles.custodians, "rolesFile": file}))
    }

    fn list_networks(&self) -> String {
        let g = lock(&self.inner);
        ok(json!({"networks": request::networks(g.regtest), "active": g.active, "pending": g.pending_network}))
    }

    fn set_active_network(&self, network: String) -> String {
        if !self.custodian("set_active_network", &caller()) {
            return refused();
        }
        if !request::is_network(&network, lock(&self.inner).regtest) {
            return err(format!("unknown network {network}"));
        }
        let st = match core_status() {
            Ok(s) => s,
            Err(e) => return coded("busy", format!("can't tell whether a wallet is open: {e}")),
        };
        let mut g = lock(&self.inner);
        if network == g.active {
            g.pending_network = None;
            return ok(json!({"active": network, "pending": null}));
        }
        let closing = g.jobs.close_pending() || st["state"] == "closing";
        match switch::plan(view::wallet_open(&st), closing, g.jobs.open_pending(), g.sends.any_open()) {
            Switch::Refuse(code, why) => coded(code, why),
            Switch::Queue => {
                g.pending_network = Some(network.clone());
                ok(json!({"active": g.active, "pending": network}))
            }
            Switch::Now => {
                g.active = network.clone();
                g.pending_network = None;
                if let Some(dir) = g.dir.clone() {
                    if let Err(e) = write_atomic(&dir.join("settings.json"), json!({"activeNetwork": network}).to_string().as_bytes()) {
                        eprintln!("bitcoin_wallet_backend: settings.json: {e}");
                    }
                }
                ok(json!({"active": network, "pending": null}))
            }
        }
    }

    fn list_wallets(&self) -> String {
        let active = lock(&self.inner).active.clone();
        // Wallets the core has on disk that this registry never saw, best effort.
        if let Ok(v) = ipc!(core(), list_wallets_async_with_timeout, READ, &active).and_then(|s| parse(s, "wallet core")) {
            let mut g = lock(&self.inner);
            for w in v["wallets"].as_array().cloned().unwrap_or_default() {
                let (group, family) = (w["group"].as_str().unwrap_or_default(), w["family"].as_str().unwrap_or_default());
                let _ = g.registry.discover(&active, group, family);
            }
        }
        let g = lock(&self.inner);
        ok(json!({"active": g.active, "wallets": g.registry.list_json()}))
    }

    fn open_wallet(&self, params_json: String) -> String {
        if !self.custodian("open_wallet", &caller()) {
            return refused();
        }
        let (active, regtest, pending) = {
            let g = lock(&self.inner);
            (g.active.clone(), g.regtest, g.pending_network.clone())
        };
        // Never open on the old network while a switch waits for a close to settle.
        if let Some(n) = pending {
            return coded("busy", format!("switching to {n} once the wallet has closed; try again"));
        }
        let p = match request::parse_open(&params_json, &active, regtest) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        match core_status() {
            Ok(st) if view::wallet_open(&st) => return err("a wallet is already open; close it first"),
            Ok(_) => {}
            Err(e) => return err(format!("the wallet core did not answer: {e}")),
        }
        if lock(&self.inner).jobs.lifecycle_pending().is_some() {
            return coded("busy", "a wallet is opening or closing; try again");
        }
        // Locks the network's source settings for as long as the wallet is open.
        set_wallet_open(&self.inner, &p.network, true);
        let purpose = Purpose::Open { network: p.network.clone(), label: p.label.clone() };
        match start_core_job(&self.inner, "open_wallet", &p.core_params(), purpose) {
            Ok(j) => ok(json!({"jobId": j})),
            Err(e) => {
                set_wallet_open(&self.inner, &p.network, false);
                err(e)
            }
        }
    }

    fn close_wallet(&self) -> String {
        if !self.custodian("close_wallet", &caller()) {
            return refused();
        }
        if let Some(s) = lock(&self.inner).sends.committing().map(String::from) {
            return err(format!("send {s} is going out; close the wallet once it settles"));
        }
        let open = lock(&self.inner).sends.open_ids();
        for id in open {
            let _ = withdraw(&self.inner, &id, Withdrawal::WalletClosed);
        }
        save_sends(&self.inner);
        let (network, _) = self.effective_network();
        match start_core_job(&self.inner, "close_wallet", &json!({}), Purpose::Close { network }) {
            Ok(j) => ok(json!({"jobId": j})),
            Err(e) => err(e),
        }
    }

    fn job_status(&self, job_id: String) -> String {
        lock(&self.inner).jobs.status_json(&job_id).to_string()
    }

    fn descriptors(&self) -> String {
        if !self.custodian("descriptors", &caller()) {
            return refused();
        }
        relay(ipc!(core(), descriptors_async_with_timeout, READ), "the wallet core")
    }

    fn set_birthday(&self, params_json: String) -> String {
        if !self.custodian("set_birthday", &caller()) {
            return refused();
        }
        let params = match request::parse_birthday(&params_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let st = match core_status() {
            Ok(s) if matches!(s["state"].as_str(), Some("syncing" | "ready")) => s,
            Ok(_) => return err("no wallet is open"),
            Err(e) => return err(format!("the wallet core did not answer: {e}")),
        };
        let purpose = Purpose::Birthday {
            network: st["network"].as_str().unwrap_or_default().into(),
            group: st["group"].as_str().unwrap_or_default().into(),
        };
        match start_core_job(&self.inner, "set_birthday", &params, purpose) {
            Ok(j) => ok(json!({"jobId": j})),
            Err(e) => err(e),
        }
    }

    fn wallet_status(&self) -> String {
        let st = match core_status() {
            Ok(s) => s,
            Err(e) => return err(format!("the wallet core did not answer: {e}")),
        };
        let g = lock(&self.inner);
        let entry = g.registry.get(st["network"].as_str().unwrap_or_default(), st["group"].as_str().unwrap_or_default());
        ok(view::status_view(st.clone(), &g.active, entry, g.sends.any_open()))
    }

    fn balances(&self) -> String {
        match ipc!(core(), balance_async_with_timeout, READ).and_then(|s| parse(s, "wallet core")) {
            Ok(v) if v["ok"] == true => view::balance_view(v).to_string(),
            Ok(v) => v.to_string(),
            Err(e) => err(format!("the wallet core did not answer: {e}")),
        }
    }

    fn history(&self, page: i64) -> String {
        match ipc!(core(), history_async_with_timeout, READ, page.max(0)).and_then(|s| parse(s, "wallet core")) {
            Ok(v) if v["ok"] == true => view::history_view(v).to_string(),
            Ok(v) => v.to_string(),
            Err(e) => err(format!("the wallet core did not answer: {e}")),
        }
    }

    fn receive_address(&self) -> String {
        relay(ipc!(core(), next_address_async_with_timeout, READ), "the wallet core")
    }

    fn fee_rates(&self) -> String {
        relay(ipc!(core(), fee_rates_async_with_timeout, READ), "the wallet core")
    }

    fn address_valid(&self, text: String) -> String {
        let (network, _) = self.effective_network();
        relay(ipc!(core(), address_valid_async_with_timeout, READ, &text, &network), "the wallet core")
    }

    fn format_btc(&self, sat: String) -> String {
        match amount::parse_sat(&sat) {
            Ok(s) => ok(json!({"sat": s.to_string(), "btc": amount::format_btc(s)})),
            Err(e) => err(e),
        }
    }

    fn parse_btc(&self, btc: String) -> String {
        match amount::parse_btc(&btc) {
            Ok(s) => ok(json!({"btc": amount::format_btc(s), "sat": s.to_string()})),
            Err(e) => err(e),
        }
    }

    fn sources(&self) -> String {
        let (network, _) = self.effective_network();
        relay(ipc!(node(), get_sources_async_with_timeout, READ, &network), "bitcoin_node_module")
    }

    fn source_health(&self) -> String {
        let (network, st) = self.effective_network();
        let mut v = match ipc!(node(), source_health_async_with_timeout, READ, &network).and_then(|s| parse(s, "node module")) {
            Ok(v) => v,
            Err(e) => return err(format!("bitcoin_node_module did not answer: {e}")),
        };
        if st.as_ref().is_some_and(view::wallet_open) {
            if let Ok(w) = ipc!(core(), sources_status_async_with_timeout, READ).and_then(|s| parse(s, "wallet core")) {
                v["wallet"] = w;
            }
        }
        v.to_string()
    }

    fn probe(&self, params_json: String) -> String {
        let regtest = lock(&self.inner).regtest;
        let (network, source) = match request::parse_probe(&params_json, regtest) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let network = network.unwrap_or_else(|| self.effective_network().0);
        relay(ipc!(node(), probe_async_with_timeout, PROBE, &network, &source), "bitcoin_node_module")
    }

    fn exposure(&self) -> String {
        relay(ipc!(core(), exposure_async_with_timeout, READ), "the wallet core")
    }

    fn set_sources(&self, list_json: String) -> String {
        if !self.custodian("set_sources", &caller()) {
            return refused();
        }
        match self.settings_unlocked() {
            Ok(net) => relay(ipc!(node(), set_sources_async_with_timeout, WRITE, &net, &list_json), "bitcoin_node_module"),
            Err(e) => err(e),
        }
    }

    fn set_source(&self, source_id: String, changes_json: String) -> String {
        if !self.custodian("set_source", &caller()) {
            return refused();
        }
        match self.settings_unlocked() {
            Ok(net) => relay(ipc!(node(), set_source_async_with_timeout, WRITE, &net, &source_id, &changes_json), "bitcoin_node_module"),
            Err(e) => err(e),
        }
    }

    fn set_proxy(&self, proxy: String) -> String {
        if !self.custodian("set_proxy", &caller()) {
            return refused();
        }
        match self.settings_unlocked() {
            Ok(_) => relay(ipc!(node(), set_proxy_async_with_timeout, WRITE, &proxy), "bitcoin_node_module"),
            Err(e) => err(e),
        }
    }

    fn clear_suspect(&self, source_id: String) -> String {
        if !self.custodian("clear_suspect", &caller()) {
            return refused();
        }
        // Not locked by an open wallet: the core marks sources suspect while it syncs one.
        let (network, st) = self.effective_network();
        let n = ipc!(node(), clear_suspect_async_with_timeout, WRITE, &network, &source_id)
            .and_then(|s| parse(s, "node module"))
            .unwrap_or_else(|e| json!({"ok": false, "error": e}));
        let mut out = json!({"ok": n["ok"] == true, "node": n});
        if st.as_ref().is_some_and(view::wallet_open) {
            let w = ipc!(core(), clear_suspect_async_with_timeout, WRITE, &source_id)
                .and_then(|s| parse(s, "wallet core"))
                .unwrap_or_else(|e| json!({"ok": false, "error": e}));
            out["ok"] = json!(out["ok"] == true || w["ok"] == true);
            out["wallet"] = w;
        }
        out.to_string()
    }

    fn set_own_peers(&self, params_json: String) -> String {
        if !self.custodian("set_own_peers", &caller()) {
            return refused();
        }
        let regtest = lock(&self.inner).regtest;
        let (network, peers) = match request::parse_own_peers(&params_json, regtest) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        match self.settings_unlocked() {
            Ok(_) => relay(ipc!(node(), set_own_peers_async_with_timeout, WRITE, &network, &peers), "bitcoin_node_module"),
            Err(e) => err(e),
        }
    }

    fn peer_list(&self, params_json: String) -> String {
        if !self.custodian("peer_list", &caller()) {
            return refused();
        }
        let regtest = lock(&self.inner).regtest;
        let network = match request::parse_peer_list(&params_json, regtest) {
            Ok(n) => n.unwrap_or_else(|| self.effective_network().0),
            Err(e) => return err(e),
        };
        relay(ipc!(node(), peer_list_async_with_timeout, READ, &network), "bitcoin_node_module")
    }

    fn request_send(&self, request_json: String) -> String {
        let c = caller();
        if !gate::requester_admits("request_send", &c) {
            return refused();
        }
        let requester = c.named().unwrap_or_default().to_string();
        let (params, checks) = match request::parse_send(&request_json) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let (network, _) = self.effective_network();
        for chk in checks {
            let v = match ipc!(core(), address_valid_async_with_timeout, READ, &chk.address, &network).and_then(|s| parse(s, "wallet core")) {
                Ok(v) => v,
                Err(e) => return err(format!("the wallet core did not answer: {e}")),
            };
            if v["valid"] != true {
                let why = v["reason"].as_str().or(v["error"].as_str()).unwrap_or("not a valid address");
                return err(format!("recipient {}: {why}", chk.index + 1));
            }
            if chk.needs_uri_amount && v["amountSat"].as_str().is_none_or(|a| a.is_empty() || a == "0") {
                return err(format!("recipient {}: give an amount; the URI names none", chk.index + 1));
            }
        }
        self.start_send(Kind::Send, &requester, "build_send", params)
    }

    fn request_fee_bump(&self, request_json: String) -> String {
        let c = caller();
        if !gate::requester_admits("request_fee_bump", &c) {
            return refused();
        }
        let requester = c.named().unwrap_or_default().to_string();
        match request::parse_bump(&request_json) {
            Ok(p) => self.start_send(Kind::FeeBump, &requester, "bump_fee", p),
            Err(e) => err(e),
        }
    }

    fn send_status(&self, request_id: String) -> String {
        lock(&self.inner).sends.status_json(request_id.trim()).to_string()
    }

    fn list_sends(&self) -> String {
        ok(json!({"sends": lock(&self.inner).sends.list_json()}))
    }

    fn cancel_send(&self, request_id: String) -> String {
        let c = caller();
        let id = request_id.trim().to_string();
        let Some(requester) = lock(&self.inner).sends.get(&id).map(|s| s.requester.clone()) else {
            // Unknown sends are refused like foreign ones, so ids can't be probed.
            return if self.roles.lock().unwrap_or_else(|e| e.into_inner()).holds(&c) { err("unknown send") } else { refused() };
        };
        if !gate::canceller_admits(&self.roles.lock().unwrap_or_else(|e| e.into_inner()), &c, &requester) {
            return refused();
        }
        let who = c.named().unwrap_or("a custodian").to_string();
        if let Err(e) = withdraw(&self.inner, &id, Withdrawal::Caller(who)) {
            return err(e);
        }
        save_sends(&self.inner);
        let g = lock(&self.inner);
        let s = g.sends.status_json(&id);
        ok(json!({"requestId": id, "state": s["state"], "cancelling": s["cancelling"]}))
    }

    fn rebroadcast(&self, request_or_txid: String) -> String {
        if !self.custodian("rebroadcast", &caller()) {
            return refused();
        }
        let arg = request_or_txid.trim().to_ascii_lowercase();
        let (rid, txid) = {
            let g = lock(&self.inner);
            if request::is_txid(&arg) {
                (g.sends.by_txid(&arg).map(String::from), arg.clone())
            } else {
                match g.sends.get(&arg) {
                    None => return err("unknown send"),
                    Some(s) if !matches!(s.state, sends::State::Unknown | sends::State::Sent) => {
                        return err(format!("send is {}: nothing went out to rebroadcast", s.state.name()))
                    }
                    Some(s) if s.txid.is_empty() => return err("the send has no transaction yet"),
                    Some(s) => (Some(s.id.clone()), s.txid.clone()),
                }
            }
        };
        match start_core_job(&self.inner, "rebroadcast", &json!({"txid": txid}), Purpose::Rebroadcast { request: rid, txid: txid.clone() }) {
            Ok(j) => ok(json!({"jobId": j, "txid": txid})),
            Err(e) => err(e),
        }
    }

    fn abandon_send(&self, params_json: String) -> String {
        if !self.custodian("abandon_send", &caller()) {
            return refused();
        }
        let txid = match request::parse_abandon(&params_json) {
            Ok(t) => t,
            Err(e) => return err(e),
        };
        let rid = {
            let g = lock(&self.inner);
            match g.sends.by_txid(&txid).and_then(|id| g.sends.get(id)) {
                None => return err("no send of this wallet has that txid"),
                Some(s) if s.state != sends::State::Unknown => {
                    return err(format!("send {} is {}: only a send whose outcome is unknown can be abandoned", s.id, s.state.name()))
                }
                Some(s) if s.abandoned => return err(format!("send {} was abandoned already", s.id)),
                Some(s) => s.id.clone(),
            }
        };
        let purpose = Purpose::Abandon { request: rid.clone(), txid: txid.clone() };
        match start_core_job(&self.inner, "abandon_send", &json!({"txid": txid}), purpose) {
            Ok(j) => ok(json!({"jobId": j, "requestId": rid, "txid": txid, "note": sends::ABANDONED})),
            Err(e) => err(e),
        }
    }

    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let dir = PathBuf::from(&ctx.instance_persistence_path);
        let roles = std::fs::read_to_string(dir.join("roles.json")).ok();
        *self.roles.lock().unwrap_or_else(|e| e.into_inner()) = Roles::from_file(roles.as_deref());
        {
            let mut g = lock(&self.inner);
            // A local test chain, for test harnesses only; the core and node module read the same file.
            g.regtest = dir.join("regtest.json").is_file();
            if let Ok(s) = std::fs::read_to_string(dir.join("settings.json")) {
                if let Some(n) = serde_json::from_str::<Value>(&s).ok().and_then(|v| v["activeNetwork"].as_str().map(String::from)) {
                    if request::is_network(&n, g.regtest) {
                        g.active = n;
                    }
                }
            }
            g.registry = Registry::new(Some(dir.join("wallets.json")));
            // Sends survive a restart; open ones are reconciled with the core's previews.
            match std::fs::read_to_string(dir.join("sends.json")).map(|t| serde_json::from_str::<Value>(&t)) {
                Ok(Ok(v)) => g.sends = Sends::load(&v, Instant::now()),
                Ok(Err(e)) => eprintln!("bitcoin_wallet_backend: sends.json unreadable ({e}); starting with none"),
                Err(_) => {}
            }
            g.dir = Some(dir);
        }
        // Nothing outbound here: the threads make every call, after the host has wired us.
        let mut threads = self.threads.lock().unwrap_or_else(|e| e.into_inner());
        let inner = self.inner.clone();
        threads.push(std::thread::spawn(move || reactor(inner)));
        for name in ["walletStateChanged", "sourceAlert"] {
            let inner = self.inner.clone();
            threads.push(std::thread::spawn(move || listen(name, inner)));
        }
    }
}

impl AboutToUnload for BitcoinWalletBackendModuleImpl {
    /// Stops the threads; IPC waits answer at once, so they join within the grace period.
    fn about_to_unload(&self) -> Shutdown {
        UNLOADING.store(true, Ordering::SeqCst);
        let threads: Vec<JoinHandle<()>> = std::mem::take(&mut *self.threads.lock().unwrap_or_else(|e| e.into_inner()));
        let until = Instant::now() + Duration::from_millis(1500);
        for t in threads {
            while !t.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
        Shutdown::Synchronous
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    logos_install!(BitcoinWalletBackendModuleImpl);
}
