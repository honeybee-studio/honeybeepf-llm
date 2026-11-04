//! Responses calls made with `"background": true`, waiting for the later GET that carries their usage.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// Calls kept at most; the one idle longest goes first.
const MAX_CALLS: usize = 4096;
/// A call not polled for this long is given up on.
const IDLE_TTL: Duration = Duration::from_secs(3600);

/// The create request of a call still running, so its result is recorded against it.
pub struct PendingCall {
    pub start_time: Instant,
    pub pid: u32,
    #[cfg(feature = "k8s")]
    pub pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
    last_seen: Instant,
}

impl PendingCall {
    pub fn new(
        start_time: Instant,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Self {
        Self {
            start_time,
            pid,
            #[cfg(feature = "k8s")]
            pod_info,
            last_seen: Instant::now(),
        }
    }
}

/// Running background calls by response id, for every connection and process on the node.
#[derive(Default)]
pub struct BackgroundCalls {
    calls: HashMap<Box<str>, PendingCall>,
}

impl BackgroundCalls {
    /// Remember a call; returns the one dropped to stay under MAX_CALLS.
    pub fn insert(&mut self, id: Box<str>, call: PendingCall) -> Option<PendingCall> {
        let evicted = if self.calls.len() >= MAX_CALLS && !self.calls.contains_key(&id) {
            let oldest = self
                .calls
                .iter()
                .min_by_key(|(_, c)| c.last_seen)
                .map(|(k, _)| k.clone());
            oldest.and_then(|k| self.calls.remove(&k))
        } else {
            None
        };
        self.calls.insert(id, call);
        evicted
    }

    pub fn contains(&self, id: &str) -> bool {
        self.calls.contains_key(id)
    }

    /// A poll saw the call still running.
    pub fn touch(&mut self, id: &str) {
        if let Some(call) = self.calls.get_mut(id) {
            call.last_seen = Instant::now();
        }
    }

    /// The call whose result arrived; None when another poll already took it.
    pub fn take(&mut self, id: &str) -> Option<PendingCall> {
        self.calls.remove(id)
    }

    /// Calls not polled within IDLE_TTL of `now`.
    pub fn expire(&mut self, now: Instant) -> impl Iterator<Item = PendingCall> + '_ {
        self.calls
            .extract_if(move |_, c| now.duration_since(c.last_seen) >= IDLE_TTL)
            .map(|(_, c)| c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> PendingCall {
        PendingCall::new(
            Instant::now(),
            1,
            #[cfg(feature = "k8s")]
            None,
        )
    }

    #[test]
    fn a_result_is_taken_once() {
        let mut calls = BackgroundCalls::default();
        assert!(calls.insert("resp_1".into(), call()).is_none());
        assert!(calls.contains("resp_1"));
        assert!(calls.take("resp_1").is_some());
        assert!(calls.take("resp_1").is_none());
        assert!(!calls.contains("resp_1"));
    }

    #[test]
    fn the_longest_idle_call_makes_room() {
        let mut calls = BackgroundCalls::default();
        for i in 0..MAX_CALLS {
            calls.insert(format!("resp_{i}").into(), call());
        }
        calls.touch("resp_0");
        let evicted = calls.insert("resp_new".into(), call());
        assert!(evicted.is_some());
        assert_eq!(calls.calls.len(), MAX_CALLS);
        assert!(calls.contains("resp_0") && calls.contains("resp_new"));
        // The same id again replaces, it does not evict.
        assert!(calls.insert("resp_new".into(), call()).is_none());
    }

    #[test]
    fn only_calls_idle_past_the_ttl_expire() {
        let mut calls = BackgroundCalls::default();
        calls.insert("resp_1".into(), call());
        let now = Instant::now();
        assert_eq!(calls.expire(now + IDLE_TTL / 2).count(), 0);
        assert_eq!(calls.expire(now + IDLE_TTL).count(), 1);
        assert!(calls.calls.is_empty());
    }
}
