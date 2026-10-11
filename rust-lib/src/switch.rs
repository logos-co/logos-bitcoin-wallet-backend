//! Switching the active network: now, after a close that is still settling, or not yet.

/// What a `set_active_network` does.
#[derive(Debug, PartialEq, Eq)]
pub enum Switch {
    /// Nothing is open or settling: switch now.
    Now,
    /// A close is settling: switch once it has, and refuse opens until then.
    Queue,
    /// Refused, with a code the app can act on.
    Refuse(&'static str, String),
}

/// `closing`: a close job runs or the core says closing. `opening`: an open job runs.
pub fn plan(wallet_open: bool, closing: bool, opening: bool, open_send: Option<&str>) -> Switch {
    if let Some(s) = open_send {
        return Switch::Refuse("send_open", format!("send {s} is still open"));
    }
    if closing {
        return Switch::Queue;
    }
    if opening {
        return Switch::Refuse("busy", "a wallet is opening; close it, or wait, then switch".into());
    }
    if wallet_open {
        return Switch::Refuse("wallet_open", "close the open wallet before switching networks".into());
    }
    Switch::Now
}

/// A queued switch applies once nothing is open and no open or close runs.
pub fn applies(wallet_open: bool, lifecycle_pending: bool) -> bool {
    !wallet_open && !lifecycle_pending
}

/// The next pause of a poller: 100 ms after a first failure, doubling to 5 s; `None` after a success.
pub fn backoff(prev: Option<std::time::Duration>, failed: bool) -> Option<std::time::Duration> {
    use std::time::Duration;
    if !failed {
        return None;
    }
    Some(prev.map_or(Duration::from_millis(100), |d| (d * 2).min(Duration::from_secs(5))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_switch_during_a_close_waits_for_it() {
        assert_eq!(plan(false, false, false, None), Switch::Now);
        assert_eq!(plan(true, true, false, None), Switch::Queue, "a close settling: queued, not refused");
        assert_eq!(plan(false, true, true, None), Switch::Queue, "a close that cancels an open");
        assert!(matches!(plan(false, false, true, None), Switch::Refuse("busy", _)));
        assert!(matches!(plan(true, false, false, None), Switch::Refuse("wallet_open", _)));
        assert!(matches!(plan(false, true, false, Some("s3")), Switch::Refuse("send_open", m) if m.contains("s3")));
        assert!(applies(false, false) && !applies(true, false) && !applies(false, true));
    }

    #[test]
    fn backoff_doubles_to_five_seconds_and_resets() {
        let mut d = None;
        let mut seen = vec![];
        for _ in 0..8 {
            d = backoff(d, true);
            seen.push(d.unwrap().as_millis());
        }
        assert_eq!(seen, vec![100, 200, 400, 800, 1600, 3200, 5000, 5000]);
        assert_eq!(backoff(d, false), None, "a success resets it");
        assert_eq!(backoff(None, true), Some(Duration::from_millis(100)));
    }
}
