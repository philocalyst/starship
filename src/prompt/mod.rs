//! The incremental prompt: how a module's work is reused, refreshed, and shown.
//!
//! The shape of this module follows one rule — *a value may be reused only
//! against a stated reason*. Everything else falls out of it:
//!
//! * [`deps`] is how a reason is stated: the observations a render rests on.
//!   The same declaration is the cache key and the explanation, so those two
//!   cannot drift apart.
//! * [`keying`] is the small vocabulary modules draw those observations from,
//!   plus the one table saying which module uses which. It separates *costly*
//!   from *unstable*, which a duration threshold cannot distinguish.
//! * [`store`] keeps completed renders as structure rather than finished ANSI,
//!   and refuses to hand one back without first proving its reasons still hold.
//! * [`policy`] is what a paint does when a reason no longer holds — compute
//!   now, show the previous value while a refresh runs, or omit.
//! * The core render path handles both complete and deferred paints. Cache
//!   state is on disk, not in a Starship daemon.
//! * [`watchman`] optionally relays Facebook Watchman events to a shell, so a
//!   filesystem change can schedule a normal background refresh.

pub mod deps;
pub mod keying;
pub mod policy;
pub mod render;
pub mod store;
pub mod watchman;

pub use deps::{Deps, Envelope, Fingerprint, WatchTarget};
pub use keying::{Keying, Profile, profile};
pub use policy::{Provenance, RenderPolicy};
pub use render::{Render, Resolved};
pub use store::{Entry, Snapshot, Store};
