use std::fmt;

use serde::{Serialize, Serializer};

/// Why a verdict was reached. One place owns both wordings, and the compiler
/// checks that every reason has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    CurrentHead,
    DefaultBranch,
    PrimaryWorktree,
    KeepConfigured,
    OpenPr,
    GoneMergedPr,
    MergedPr,
    ClosedPr,
    MergedPrNoLocalCommits,
    ClosedPrNoLocalCommits,
    MergeOnlyUnique,
    Merged,
    Stale,
    LeftoverEmptyRef,
    WorkAlreadyLanded,
    DiscardableUniqueWork,
    ThrowawayAbandonedNoise,
    UncertainLeftoverEmptyRef,
    UncertainWorkAlreadyLanded,
    UncertainThrowawayPath,
    ClosedPrLooksAbandoned,
    PrFinishedCommitsRemain,
    NoClearReasonToKeep,
    DeletableButUnpushed,
    FreshEmptyBranch,
    UniqueWorkWouldBeLost,
    StillActive,
    CommitsOnlyLocal,
    JevNoApiKey,
    JevOff,
    JevRequestFailed,
    /// A delete that became a question because the worktree is dirty.
    Dirty(Box<Reason>),
}

impl Reason {
    /// Full wording, as shown in `--json` and on the confirmation card.
    pub fn long(&self) -> String {
        match self {
            Reason::Dirty(inner) => {
                format!("{}; worktree has uncommitted changes", inner.long())
            }
            other => other.wording().0.to_string(),
        }
    }

    /// One word for the WHY column.
    pub fn short(&self) -> &'static str {
        match self {
            Reason::Dirty(_) => "dirty?",
            other => other.wording().1,
        }
    }

    fn wording(&self) -> (&'static str, &'static str) {
        match self {
            Reason::CurrentHead => ("current HEAD", "head"),
            Reason::DefaultBranch => ("default branch", "default"),
            Reason::PrimaryWorktree => ("primary worktree", "primary"),
            Reason::KeepConfigured => ("karu.keep", "config"),
            Reason::OpenPr => ("open PR", "open"),
            Reason::GoneMergedPr => ("gone merged PR", "shipped"),
            Reason::MergedPr => ("merged PR", "shipped"),
            Reason::ClosedPr => ("closed PR", "closed"),
            Reason::MergedPrNoLocalCommits => ("merged PR, no local commits", "shipped"),
            Reason::ClosedPrNoLocalCommits => ("closed PR, no local commits", "closed"),
            Reason::MergeOnlyUnique => ("merge-only unique", "junk"),
            Reason::Merged => ("merged", "merged"),
            Reason::Stale => ("idle for a long time", "stale"),
            Reason::LeftoverEmptyRef => ("leftover empty ref", "empty"),
            Reason::WorkAlreadyLanded => ("work already landed", "shipped"),
            Reason::DiscardableUniqueWork => ("discardable unique work", "junk"),
            Reason::ThrowawayAbandonedNoise => ("throwaway abandoned noise", "scratch"),
            Reason::UncertainLeftoverEmptyRef => ("uncertain leftover empty ref", "empty?"),
            Reason::UncertainWorkAlreadyLanded => {
                ("uncertain whether work already landed", "shipped?")
            }
            Reason::UncertainThrowawayPath => ("uncertain throwaway path", "scratch?"),
            Reason::ClosedPrLooksAbandoned => ("closed PR looks abandoned", "closed?"),
            Reason::PrFinishedCommitsRemain => ("PR finished, commits remain", "leftover"),
            Reason::NoClearReasonToKeep => ("no clear reason to keep", "unsure"),
            Reason::DeletableButUnpushed => {
                ("looks deletable, but commits are unpushed", "unpushed?")
            }
            Reason::FreshEmptyBranch => ("fresh empty branch", "fresh"),
            Reason::UniqueWorkWouldBeLost => ("unique work would be lost", "work"),
            Reason::StillActive => ("still active", "active"),
            Reason::CommitsOnlyLocal => ("commits exist only locally", "unpushed"),
            Reason::JevNoApiKey => ("jev skipped: no API key", "no-key"),
            Reason::JevOff => ("jev skipped: not enabled", "off"),
            Reason::JevRequestFailed => ("jev skipped: request failed", "no-jev"),
            Reason::Dirty(_) => unreachable!("Dirty builds its wording from the inner reason"),
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.long())
    }
}

impl Serialize for Reason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.long())
    }
}

