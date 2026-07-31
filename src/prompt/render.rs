//! The incremental-render state for a single prompt.
//!
//! One of these hangs off [`Context`], which is what replaced the process-global
//! execution mode. The difference is not stylistic. A global can describe only
//! one render at a time, so the background pass had to build a whole second
//! [`Context`] — reparsing the configuration, recanonicalizing the directory,
//! rediscovering the repository — to render the right prompt after the left.
//! Carrying the state as a value makes rendering both prompts from one set of
//! gathered facts a matter of calling twice.
//!
//! [`Context`]: crate::context::Context

use std::collections::BTreeSet;
use std::sync::Mutex;

use crate::segment::Segment;

use super::deps::Envelope;
use super::policy::{Provenance, RenderPolicy};
use super::store::{Entry, Snapshot};

/// What a module lookup produced, and on what standing.
pub struct Resolved {
    pub segments: Vec<Segment>,
    pub provenance: Provenance,
}

/// Everything one prompt needs to reuse, record, and report on module work.
///
/// Modules render in parallel, so the mutable halves are behind locks. They are
/// deliberately separate locks: recording a completed module and noting a
/// provisional one are unrelated events, and a single lock would serialize
/// every module against every other at exactly the moment parallelism matters.
#[derive(Debug)]
pub struct Render {
    policy: RenderPolicy,
    /// What a previous pass left behind for this directory.
    previous: Snapshot,
    /// What this pass is producing. Empty unless the policy records.
    recorded: Mutex<Snapshot>,
    /// Modules shown from a lapsed entry, or not shown at all — the refresh
    /// list. Ordered, so a repaint request is stable and testable.
    provisional: Mutex<BTreeSet<String>>,
}

impl Default for Render {
    fn default() -> Self {
        Self::new(RenderPolicy::default(), Snapshot::default())
    }
}

