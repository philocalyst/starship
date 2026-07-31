//! What a paint does when a module's reasons no longer hold.
//!
//! This replaces a process-global `OnceLock<ExecMode>` that the module
//! dispatcher reached out to. Ambient mode was a symptom: the render pipeline
//! had nowhere to say *how* a render should behave, so the information travelled
//! beside it instead of through it. It belongs on [`Context`], which is already
//! threaded to every module — and once it is a value rather than a global, a
//! single process can render under two policies, which is what lets one
//! background pass serve both prompts.
//!
//! [`Context`]: crate::context::Context

use serde::{Deserialize, Serialize};

/// How a render treats work it cannot reuse.
///
/// Note what is *not* here: there is no "read from cache" mode. Reuse is not a
/// policy, it is what happens whenever an entry is valid, under every policy.
/// The only real question is what to do about the remainder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RenderPolicy {
    /// Compute whatever cannot be reused, and wait for it.
    ///
    /// The classic synchronous prompt, and still the right behavior for
    /// scripts, `starship explain`, and anything capturing output — a caller
    /// that is not going to repaint must be given the real answer.
    #[default]
    Complete,

    /// Paint immediately: reuse what is valid, show the previous value for what
    /// is not, and omit what has no previous value at all.
    ///
    /// The interactive fast path. Anything shown from a superseded entry is
    /// marked [`Provenance::Stale`], so the caller can render it differently
    /// and the background refresh knows what to recompute.
    Immediate,

    /// Compute everything that cannot be reused, and record the results.
    ///
    /// The background pass. It differs from [`RenderPolicy::Complete`] only in
    /// that it writes to the store — the distinction being that a foreground
    /// render should not pay to persist work a repaint will not read.
    Refresh,
}

impl RenderPolicy {
    /// Whether completed work should be written to the store.
    ///
    /// Only the background pass records. A `Complete` render is usually a
    /// one-shot with no repaint to follow, and an `Immediate` render by
    /// definition computed nothing worth keeping.
    pub fn records(self) -> bool {
        matches!(self, Self::Refresh)
    }

    /// Whether this render may show a value whose reasons have lapsed.
    pub fn tolerates_stale(self) -> bool {
        matches!(self, Self::Immediate)
    }

    /// Whether this render may block on expensive work.
    pub fn may_compute(self) -> bool {
        !matches!(self, Self::Immediate)
    }
}

/// Where a module's shown value came from.
///
/// Carried alongside the render so the difference between "this is current" and
/// "this is the last thing we knew" survives to the point of use, instead of
/// being flattened into an indistinguishable string. That flattening is what
/// made the previous design unable to tell a user — or itself — that a segment
/// was provisional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Computed during this render.
    Computed,
    /// Reused from an entry whose observations still hold.
    Reused,
    /// Shown from a superseded entry while a refresh is pending.
    Stale,
    /// Nothing to show: expensive, not yet computed, and this paint would not
    /// wait for it.
    Pending,
}

impl Provenance {
    /// Whether a refresh should be asked for on account of this module.
    pub fn wants_refresh(self) -> bool {
        matches!(self, Self::Stale | Self::Pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_background_pass_records() {
        assert!(RenderPolicy::Refresh.records());
        assert!(!RenderPolicy::Complete.records());
        assert!(
            !RenderPolicy::Immediate.records(),
            "the fast paint computes nothing expensive, so it has nothing worth \
             persisting -- and writing from the paint path would put a disk \
             write in front of the user",
        );
    }

    #[test]
    fn the_fast_paint_is_the_only_one_that_may_show_stale_values() {
        assert!(RenderPolicy::Immediate.tolerates_stale());
        assert!(!RenderPolicy::Complete.tolerates_stale());
        assert!(
            !RenderPolicy::Refresh.tolerates_stale(),
            "a refresh exists to replace stale values; accepting them would \
             make it a no-op",
        );
    }

    #[test]
    fn the_fast_paint_never_blocks() {
        assert!(!RenderPolicy::Immediate.may_compute());
        assert!(RenderPolicy::Complete.may_compute());
        assert!(RenderPolicy::Refresh.may_compute());
    }

    #[test]
    fn provisional_values_ask_for_a_refresh() {
        assert!(Provenance::Stale.wants_refresh());
        assert!(Provenance::Pending.wants_refresh());
        assert!(!Provenance::Reused.wants_refresh());
        assert!(!Provenance::Computed.wants_refresh());
    }
}
