//! Retention and garbage collection: deleting the snapshots a policy does not keep and the chunks
//! that no kept snapshot needs.
//!
//! [`Repository::prune`](crate::Repository::prune) is the only operation that ever deletes
//! repository data, and it is built so that it cannot make a kept snapshot unrestorable:
//!
//! 1. **Mark.** Every snapshot is read through authenticated decryption. If any cannot be read,
//!    the set of chunks in use is unknown and prune **refuses** (`Error::UnreadableSnapshots`); it
//!    never guesses. The [`RetentionPolicy`] decides which snapshots are kept; the chunks they
//!    need are marked.
//! 2. **Check.** Every marked chunk must exist. If a kept snapshot is already missing chunks,
//!    prune refuses (`Error::KeptSnapshotsDamaged`): deleting the older, intact snapshot of the
//!    same data would destroy the last good copy.
//! 3. **Sweep, in a crash-safe order.** First the snapshot files that are not kept are deleted and
//!    their directory is made durable; only then are the objects that no kept snapshot needs
//!    deleted. At every instant, every snapshot file that exists has all of its chunks, so a crash
//!    or a cancellation at any point leaves a consistent repository: at worst unreferenced
//!    objects, which the next prune removes. Nothing is ever deleted that a kept snapshot needs,
//!    because the objects deleted are exactly those not marked.
//! 4. **Verify.** After the sweep every marked chunk is checked to still exist.
//!
//! Deletion is irreversible, so it is never the default. [`PruneMode::DryRun`] is, and executing
//! needs either the digest of a plan a person has seen ([`PruneMode::Confirmed`]) or an explicit
//! [`PruneMode::Unattended`]. A confirmed prune is executed only if the plan made under the
//! exclusive lock is the plan that was confirmed.
//!
//! A prune holds the **exclusive** lock (a dry run the shared one), so it never runs next to a
//! backup, a restore, a check, or another prune. Details and limits are in `docs/PRUNE.md`.

mod engine;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::backup::CancelToken;
use crate::retention::{Decision, RetentionPolicy};

pub(crate) use engine::{run, Subject};

/// Identifies exactly which snapshots a plan keeps and which it removes, so that a plan a person
/// has looked at can be executed and nothing else. It does not cover the chunks: those are
/// whatever the kept snapshots do not need at the moment of the prune.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlanDigest([u8; 32]);

impl PlanDigest {
    /// The digest of a set of decisions, independent of the order they are given in.
    pub(crate) fn of(decisions: &[Decision]) -> Self {
        let mut ordered: Vec<(&str, bool)> = decisions
            .iter()
            .map(|decision| (decision.id.as_str(), decision.is_kept()))
            .collect();
        ordered.sort_unstable();
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"vaultsnap prune plan v1\n");
        for (id, kept) in ordered {
            hasher.update(id.as_bytes());
            hasher.update(if kept { b" keep\n" } else { b" remove\n" });
        }
        Self(*hasher.finalize().as_bytes())
    }

    /// 64 lowercase hexadecimal characters.
    pub fn to_hex(&self) -> String {
        crate::util::hex(&self.0)
    }

    /// Parses the output of [`PlanDigest::to_hex`]; `None` for anything else.
    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes = crate::util::decode_hex(text)?;
        Some(Self(<[u8; 32]>::try_from(bytes).ok()?))
    }
}

impl fmt::Display for PlanDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for PlanDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PlanDigest({})", self.to_hex())
    }
}

/// Whether a prune deletes anything, and on whose authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PruneMode {
    /// Work out and report the plan; delete nothing. This is the default.
    #[default]
    DryRun,
    /// Delete, but only if the plan is exactly the one whose digest was confirmed (see
    /// [`PruneReport::digest`]). If snapshots were added or removed since, nothing is deleted and
    /// the prune fails with `Error::PlanChanged`.
    Confirmed(PlanDigest),
    /// Delete whatever the policy selects now, without anyone having looked at it. This is the
    /// non-interactive mode; the caller takes the responsibility.
    Unattended,
}

/// The steps of a prune, for progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrunePhase {
    /// Reading the snapshots to know what is in use.
    Reading,
    /// Deleting snapshot files.
    RemovingSnapshots,
    /// Deleting chunk objects.
    RemovingChunks,
}

/// Progress notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneEvent {
    /// A phase begins; it has `total` steps.
    Started {
        /// The phase.
        phase: PrunePhase,
        /// How many steps it will take.
        total: u64,
    },
    /// One step of the phase is done. In the two removing phases a step is one deletion.
    Step {
        /// The phase.
        phase: PrunePhase,
    },
}

