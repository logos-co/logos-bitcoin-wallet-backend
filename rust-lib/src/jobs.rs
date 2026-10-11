//! Wallet-core jobs this backend started, under its own ids. The core's receipts never leave
//! this module, so a job id that crosses the event plane authorises nothing.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// What a job is for, which decides what its outcome moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Purpose {
    Open { network: String, label: Option<String> },
    Close { network: String },
    Build { request: String },
    Sign { request: String },
    CancelSend { request: String },
    Rebroadcast { request: Option<String>, txid: String },
    Abandon { request: String, txid: String },
}

impl Purpose {
    /// Opens, closes and rebroadcasts are polled by callers; send jobs show through send_status.
    pub fn public(&self) -> bool {
        matches!(self, Purpose::Open { .. } | Purpose::Close { .. } | Purpose::Rebroadcast { .. } | Purpose::Abandon { .. })
    }

    pub fn request(&self) -> Option<&str> {
        match self {
            Purpose::Build { request } | Purpose::Sign { request } | Purpose::CancelSend { request } | Purpose::Abandon { request, .. } => Some(request),
            Purpose::Rebroadcast { request, .. } => request.as_deref(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Job {
    pub kind: String,
    pub core_id: String,
    pub receipt: String,
    pub state: String,
    pub result: Value,
    pub error: String,
    pub misses: u32,
    pub purpose: Purpose,
    pub finished_at: Option<Instant>,
}

impl Job {
    pub fn finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "cancelled")
    }
}

/// Consecutive unanswered polls before a job is called lost: the core is not coming back.
pub const GIVE_UP_AFTER: u32 = 10;
/// Finished jobs are forgotten after this long.
const KEEP_FINISHED: Duration = Duration::from_secs(600);

#[derive(Default)]
pub struct Jobs {
    next: u64,
    map: BTreeMap<String, Job>,
}

impl Jobs {
    pub fn track(&mut self, kind: &str, core_id: &str, receipt: &str, purpose: Purpose) -> String {
        self.prune();
        self.next += 1;
        let id = format!("b{}", self.next);
        self.map.insert(
            id.clone(),
            Job {
                kind: kind.into(),
                core_id: core_id.into(),
                receipt: receipt.into(),
                state: "queued".into(),
                result: Value::Null,
                error: String::new(),
                misses: 0,
                purpose,
                finished_at: None,
            },
        );
        id
    }

    pub fn get(&self, id: &str) -> Option<&Job> {
        self.map.get(id)
    }

    /// `(id, core id, receipt)` of every job still queued or running.
    pub fn pending(&self) -> Vec<(String, String, String)> {
        self.map.iter().filter(|(_, j)| !j.finished()).map(|(k, j)| (k.clone(), j.core_id.clone(), j.receipt.clone())).collect()
    }

    /// The network of an open still running, or of a close.
    pub fn lifecycle_pending(&self) -> Option<String> {
        self.map.values().filter(|j| !j.finished()).find_map(|j| match &j.purpose {
            Purpose::Open { network, .. } | Purpose::Close { network } => Some(network.clone()),
            _ => None,
        })
    }

    /// The core answered: the job is queued or running still.
    pub fn answered(&mut self, id: &str, state: &str) {
        if let Some(j) = self.map.get_mut(id) {
            j.misses = 0;
            if !j.finished() && matches!(state, "queued" | "running") {
                j.state = state.into();
            }
        }
    }

    /// The core didn't answer; true once the job is given up as lost.
    pub fn miss(&mut self, id: &str) -> bool {
        let Some(j) = self.map.get_mut(id) else { return false };
        if j.finished() {
            return false;
        }
        j.misses += 1;
        if j.misses < GIVE_UP_AFTER {
            return false;
        }
        j.state = "failed".into();
        j.error = "the wallet core stopped answering".into();
        j.finished_at = Some(Instant::now());
        true
    }

    pub fn finish(&mut self, id: &str, state: &str, result: Value, error: String) {
        if let Some(j) = self.map.get_mut(id) {
            j.state = state.into();
            j.result = result;
            j.error = error;
            j.finished_at = Some(Instant::now());
        }
    }

    /// Only jobs handed to a caller answer; send jobs are internal.
    pub fn status_json(&self, id: &str) -> Value {
        match self.map.get(id).filter(|j| j.purpose.public()) {
            None => json!({"ok": false, "error": "unknown job"}),
            Some(j) => {
                let mut v = json!({"ok": true, "jobId": id, "kind": j.kind, "state": j.state});
                if j.state == "done" {
                    v["result"] = j.result.clone();
                }
                if j.state == "failed" || j.state == "cancelled" {
                    v["error"] = json!(j.error);
                }
                v
            }
        }
    }

    fn prune(&mut self) {
        self.map.retain(|_, j| j.finished_at.is_none_or(|t| t.elapsed() < KEEP_FINISHED));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_status_hides_core_ids_and_receipts() {
        let mut j = Jobs::default();
        let id = j.track("open_wallet", "j7", "secret-receipt", Purpose::Open { network: "signet".into(), label: None });
        let s = j.status_json(&id).to_string();
        assert!(!s.contains("secret-receipt") && !s.contains("\"j7\""), "{s}");
        assert_eq!(j.pending().len(), 1);
        assert_eq!(j.lifecycle_pending().as_deref(), Some("signet"));
        j.finish(&id, "done", json!({"group": "g_1"}), String::new());
        assert_eq!(j.status_json(&id)["result"]["group"], "g_1");
        assert!(j.lifecycle_pending().is_none());
    }

    #[test]
    fn send_jobs_are_internal() {
        let mut j = Jobs::default();
        let id = j.track("build_send", "j1", "r", Purpose::Build { request: "s1".into() });
        assert_eq!(j.status_json(&id)["ok"], false);
        assert_eq!(j.get(&id).unwrap().purpose.request(), Some("s1"));
    }

    #[test]
    fn a_core_that_stops_answering_is_given_up() {
        let mut j = Jobs::default();
        let id = j.track("close_wallet", "j1", "r", Purpose::Close { network: "signet".into() });
        for _ in 1..GIVE_UP_AFTER {
            assert!(!j.miss(&id));
        }
        j.answered(&id, "running");
        assert_eq!(j.get(&id).unwrap().misses, 0, "an answer resets the count");
        for _ in 1..GIVE_UP_AFTER {
            assert!(!j.miss(&id));
        }
        assert!(j.miss(&id));
        assert_eq!(j.status_json(&id)["state"], "failed");
        assert!(!j.miss(&id), "given up once");
    }
}