/// Lets tests keep asserting on the wording users see.
#[cfg(test)]
impl PartialEq<&str> for Reason {
    fn eq(&self, other: &&str) -> bool {
        self.long() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wording users and `--json` consumers see, pinned on purpose: a
    /// change here is a change to the tool's output. `wording()` is checked
    /// for completeness by the compiler; this table is kept by hand.
    fn wording_table() -> Vec<(Reason, &'static str, &'static str)> {
        vec![
            (Reason::CurrentHead, "current HEAD", "head"),
            (Reason::DefaultBranch, "default branch", "default"),
            (Reason::PrimaryWorktree, "primary worktree", "primary"),
            (Reason::KeepConfigured, "karu.keep", "config"),
            (Reason::OpenPr, "open PR", "open"),
            (Reason::GoneMergedPr, "gone merged PR", "shipped"),
            (Reason::MergedPr, "merged PR", "shipped"),
            (Reason::ClosedPr, "closed PR", "closed"),
            (
                Reason::MergedPrNoLocalCommits,
                "merged PR, no local commits",
                "shipped",
            ),
            (
                Reason::ClosedPrNoLocalCommits,
                "closed PR, no local commits",
                "closed",
            ),
            (Reason::MergeOnlyUnique, "merge-only unique", "junk"),
            (Reason::Merged, "merged", "merged"),
            (Reason::Stale, "idle for a long time", "stale"),
            (Reason::LeftoverEmptyRef, "leftover empty ref", "empty"),
            (Reason::WorkAlreadyLanded, "work already landed", "shipped"),
            (
                Reason::DiscardableUniqueWork,
                "discardable unique work",
                "junk",
            ),
            (
                Reason::ThrowawayAbandonedNoise,
                "throwaway abandoned noise",
                "scratch",
            ),
            (
                Reason::UncertainLeftoverEmptyRef,
                "uncertain leftover empty ref",
                "empty?",
            ),
            (
                Reason::UncertainWorkAlreadyLanded,
                "uncertain whether work already landed",
                "shipped?",
            ),
            (
                Reason::UncertainThrowawayPath,
                "uncertain throwaway path",
                "scratch?",
            ),
            (
                Reason::ClosedPrLooksAbandoned,
                "closed PR looks abandoned",
                "closed?",
            ),
            (
                Reason::PrFinishedCommitsRemain,
                "PR finished, commits remain",
                "leftover",
            ),
            (
                Reason::NoClearReasonToKeep,
                "no clear reason to keep",
                "unsure",
            ),
            (
                Reason::DeletableButUnpushed,
                "looks deletable, but commits are unpushed",
                "unpushed?",
            ),
            (Reason::FreshEmptyBranch, "fresh empty branch", "fresh"),
            (
                Reason::UniqueWorkWouldBeLost,
                "unique work would be lost",
                "work",
            ),
            (Reason::StillActive, "still active", "active"),
            (
                Reason::CommitsOnlyLocal,
                "commits exist only locally",
                "unpushed",
            ),
            (Reason::JevNoApiKey, "jev skipped: no API key", "no-key"),
            (Reason::JevOff, "jev skipped: not enabled", "off"),
            (
                Reason::JevRequestFailed,
                "jev skipped: request failed",
                "no-jev",
            ),
        ]
    }

    #[test]
    fn every_reason_keeps_its_wording() {
        for (reason, long, short) in wording_table() {
            assert_eq!(reason.long(), long, "{reason:?}");
            assert_eq!(reason.short(), short, "{reason:?}");
        }
    }

    #[test]
    fn long_wording_identifies_a_reason() {
        let mut seen = std::collections::HashSet::new();
        for (reason, long, _) in wording_table() {
            assert!(seen.insert(long), "duplicate wording: {reason:?}");
        }
    }

    #[test]
    fn short_label_never_repeats_the_verdict() {
        for (reason, _, short) in wording_table() {
            for verdict in ["keep", "delete", "ask"] {
                assert_ne!(short, verdict, "{reason:?}");
            }
        }
    }

    #[test]
    fn dirty_wraps_the_reason_it_overrode() {
        let reason = Reason::Dirty(Box::new(Reason::MergedPr));
        assert_eq!(reason.long(), "merged PR; worktree has uncommitted changes");
        assert_eq!(reason.short(), "dirty?");
        assert_eq!(reason, "merged PR; worktree has uncommitted changes");
    }

    #[test]
    fn serializes_as_the_long_wording() {
        assert_eq!(
            serde_json::to_value(Reason::Stale).unwrap(),
            serde_json::json!("idle for a long time")
        );
        assert_eq!(Reason::Stale.to_string(), "idle for a long time");
    }
}