impl Render {
    pub fn new(policy: RenderPolicy, previous: Snapshot) -> Self {
        Self {
            policy,
            previous,
            recorded: Mutex::new(Snapshot::default()),
            provisional: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn policy(&self) -> RenderPolicy {
        self.policy
    }

    pub fn previous(&self) -> &Snapshot {
        &self.previous
    }

    /// Reuse a valid entry, or say why one cannot be had.
    ///
    /// The three outcomes correspond exactly to the three ways a paint can end
    /// up without fresh work: the entry was good ([`Provenance::Reused`]), it
    /// had lapsed but the caller accepts a previous reading
    /// ([`Provenance::Stale`]), or there is nothing at all
    /// ([`Provenance::Pending`]). Returning `None` means the caller must
    /// compute.
    pub fn reuse(&self, module: &str, envelope: &Envelope) -> Option<Resolved> {
        if let Some(entry) = self.previous.get(module, envelope) {
            // Carry the entry forward into what this pass will persist.
            // Without this a refresh would write back only what it recomputed,
            // silently dropping every module it was able to reuse -- which
            // would make the next paint recompute them and the cache oscillate.
            // Doing it here rather than with a separate "was resolved" set also
            // prunes naturally: a module no longer in the prompt is never
            // reused, so it never carries forward.
            if self.policy.records()
                && let Ok(mut recorded) = self.recorded.lock()
            {
                recorded.insert(module, entry.clone());
            }
            return Some(Resolved {
                segments: entry.segments().to_vec(),
                provenance: Provenance::Reused,
            });
        }

        if self.policy.may_compute() {
            // The caller can afford the real answer, so a lapsed entry is of no
            // interest — taking it would be strictly worse than the work.
            return None;
        }

        self.note_provisional(module);

        match self.previous.get_stale(module) {
            Some(entry) => Some(Resolved {
                segments: entry.segments().to_vec(),
                provenance: Provenance::Stale,
            }),
            None => Some(Resolved {
                segments: Vec::new(),
                provenance: Provenance::Pending,
            }),
        }
    }

    /// Record a completed render, if this pass is one that records.
    pub fn record(&self, module: &str, entry: Entry) {
        if !self.policy.records() {
            return;
        }
        if let Ok(mut recorded) = self.recorded.lock() {
            recorded.insert(module, entry);
        }
    }

    /// Note that a module was shown provisionally, or not at all.
    pub fn note_provisional(&self, module: &str) {
        if let Ok(mut provisional) = self.provisional.lock() {
            provisional.insert(module.to_owned());
        }
    }

    /// The modules a refresh should attend to.
    ///
    /// Empty means the paint was complete, which is the signal to *not* ask for
    /// a refresh at all. That distinction is what makes an idle prompt free:
    /// with no lapsed modules there is no request, so nothing wakes up.
    pub fn provisional(&self) -> Vec<String> {
        self.provisional
            .lock()
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether anything shown was provisional.
    pub fn is_complete(&self) -> bool {
        self.provisional
            .lock()
            .is_ok_and(|provisional| provisional.is_empty())
    }

    /// Take what this pass recorded, for writing to the store.
    pub fn take_recorded(&self) -> Snapshot {
        self.recorded
            .lock()
            .map(|mut recorded| std::mem::take(&mut *recorded))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::deps::Deps;
    use crate::prompt::store::Store;

    fn envelope() -> Envelope {
        Envelope::new("v", "cfg")
    }

    fn snapshot_with(module: &str, entry: Entry) -> Snapshot {
        let mut snapshot = Snapshot::default();
        snapshot.insert(module, entry);
        snapshot
    }

    #[test]
    fn a_valid_entry_is_reused_under_every_policy() {
        let entry = Store::keyed(envelope(), Deps::default(), Segment::from_text(None, "x"));
        let previous = snapshot_with("nodejs", entry);

        for policy in [
            RenderPolicy::Complete,
            RenderPolicy::Immediate,
            RenderPolicy::Refresh,
        ] {
            let render = Render::new(policy, previous.clone());
            let resolved = render.reuse("nodejs", &envelope()).expect("reusable");
            assert_eq!(resolved.provenance, Provenance::Reused);
        }
    }

    #[test]
    fn a_computing_policy_refuses_a_lapsed_entry() {
        // A stale value is only ever a substitute for waiting. A caller that is
        // willing to wait must not be given one.
        let entry = Store::keyed(
            Envelope::new("v", "old"),
            Deps::default(),
            Segment::from_text(None, "x"),
        );
        let render = Render::new(RenderPolicy::Complete, snapshot_with("nodejs", entry));

        assert!(render.reuse("nodejs", &envelope()).is_none());
    }

    #[test]
    fn the_fast_paint_shows_a_lapsed_entry_and_flags_it() {
        let entry = Store::keyed(
            Envelope::new("v", "old"),
            Deps::default(),
            Segment::from_text(None, "old-value"),
        );
        let render = Render::new(RenderPolicy::Immediate, snapshot_with("nodejs", entry));

        let resolved = render.reuse("nodejs", &envelope()).expect("shown");
        assert_eq!(resolved.provenance, Provenance::Stale);
        assert_eq!(render.provisional(), vec!["nodejs"]);
        assert!(!render.is_complete());
    }

    #[test]
    fn a_module_with_nothing_stored_is_pending_not_absent() {
        let render = Render::new(RenderPolicy::Immediate, Snapshot::default());

        let resolved = render.reuse("nodejs", &envelope()).expect("resolved");
        assert_eq!(resolved.provenance, Provenance::Pending);
        assert!(resolved.segments.is_empty());
        assert_eq!(
            render.provisional(),
            vec!["nodejs"],
            "a module that could not be shown is the strongest reason to refresh",
        );
    }

    #[test]
    fn a_complete_paint_asks_for_nothing() {
        let entry = Store::keyed(envelope(), Deps::default(), Segment::from_text(None, "x"));
        let render = Render::new(RenderPolicy::Immediate, snapshot_with("nodejs", entry));

        render.reuse("nodejs", &envelope()).expect("reusable");

        assert!(
            render.is_complete(),
            "with nothing lapsed there is no refresh to request, which is what \
             makes an idle prompt cost nothing",
        );
    }

    #[test]
    fn a_refresh_carries_forward_what_it_reused() {
        let entry = Store::keyed(envelope(), Deps::default(), Segment::from_text(None, "x"));
        let render = Render::new(RenderPolicy::Refresh, snapshot_with("nodejs", entry));

        render.reuse("nodejs", &envelope()).expect("reusable");

        let persisted = render.take_recorded();
        assert!(
            persisted.get("nodejs", &envelope()).is_some(),
            "a refresh writes a whole snapshot; a reused module missing from it \
             would be dropped, recomputed next paint, and the cache would \
             oscillate between the two",
        );
    }

    #[test]
    fn a_module_no_longer_in_the_prompt_is_not_carried_forward() {
        let entry = Store::keyed(envelope(), Deps::default(), Segment::from_text(None, "x"));
        let mut previous = snapshot_with("nodejs", entry.clone());
        previous.insert("removed_module", entry);

        let render = Render::new(RenderPolicy::Refresh, previous);
        render.reuse("nodejs", &envelope()).expect("reusable");

        let persisted = render.take_recorded();
        assert!(persisted.get("removed_module", &envelope()).is_none());
    }

    #[test]
    fn only_a_recording_policy_keeps_what_it_computed() {
        let entry = || Store::keyed(envelope(), Deps::default(), Segment::from_text(None, "x"));

        let refreshing = Render::new(RenderPolicy::Refresh, Snapshot::default());
        refreshing.record("nodejs", entry());
        assert!(!refreshing.take_recorded().is_empty());

        let painting = Render::new(RenderPolicy::Immediate, Snapshot::default());
        painting.record("nodejs", entry());
        assert!(painting.take_recorded().is_empty());
    }
}