/// Receives [`PruneEvent`]s. Any `Fn(&PruneEvent) + Send + Sync` closure is an observer.
pub trait PruneObserver: Send + Sync {
    /// Called for every event.
    fn event(&self, event: &PruneEvent);
}

impl<F> PruneObserver for F
where
    F: Fn(&PruneEvent) + Send + Sync,
{
    fn event(&self, event: &PruneEvent) {
        self(event);
    }
}

/// Everything that controls a prune.
#[derive(Clone)]
pub struct PruneOptions {
    /// Which snapshots to keep. Must be valid ([`RetentionPolicy::validate`]).
    pub policy: RetentionPolicy,
    /// Whether and on whose authority to delete.
    pub mode: PruneMode,
    /// Receives progress events.
    pub observer: Option<Arc<dyn PruneObserver>>,
    /// Asks the prune to stop before its next deletion; it then fails with
    /// `Error::PruneCancelled`, and the repository is consistent.
    pub cancel: CancelToken,
}

impl PruneOptions {
    /// A dry run of `policy`.
    pub fn new(policy: RetentionPolicy) -> Self {
        Self {
            policy,
            mode: PruneMode::DryRun,
            observer: None,
            cancel: CancelToken::new(),
        }
    }
}

impl fmt::Debug for PruneOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PruneOptions")
            .field("policy", &self.policy)
            .field("mode", &self.mode)
            .field("observer", &self.observer.is_some())
            .field("cancel", &self.cancel)
            .finish()
    }
}

/// An object that could not be deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneFailure {
    /// The repository-relative path of the object.
    pub subject: String,
    /// Why, in words.
    pub reason: String,
}

/// How many failures a report lists; the count is always exact.
pub const MAX_LISTED_FAILURES: usize = 100;

/// The plan of a prune and, if it was executed, what it did.
#[derive(Debug, Clone)]
pub struct PruneReport {
    /// False for a dry run: nothing below the "removed" counts happened.
    pub executed: bool,
    /// What the policy decides about every snapshot, oldest first.
    pub decisions: Vec<Decision>,
    /// Identifies this plan; pass it as [`PruneMode::Confirmed`] to execute exactly it.
    pub digest: PlanDigest,
    /// Distinct chunks that the kept snapshots need.
    pub chunks_in_use: u64,
    /// Chunk objects found in `data/` before the prune.
    pub objects: u64,
    /// Objects that no kept snapshot needs: to be deleted.
    pub chunks_unreferenced: u64,
    /// Their total size in bytes.
    pub bytes_unreferenced: u64,
    /// Snapshot files deleted.
    pub snapshots_removed: u64,
    /// Chunk objects deleted.
    pub chunks_removed: u64,
    /// Their total size in bytes.
    pub bytes_removed: u64,
    /// Chunk objects that could not be deleted (at most [`MAX_LISTED_FAILURES`] listed).
    pub failures: Vec<PruneFailure>,
    /// How many could not be deleted. They are harmless leftovers; the next prune tries again.
    pub failure_count: u64,
    /// After the sweep, how many of the chunks in use were confirmed to still exist.
    pub chunks_verified: u64,
    /// Wall-clock time taken.
    pub duration: Duration,
}

impl PruneReport {
    /// The snapshots that are kept.
    pub fn kept(&self) -> impl Iterator<Item = &Decision> {
        self.decisions.iter().filter(|decision| decision.is_kept())
    }

    /// The snapshots that are (or would be) removed.
    pub fn to_remove(&self) -> impl Iterator<Item = &Decision> {
        self.decisions.iter().filter(|decision| !decision.is_kept())
    }

    /// How many snapshots are (or would be) removed.
    pub fn snapshots_to_remove(&self) -> u64 {
        self.to_remove().count() as u64
    }

    /// True if the plan deletes nothing: no snapshot and no object.
    pub fn is_noop(&self) -> bool {
        self.snapshots_to_remove() == 0 && self.chunks_unreferenced == 0
    }

    /// True if everything that was to be deleted was.
    pub fn is_complete(&self) -> bool {
        self.failure_count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retention::KeepReason;
    use crate::scan::Timestamp;

    fn decision(pair: &str, kept: bool) -> Decision {
        Decision {
            id: pair.repeat(32).parse().expect("valid id"),
            created: Timestamp { secs: 0, nanos: 0 },
            keep: if kept {
                vec![KeepReason::Last]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn the_digest_names_who_is_kept_and_who_is_removed_whatever_the_order() {
        let plan = [
            decision("aa", true),
            decision("bb", false),
            decision("cc", true),
        ];
        let digest = PlanDigest::of(&plan);
        let shuffled = [
            decision("cc", true),
            decision("aa", true),
            decision("bb", false),
        ];
        assert_eq!(PlanDigest::of(&shuffled), digest);
        assert_eq!(digest.to_hex().len(), 64);
        assert_eq!(digest.to_string(), digest.to_hex());
        assert!(format!("{digest:?}").starts_with("PlanDigest("));

        // Any change of who is kept or removed, or of who exists, changes it.
        let flipped = [
            decision("aa", true),
            decision("bb", true),
            decision("cc", true),
        ];
        assert_ne!(PlanDigest::of(&flipped), digest);
        let extra = [
            decision("aa", true),
            decision("bb", false),
            decision("cc", true),
            decision("dd", true),
        ];
        assert_ne!(PlanDigest::of(&extra), digest);
        assert_ne!(PlanDigest::of(&plan[..2]), digest);
        assert_ne!(PlanDigest::of(&[]), digest);
    }

    #[test]
    fn a_digest_survives_a_round_trip_through_text_and_nothing_else_parses() {
        let digest = PlanDigest::of(&[decision("aa", true), decision("bb", false)]);
        assert_eq!(PlanDigest::from_hex(&digest.to_hex()), Some(digest));
        let bad = [
            String::new(),
            "abc".to_owned(),
            "0".repeat(63),
            "0".repeat(65),
            "g".repeat(64),
            "A".repeat(64),
        ];
        for text in bad {
            assert_eq!(PlanDigest::from_hex(&text), None, "{text:?}");
        }
    }

    #[test]
    fn the_reasons_for_keeping_do_not_matter_to_the_digest() {
        let mut other = decision("aa", true);
        other.keep = vec![KeepReason::Daily, KeepReason::Monthly];
        assert_eq!(
            PlanDigest::of(&[other]),
            PlanDigest::of(&[decision("aa", true)])
        );
    }

    #[test]
    fn the_default_mode_deletes_nothing_and_a_new_option_set_is_a_dry_run() {
        assert_eq!(PruneMode::default(), PruneMode::DryRun);
        let options = PruneOptions::new(RetentionPolicy::default());
        assert_eq!(options.mode, PruneMode::DryRun);
        assert!(options.observer.is_none() && !options.cancel.is_cancelled());
        assert!(format!("{options:?}").contains("PruneOptions"));
    }

    fn report(decisions: Vec<Decision>, unreferenced: u64, failures: u64) -> PruneReport {
        PruneReport {
            executed: false,
            digest: PlanDigest::of(&decisions),
            decisions,
            chunks_in_use: 0,
            objects: unreferenced,
            chunks_unreferenced: unreferenced,
            bytes_unreferenced: 0,
            snapshots_removed: 0,
            chunks_removed: 0,
            bytes_removed: 0,
            failures: Vec::new(),
            failure_count: failures,
            chunks_verified: 0,
            duration: Duration::ZERO,
        }
    }

    #[test]
    fn a_plan_is_a_no_op_only_when_it_removes_no_snapshot_and_no_object() {
        let kept_only = vec![decision("aa", true), decision("bb", true)];
        assert!(report(kept_only.clone(), 0, 0).is_noop());
        assert!(!report(kept_only, 3, 0).is_noop(), "garbage alone is work");
        let with_removal = vec![decision("aa", false), decision("bb", true)];
        let plan = report(with_removal, 0, 0);
        assert!(!plan.is_noop());
        assert_eq!(plan.snapshots_to_remove(), 1);
        assert_eq!(plan.kept().count(), 1);
        assert_eq!(
            plan.to_remove()
                .next()
                .map(|d| d.id.as_str()[..2].to_owned()),
            Some("aa".to_owned())
        );
        assert!(plan.is_complete());
        assert!(!report(Vec::new(), 0, 2).is_complete());
    }

    #[test]
    fn closures_are_observers() {
        let seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = Arc::clone(&seen);
        let observer: Arc<dyn PruneObserver> = Arc::new(move |event: &PruneEvent| {
            if matches!(event, PruneEvent::Step { .. }) {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        observer.event(&PruneEvent::Step {
            phase: PrunePhase::RemovingChunks,
        });
        observer.event(&PruneEvent::Started {
            phase: PrunePhase::Reading,
            total: 1,
        });
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
