use crate::candidate::{Candidate, Nouls, Source, Verdict, VerdictKind};
use crate::reason::Reason;

pub const LANDED_DELETE: f64 = 0.85;
pub const THROWAWAY_DELETE: f64 = 0.8;
pub const ABANDONED_DELETE: f64 = 0.7;
pub const NOISE_DELETE: f64 = 0.7;
pub const ASK_LOW: f64 = 0.4;
pub const ASK_HIGH: f64 = 0.6;

/// Everything that can be decided without asking Jev.
pub fn without_jev(candidate: &Candidate, force: bool, stale_after_secs: u64) -> Option<Verdict> {
    hard_verdict(candidate, force).or_else(|| stale_verdict(candidate, stale_after_secs))
}

/// Jev's answer as a verdict, behind the same guard as the rules.
pub fn with_jev(candidate: &Candidate, nouls: Nouls, force: bool) -> Verdict {
    guard_dirty(candidate, compose_jev_for(candidate, nouls), force)
}

/// Whether `git status` on the worktree could change the outcome. The keeps
/// below hold either way, which lets the collector skip that (slow) check.
pub fn dirty_can_change_verdict(candidate: &Candidate) -> bool {
    candidate.worktree_path.is_some() && unconditional_keep(candidate).is_none()
}

fn hard_verdict(candidate: &Candidate, force: bool) -> Option<Verdict> {
    hard_rules(candidate).map(|verdict| guard_dirty(candidate, verdict, force))
}

/// Uncommitted changes cannot be recovered from anywhere, so a dirty worktree
/// is never deleted on a rule's word alone: the delete becomes a question.
/// `--force` opts out.
fn guard_dirty(candidate: &Candidate, verdict: Verdict, force: bool) -> Verdict {
    if force || !candidate.worktree_dirty || verdict.kind != VerdictKind::Delete {
        return verdict;
    }
    Verdict {
        kind: VerdictKind::Ask,
        source: verdict.source,
        reason: Reason::Dirty(Box::new(verdict.reason)),
        score: None,
    }
}

fn unconditional_keep(candidate: &Candidate) -> Option<Verdict> {
    if candidate.is_head {
        return Some(keep(Reason::CurrentHead));
    }
    if candidate.is_default {
        return Some(keep(Reason::DefaultBranch));
    }
    if candidate.is_primary_worktree {
        return Some(keep(Reason::PrimaryWorktree));
    }
    if candidate.keep_configured {
        return Some(keep(Reason::KeepConfigured));
    }
    if candidate.has_open_pr() {
        return Some(keep(Reason::OpenPr));
    }
    None
}

fn hard_rules(candidate: &Candidate) -> Option<Verdict> {
    if let Some(verdict) = unconditional_keep(candidate) {
        return Some(verdict);
    }

    if candidate.gone_with_merged_pr() {
        return Some(delete(Reason::GoneMergedPr));
    }

    if candidate.is_at_merged_pr_head() {
        return Some(delete(Reason::MergedPr));
    }

    if candidate.is_at_closed_pr_head() {
        return Some(delete(Reason::ClosedPr));
    }

    if candidate.is_finished_pr_without_local_commits() {
        return Some(delete(if candidate.has_closed_pr() {
            Reason::ClosedPrNoLocalCommits
        } else {
            Reason::MergedPrNoLocalCommits
        }));
    }

    if candidate.unique_commits_are_merges_only() {
        return Some(delete(Reason::MergeOnlyUnique));
    }

    if candidate.merged && candidate.diverged {
        return Some(delete(Reason::Merged));
    }

    None
}

pub const STALE_AFTER_SECS: u64 = 7 * 24 * 60 * 60;

/// Runs after `hard_verdict` found nothing, so open PRs, dirty worktrees and
/// the other keeps are already out. What is left and untouched for this long
/// is worth a question rather than a silent keep.
fn stale_verdict(candidate: &Candidate, stale_after_secs: u64) -> Option<Verdict> {
    let idle = candidate.ref_age_secs?;
    (idle >= stale_after_secs).then_some(ask(Reason::Stale))
}

/// Jev's scores as a delete or a question; `None` when they support neither.
fn compose_jev(nouls: Nouls) -> Option<Verdict> {
    if nouls.empty_ref_is_leftover >= LANDED_DELETE {
        return Some(jev_delete(Reason::LeftoverEmptyRef));
    }

    if nouls.work_already_landed >= LANDED_DELETE {
        return Some(jev_delete(Reason::WorkAlreadyLanded));
    }

    if nouls.unique_commits_are_noise >= LANDED_DELETE && nouls.looks_abandoned >= ABANDONED_DELETE
    {
        return Some(jev_delete(Reason::DiscardableUniqueWork));
    }

    if nouls.name_is_throwaway >= THROWAWAY_DELETE
        && nouls.looks_abandoned >= ABANDONED_DELETE
        && nouls.unique_commits_are_noise >= NOISE_DELETE
    {
        return Some(jev_delete(Reason::ThrowawayAbandonedNoise));
    }

    if probably(nouls.empty_ref_is_leftover) {
        return Some(jev_ask(
            Reason::UncertainLeftoverEmptyRef,
            nouls.empty_ref_is_leftover,
        ));
    }

    if probably(nouls.work_already_landed) {
        return Some(jev_ask(
            Reason::UncertainWorkAlreadyLanded,
            nouls.work_already_landed,
        ));
    }

    if throwaway_path_is_ask(nouls) {
        return Some(jev_ask(
            Reason::UncertainThrowawayPath,
            throwaway_ask_score(nouls),
        ));
    }

    None
}

fn compose_jev_for(candidate: &Candidate, nouls: Nouls) -> Verdict {
    let Some(verdict) = compose_jev(nouls) else {
        if candidate.has_closed_pr() && nouls.looks_abandoned >= ABANDONED_DELETE {
            return jev_ask(Reason::ClosedPrLooksAbandoned, nouls.looks_abandoned);
        }
        return doubtful_keep(candidate, nouls);
    };
    // Jev is a guess; commits no remote holds cannot be recovered from one.
    if verdict.kind == VerdictKind::Delete && candidate.local_only_commit_count.unwrap_or(0) > 0 {
        return jev_ask_unscored(Reason::DeletableButUnpushed);
    }
    verdict
}

/// Jev found no reason to delete. Keep only on a positive sign that the branch
/// is in use; anything else is worth a question.
fn doubtful_keep(candidate: &Candidate, nouls: Nouls) -> Verdict {
    if candidate.unique_commit_count == Some(0) {
        return jev_keep(Reason::FreshEmptyBranch);
    }
    if candidate.pr.is_some() && !candidate.has_open_pr() {
        return jev_ask_unscored(Reason::PrFinishedCommitsRemain);
    }
    let in_use = if nouls.unique_commits_are_noise <= ASK_LOW {
        Some(Reason::UniqueWorkWouldBeLost)
    } else if nouls.unique_commits_are_noise >= NOISE_DELETE && nouls.looks_abandoned <= ASK_LOW {
        Some(Reason::StillActive)
    } else {
        None
    };
    match in_use {
        Some(_) if candidate.local_only_commit_count.unwrap_or(0) > 0 => {
            jev_keep(Reason::CommitsOnlyLocal)
        }
        Some(reason) => jev_keep(reason),
        None => jev_ask_unscored(Reason::NoClearReasonToKeep),
    }
}

/// Jev was never asked: no key, or it is not turned on for this repo.
pub fn jev_unavailable(reason: Reason) -> Verdict {
    jev_keep(reason)
}

pub fn jev_failed() -> Verdict {
    jev_keep(Reason::JevRequestFailed)
}

fn throwaway_path_is_ask(nouls: Nouls) -> bool {
    let name = gate(nouls.name_is_throwaway, THROWAWAY_DELETE);
    let abandoned = gate(nouls.looks_abandoned, ABANDONED_DELETE);
    let noise = gate(nouls.unique_commits_are_noise, NOISE_DELETE);
    let any_ask =
        matches!(name, Gate::Ask) || matches!(abandoned, Gate::Ask) || matches!(noise, Gate::Ask);
    any_ask
        && !matches!(name, Gate::No)
        && !matches!(abandoned, Gate::No)
        && !matches!(noise, Gate::No)
}

fn throwaway_ask_score(nouls: Nouls) -> f64 {
    [
        nouls.name_is_throwaway,
        nouls.looks_abandoned,
        nouls.unique_commits_are_noise,
    ]
    .into_iter()
    .filter(|value| in_ask_band(*value))
    .min_by(|a, b| {
        (a - 0.5)
            .abs()
            .partial_cmp(&(b - 0.5).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    })
    .unwrap_or(0.5)
}

#[derive(Clone, Copy)]
enum Gate {
    Yes,
    Ask,
    No,
}

fn gate(value: f64, threshold: f64) -> Gate {
    if value >= threshold {
        Gate::Yes
    } else if in_ask_band(value) {
        Gate::Ask
    } else {
        Gate::No
    }
}

fn in_ask_band(value: f64) -> bool {
    value > ASK_LOW && value < ASK_HIGH
}

/// For a question that deletes on its own at `LANDED_DELETE`: anything from
/// "maybe" up to just short of that is worth asking. Leaving 0.6 to 0.85 to
/// fall through kept branches Jev was 82% sure were leftovers.
fn probably(value: f64) -> bool {
    value > ASK_LOW && value < LANDED_DELETE
}

fn keep(reason: Reason) -> Verdict {
    Verdict {
        kind: VerdictKind::Keep,
        source: Source::Hard,
        reason,
        score: None,
    }
}

fn ask(reason: Reason) -> Verdict {
    Verdict {
        kind: VerdictKind::Ask,
        source: Source::Hard,
        reason,
        score: None,
    }
}

fn delete(reason: Reason) -> Verdict {
    Verdict {
        kind: VerdictKind::Delete,
        source: Source::Hard,
        reason,
        score: None,
    }
}

fn jev_delete(reason: Reason) -> Verdict {
    Verdict {
        kind: VerdictKind::Delete,
        source: Source::Jev,
        reason,
        score: None,
    }
}

fn jev_ask_unscored(reason: Reason) -> Verdict {
    Verdict {
        kind: VerdictKind::Ask,
        source: Source::Jev,
        reason,
        score: None,
    }
}

fn jev_ask(reason: Reason, score: f64) -> Verdict {
    Verdict {
        kind: VerdictKind::Ask,
        source: Source::Jev,
        reason,
        score: Some(score),
    }
}

fn jev_keep(reason: Reason) -> Verdict {
    Verdict {
        kind: VerdictKind::Keep,
        source: Source::Jev,
        reason,
        score: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> Candidate {
        crate::candidate::fixtures::candidate()
    }

    #[test]
    fn head_is_hard_keep() {
        let mut c = candidate();
        c.is_head = true;
        c.merged = true;
        c.diverged = true;
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.kind, VerdictKind::Keep);
        assert_eq!(v.reason, "current HEAD");
    }

    #[test]
    fn default_branch_is_hard_keep() {
        let mut c = candidate();
        c.is_default = true;
        c.merged = true;
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.reason, "default branch");
    }

    #[test]
    fn configured_keep_wins_over_merged() {
        let mut c = candidate();
        c.keep_configured = true;
        c.merged = true;
        c.diverged = true;
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.reason, "karu.keep");
        assert_eq!(v.kind, VerdictKind::Keep);
    }

    #[test]
    fn dirty_worktree_is_asked_without_force() {
        let mut c = candidate();
        c.worktree_dirty = true;
        c.merged = true;
        c.diverged = true;
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.kind, VerdictKind::Ask);
        assert_eq!(v.reason, "merged; worktree has uncommitted changes");
        assert!(hard_verdict(&c, true).unwrap().kind == VerdictKind::Delete);
    }

    #[test]
    fn merged_and_diverged_is_hard_delete() {
        let mut c = candidate();
        c.merged = true;
        c.diverged = true;
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.source, Source::Hard);
        assert_eq!(v.reason, "merged");
    }

    #[test]
    fn unchanged_fresh_goes_to_jev() {
        let mut c = candidate();
        c.merged = true;
        c.diverged = false;
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn unchanged_with_age_goes_to_jev() {
        let mut c = candidate();
        c.diverged = false;
        c.ref_age_secs = Some(3 * 24 * 60 * 60);
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn unchanged_stale_still_keeps_head() {
        let mut c = candidate();
        c.diverged = false;
        c.is_head = true;
        c.ref_age_secs = Some(10 * 24 * 60 * 60);
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.reason, "current HEAD");
    }

    #[test]
    fn many_unique_commits_go_to_jev() {
        let mut c = candidate();
        c.unique_commit_count = Some(40);
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn many_unique_commits_still_hard_delete_when_merged() {
        let mut c = candidate();
        c.unique_commit_count = Some(40);
        c.merged = true;
        c.diverged = true;
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
    }

    #[test]
    fn detached_without_unique_commits_goes_to_jev() {
        let mut c = candidate();
        c.branch = None;
        c.worktree_path = Some("/tmp/wt".into());
        c.unique_commit_count = Some(0);
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn leftover_unmerged_goes_to_jev() {
        assert!(hard_verdict(&candidate(), false).is_none());
    }

    #[test]
    fn merge_only_unique_is_hard_delete() {
        let mut c = candidate();
        c.unique_commit_count = Some(2);
        c.non_merge_unique_count = Some(0);
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.reason, "merge-only unique");
    }

    #[test]
    fn a_subject_starting_with_merge_is_not_a_merge_commit() {
        let mut c = candidate();
        c.unique_commit_count = Some(1);
        c.non_merge_unique_count = Some(1);
        c.unique_subjects = vec!["Merge sort implementation".into()];
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn mix_of_merge_and_feature_goes_to_jev() {
        let mut c = candidate();
        c.unique_commit_count = Some(2);
        c.non_merge_unique_count = Some(1);
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn unknown_merge_count_is_not_hard_delete() {
        let mut c = candidate();
        c.unique_commit_count = Some(2);
        c.non_merge_unique_count = None;
        c.unique_subjects = vec!["Merge branch 'main'".into(), "Merge branch 'main'".into()];
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn gone_with_merged_pr_is_hard_delete() {
        let mut c = candidate();
        c.upstream_gone = true;
        c.unique_commit_count = Some(2);
        c.unique_subjects = vec!["fix: group permission".into()];
        c.pr = Some(crate::candidate::PrInfo {
            state: crate::candidate::PrState::Merged,
            title: "group permission".into(),
            merged_at: Some("2026-09-10".into()),
            head_sha: None,
            number: None,
        });
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.reason, "gone merged PR");
    }

    #[test]
    fn gone_without_pr_goes_to_jev() {
        let mut c = candidate();
        c.upstream_gone = true;
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn dirty_gone_merged_pr_is_asked() {
        let mut c = candidate();
        c.worktree_dirty = true;
        c.upstream_gone = true;
        c.pr = Some(crate::candidate::PrInfo {
            state: crate::candidate::PrState::Merged,
            title: "x".into(),
            merged_at: None,
            head_sha: None,
            number: None,
        });
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.kind, VerdictKind::Ask);
        assert!(
            v.reason.long().ends_with("uncommitted changes"),
            "{}",
            v.reason
        );
    }

    #[test]
    fn landed_noul_deletes() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.85,
            name_is_throwaway: 0.1,
            looks_abandoned: 0.1,
            unique_commits_are_noise: 0.1,
            empty_ref_is_leftover: 0.0,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.source, Source::Jev);
        assert_eq!(v.reason, "work already landed");
    }

    #[test]
    fn throwaway_path_deletes() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.8,
            looks_abandoned: 0.7,
            unique_commits_are_noise: 0.7,
            empty_ref_is_leftover: 0.0,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.reason, "throwaway abandoned noise");
    }

    #[test]
    fn throwaway_path_below_threshold_keeps() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.8,
            looks_abandoned: 0.69,
            unique_commits_are_noise: 0.7,
            empty_ref_is_leftover: 0.0,
        });
        assert!(v.is_none());
    }

    #[test]
    fn landed_ask_band_asks() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.5,
            name_is_throwaway: 0.1,
            looks_abandoned: 0.1,
            unique_commits_are_noise: 0.1,
            empty_ref_is_leftover: 0.0,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Ask);
        assert_eq!(v.reason, "uncertain whether work already landed");
        assert_eq!(v.score, Some(0.5));
    }

    #[test]
    fn throwaway_ask_band_asks() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.5,
            looks_abandoned: 0.75,
            unique_commits_are_noise: 0.75,
            empty_ref_is_leftover: 0.0,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Ask);
        assert_eq!(v.reason, "uncertain throwaway path");
        assert_eq!(v.score, Some(0.5));
    }

    fn landed(score: f64) -> Option<Verdict> {
        compose_jev(Nouls {
            work_already_landed: score,
            name_is_throwaway: 0.1,
            looks_abandoned: 0.1,
            unique_commits_are_noise: 0.1,
            empty_ref_is_leftover: 0.0,
        })
    }

    fn leftover(score: f64) -> Option<Verdict> {
        compose_jev(Nouls {
            work_already_landed: 0.0,
            name_is_throwaway: 0.1,
            looks_abandoned: 0.1,
            unique_commits_are_noise: 0.1,
            empty_ref_is_leftover: score,
        })
    }

    /// "Probably yes" is a question, never a silent keep: measured against
    /// the real Jev, an old empty `pr-1234` scored 0.82 and was kept as fresh.
    #[test]
    fn a_likely_but_not_certain_delete_is_asked() {
        for ask in [landed, leftover] {
            assert!(ask(ASK_LOW).is_none(), "the lower edge is still a no");
            for score in [0.41, 0.6, 0.82, 0.849] {
                let v = ask(score).unwrap_or_else(|| panic!("{score} should be asked"));
                assert_eq!(v.kind, VerdictKind::Ask, "{score}");
                assert_eq!(v.score, Some(score));
            }
            assert_eq!(ask(LANDED_DELETE).unwrap().kind, VerdictKind::Delete);
        }
    }

    /// The throwaway path needs three agreeing scores, so its band stays narrow.
    #[test]
    fn throwaway_band_is_unchanged() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.7,
            looks_abandoned: 0.9,
            unique_commits_are_noise: 0.75,
            empty_ref_is_leftover: 0.1,
        });
        assert!(
            v.is_none(),
            "0.7 is between the ask band and the delete threshold"
        );
    }

    #[test]
    fn missing_api_key_keeps() {
        let v = jev_unavailable(Reason::JevNoApiKey);
        assert_eq!(v.kind, VerdictKind::Keep);
        assert_eq!(v.source, Source::Jev);
    }

    #[test]
    fn jev_request_failure_keeps() {
        let v = jev_failed();
        assert_eq!(v.kind, VerdictKind::Keep);
        assert_eq!(v.reason, "jev skipped: request failed");
    }

    #[test]
    fn unique_commit_count_at_threshold_goes_to_jev() {
        let mut c = candidate();
        c.unique_commit_count = Some(15);
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn unique_commit_count_none_goes_to_jev() {
        let mut c = candidate();
        c.unique_commit_count = None;
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn unique_commit_count_zero_on_named_branch_goes_to_jev() {
        let mut c = candidate();
        c.unique_commit_count = Some(0);
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn primary_worktree_is_hard_keep_even_when_merged() {
        let mut c = candidate();
        c.is_primary_worktree = true;
        c.merged = true;
        c.diverged = true;
        let v = hard_verdict(&c, true).unwrap();
        assert_eq!(v.kind, VerdictKind::Keep);
        assert_eq!(v.reason, "primary worktree");
    }

    #[test]
    fn detached_dirty_worktree_has_no_hard_verdict() {
        let mut c = candidate();
        c.branch = None;
        c.worktree_path = Some("/tmp/wt".into());
        c.unique_commit_count = Some(0);
        c.worktree_dirty = true;
        assert!(hard_verdict(&c, false).is_none());
        assert!(hard_verdict(&c, true).is_none());
    }

    #[test]
    fn detached_with_unique_commits_goes_to_jev() {
        let mut c = candidate();
        c.branch = None;
        c.worktree_path = Some("/tmp/wt".into());
        c.unique_commit_count = Some(1);
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn throwaway_yes_yes_ask_is_ask() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.9,
            looks_abandoned: 0.9,
            unique_commits_are_noise: 0.5,
            empty_ref_is_leftover: 0.0,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Ask);
        assert_eq!(v.reason, "uncertain throwaway path");
    }

    #[test]
    fn throwaway_any_no_is_keep() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.9,
            looks_abandoned: 0.1,
            unique_commits_are_noise: 0.9,
            empty_ref_is_leftover: 0.0,
        });
        assert!(v.is_none());
    }

    #[test]
    fn throwaway_all_ask_is_ask() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.5,
            looks_abandoned: 0.5,
            unique_commits_are_noise: 0.5,
            empty_ref_is_leftover: 0.0,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Ask);
    }

    #[test]
    fn nan_noul_is_keep() {
        let v = compose_jev(Nouls {
            work_already_landed: f64::NAN,
            name_is_throwaway: f64::NAN,
            looks_abandoned: f64::NAN,
            unique_commits_are_noise: f64::NAN,
            empty_ref_is_leftover: 0.0,
        });
        assert!(v.is_none());
    }

    #[test]
    fn negative_noul_is_keep() {
        let v = compose_jev(Nouls {
            work_already_landed: -1.0,
            name_is_throwaway: -0.1,
            looks_abandoned: -0.1,
            unique_commits_are_noise: -0.1,
            empty_ref_is_leftover: 0.0,
        });
        assert!(v.is_none());
    }

    #[test]
    fn throwaway_delete_wins_over_landed_ask() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.5,
            name_is_throwaway: 0.8,
            looks_abandoned: 0.7,
            unique_commits_are_noise: 0.7,
            empty_ref_is_leftover: 0.0,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.reason, "throwaway abandoned noise");
    }

    #[test]
    fn landed_exact_threshold_deletes() {
        let v = compose_jev(Nouls {
            work_already_landed: LANDED_DELETE,
            name_is_throwaway: 0.0,
            looks_abandoned: 0.0,
            unique_commits_are_noise: 0.0,
            empty_ref_is_leftover: 0.0,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.reason, "work already landed");
    }

    #[test]
    fn leftover_empty_ref_deletes() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.1,
            looks_abandoned: 0.1,
            unique_commits_are_noise: 0.1,
            empty_ref_is_leftover: 0.85,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.reason, "leftover empty ref");
    }

    #[test]
    fn discardable_unique_work_deletes_without_throwaway_name() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.1,
            looks_abandoned: 0.7,
            unique_commits_are_noise: 0.85,
            empty_ref_is_leftover: 0.0,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.reason, "discardable unique work");
    }

    #[test]
    fn leftover_empty_ref_ask_band_asks() {
        let v = compose_jev(Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.1,
            looks_abandoned: 0.1,
            unique_commits_are_noise: 0.1,
            empty_ref_is_leftover: 0.5,
        })
        .unwrap();
        assert_eq!(v.kind, VerdictKind::Ask);
        assert_eq!(v.reason, "uncertain leftover empty ref");
        assert_eq!(v.score, Some(0.5));
    }

    fn real_work_nouls(looks_abandoned: f64) -> Nouls {
        Nouls {
            work_already_landed: 0.1,
            name_is_throwaway: 0.1,
            looks_abandoned,
            unique_commits_are_noise: 0.1,
            empty_ref_is_leftover: 0.1,
        }
    }

    fn with_pr(state: &str) -> Candidate {
        let mut c = candidate();
        c.pr = Some(crate::candidate::PrInfo {
            state: crate::candidate::PrState::parse(state).expect("a PR state"),
            title: "feat".into(),
            merged_at: None,
            head_sha: None,
            number: None,
        });
        c
    }

    #[test]
    fn closed_pr_that_looks_abandoned_asks() {
        let v = compose_jev_for(&with_pr("CLOSED"), real_work_nouls(0.9));
        assert_eq!(v.kind, VerdictKind::Ask);
        assert_eq!(v.reason, "closed PR looks abandoned");
        assert_eq!(v.score, Some(0.9));
    }

    #[test]
    fn finished_pr_with_commits_left_is_asked_even_when_jev_would_keep() {
        for state in ["CLOSED", "MERGED"] {
            let v = compose_jev_for(&with_pr(state), real_work_nouls(0.3));
            assert_eq!(v.kind, VerdictKind::Ask, "{state}");
            assert_eq!(v.reason, "PR finished, commits remain");
            assert_eq!(v.score, None);
        }
    }

    #[test]
    fn signs_of_use_are_still_kept() {
        let mut c = candidate();
        c.unique_commit_count = Some(3);
        let mut active = real_work_nouls(0.1);
        active.unique_commits_are_noise = 0.9;
        for nouls in [real_work_nouls(0.9), active] {
            assert_eq!(compose_jev_for(&c, nouls).kind, VerdictKind::Keep);
        }
        c.unique_commit_count = Some(0);
        assert_eq!(
            compose_jev_for(&c, real_work_nouls(0.1)).kind,
            VerdictKind::Keep
        );
    }

    #[test]
    fn open_pr_that_looks_abandoned_is_kept() {
        let v = compose_jev_for(&with_pr("OPEN"), real_work_nouls(0.9));
        assert_eq!(v.kind, VerdictKind::Keep);
    }

    #[test]
    fn closed_pr_does_not_weaken_delete() {
        let mut nouls = real_work_nouls(0.9);
        nouls.work_already_landed = 0.95;
        let v = compose_jev_for(&with_pr("CLOSED"), nouls);
        assert_eq!(v.kind, VerdictKind::Delete);
    }

    fn keep_reason(unique: usize, nouls: Nouls) -> String {
        let mut c = candidate();
        c.unique_commit_count = Some(unique);
        let v = compose_jev_for(&c, nouls);
        assert_eq!(v.kind, VerdictKind::Keep);
        assert_eq!(v.source, Source::Jev);
        v.reason.long()
    }

    #[test]
    fn kept_empty_branch_is_fresh() {
        assert_eq!(keep_reason(0, real_work_nouls(0.1)), "fresh empty branch");
    }

    #[test]
    fn kept_real_commits_are_work() {
        assert_eq!(
            keep_reason(3, real_work_nouls(0.9)),
            "unique work would be lost"
        );
    }

    #[test]
    fn kept_noise_that_is_not_abandoned_is_active() {
        let mut nouls = real_work_nouls(0.1);
        nouls.unique_commits_are_noise = 0.9;
        assert_eq!(keep_reason(3, nouls), "still active");
    }

    #[test]
    fn no_clear_reason_to_keep_is_asked() {
        let mut nouls = real_work_nouls(0.65);
        nouls.unique_commits_are_noise = 0.65;
        let mut c = candidate();
        c.unique_commit_count = Some(3);
        for local_only in [Some(0), Some(3)] {
            c.local_only_commit_count = local_only;
            let v = compose_jev_for(&c, nouls);
            assert_eq!(v.kind, VerdictKind::Ask);
            assert_eq!(v.source, Source::Jev);
            assert_eq!(v.reason, "no clear reason to keep");
            assert_eq!(v.score, None);
        }
    }

    fn with_pr_at(state: &str, pr_head: &str, tip: &str) -> Candidate {
        let mut c = with_pr(state);
        c.pr.as_mut().unwrap().head_sha = Some(pr_head.into());
        c.sha = Some(tip.into());
        c.diverged = true;
        c.unique_commit_count = Some(2);
        c
    }

    #[test]
    fn merged_pr_at_the_same_commit_is_hard_delete() {
        let v = hard_verdict(&with_pr_at("MERGED", "abc123", "abc123"), false).unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.source, Source::Hard);
        assert_eq!(v.reason, "merged PR");
    }

    #[test]
    fn merged_pr_with_later_local_commits_goes_to_jev() {
        assert!(hard_verdict(&with_pr_at("MERGED", "abc123", "def456"), false).is_none());
    }

    #[test]
    fn open_pr_is_hard_keep() {
        for tip in ["abc123", "def456"] {
            let v = hard_verdict(&with_pr_at("OPEN", "abc123", tip), false).unwrap();
            assert_eq!(v.kind, VerdictKind::Keep);
            assert_eq!(v.reason, "open PR");
        }
    }

    #[test]
    fn dirty_worktree_turns_merged_pr_delete_into_ask() {
        let mut c = with_pr_at("MERGED", "abc123", "abc123");
        c.worktree_dirty = true;
        let v = hard_verdict(&c, false).unwrap();
        assert_eq!(v.kind, VerdictKind::Ask);
        assert_eq!(v.reason, "merged PR; worktree has uncommitted changes");
    }

    /// The dirty check is skipped exactly where it cannot matter, so the
    /// verdict must not depend on `worktree_dirty` there.
    #[test]
    fn skipping_the_dirty_check_never_changes_a_verdict() {
        let mut skipped = 0;
        for bits in 0..32u32 {
            let mut c = with_pr_at(
                if bits & 16 != 0 { "OPEN" } else { "MERGED" },
                "abc123",
                "abc123",
            );
            c.worktree_path = Some("/tmp/wt".into());
            c.is_head = bits & 1 != 0;
            c.is_default = bits & 2 != 0;
            c.is_primary_worktree = bits & 4 != 0;
            c.keep_configured = bits & 8 != 0;
            if dirty_can_change_verdict(&c) {
                continue;
            }
            skipped += 1;
            let clean = hard_verdict(&c, false).map(|v| v.kind);
            c.worktree_dirty = true;
            let dirty = hard_verdict(&c, false).map(|v| v.kind);
            assert_eq!(clean, dirty, "bits={bits:05b}");
            assert_eq!(clean, Some(VerdictKind::Keep), "bits={bits:05b}");
        }
        assert_eq!(skipped, 31);
    }

    #[test]
    fn dirty_check_is_needed_for_a_deletable_worktree() {
        let mut c = with_pr_at("MERGED", "abc123", "abc123");
        c.worktree_path = Some("/tmp/wt".into());
        assert!(dirty_can_change_verdict(&c));
        c.pr = None;
        assert!(dirty_can_change_verdict(&c));
    }

    #[test]
    fn dirty_check_is_pointless_without_a_worktree() {
        let c = with_pr_at("MERGED", "abc123", "abc123");
        assert!(!dirty_can_change_verdict(&c));
    }

    #[test]
    fn closed_pr_at_the_same_commit_is_hard_delete() {
        let v = hard_verdict(&with_pr_at("CLOSED", "abc123", "abc123"), false).unwrap();
        assert_eq!(v.kind, VerdictKind::Delete);
        assert_eq!(v.source, Source::Hard);
        assert_eq!(v.reason, "closed PR");
        assert_eq!(v.score, None);
    }

    #[test]
    fn closed_pr_with_other_local_commits_goes_to_jev() {
        assert!(hard_verdict(&with_pr_at("CLOSED", "abc123", "def456"), false).is_none());
    }

    #[test]
    fn dirty_worktree_turns_closed_pr_delete_into_ask() {
        let mut c = with_pr_at("CLOSED", "abc123", "abc123");
        c.worktree_dirty = true;
        assert_eq!(hard_verdict(&c, false).unwrap().kind, VerdictKind::Ask);
    }

    #[test]
    fn kept_commits_that_exist_only_locally_say_so() {
        let mut c = candidate();
        c.unique_commit_count = Some(2);
        c.local_only_commit_count = Some(2);
        for nouls in [real_work_nouls(0.9), real_work_nouls(0.65)] {
            let v = compose_jev_for(&c, nouls);
            assert_eq!(v.kind, VerdictKind::Keep);
            assert_eq!(v.reason, "commits exist only locally");
        }
    }

    #[test]
    fn pushed_commits_keep_their_usual_reason() {
        let mut c = candidate();
        c.unique_commit_count = Some(2);
        c.local_only_commit_count = Some(0);
        let v = compose_jev_for(&c, real_work_nouls(0.9));
        assert_eq!(v.reason, "unique work would be lost");
    }

    const WEEK: u64 = 7 * 24 * 60 * 60;

    fn idle_for(secs: Option<u64>) -> Candidate {
        let mut c = candidate();
        c.diverged = true;
        c.unique_commit_count = Some(3);
        c.ref_age_secs = secs;
        c
    }

    #[test]
    fn long_idle_branch_without_an_open_pr_is_asked() {
        let v = stale_verdict(&idle_for(Some(WEEK)), WEEK).unwrap();
        assert_eq!(v.kind, VerdictKind::Ask);
        assert_eq!(v.source, Source::Hard);
        assert_eq!(v.reason, "idle for a long time");
        assert_eq!(v.score, None);
    }

    #[test]
    fn recently_touched_branch_is_not_stale() {
        assert!(stale_verdict(&idle_for(Some(WEEK - 1)), WEEK).is_none());
    }

    #[test]
    fn unknown_idle_time_is_not_stale() {
        assert!(stale_verdict(&idle_for(None), WEEK).is_none());
    }

    #[test]
    fn stale_rule_never_overrides_a_hard_verdict() {
        let mut open = idle_for(Some(10 * WEEK));
        open.pr = with_pr("OPEN").pr;
        assert_eq!(hard_verdict(&open, false).unwrap().kind, VerdictKind::Keep);
    }

    #[test]
    fn dirty_no_longer_shields_a_long_idle_branch() {
        let mut dirty = idle_for(Some(10 * WEEK));
        dirty.worktree_dirty = true;
        assert!(hard_verdict(&dirty, false).is_none());
        assert_eq!(stale_verdict(&dirty, WEEK).unwrap().kind, VerdictKind::Ask);
    }

    #[test]
    fn dirty_guard_only_touches_deletes() {
        let mut c = candidate();
        c.worktree_dirty = true;
        let jev_delete = compose_jev_for(&c, landed_nouls());
        assert_eq!(jev_delete.kind, VerdictKind::Delete);

        let guarded = guard_dirty(&c, jev_delete.clone(), false);
        assert_eq!(guarded.kind, VerdictKind::Ask);
        assert_eq!(guarded.source, jev_delete.source);
        assert_eq!(
            guarded.reason,
            "work already landed; worktree has uncommitted changes"
        );
        assert_eq!(guard_dirty(&c, jev_delete.clone(), true), jev_delete);

        for untouched in [
            keep(Reason::OpenPr),
            jev_ask(Reason::UncertainThrowawayPath, 0.5),
        ] {
            assert_eq!(guard_dirty(&c, untouched.clone(), false), untouched);
        }
        c.worktree_dirty = false;
        assert_eq!(guard_dirty(&c, jev_delete.clone(), false), jev_delete);
    }

    fn finished_pr_checkout(state: &str, local_only: Option<usize>) -> Candidate {
        let mut c = with_pr_at(state, "pr-head", "older-pr-head");
        c.local_only_commit_count = local_only;
        c
    }

    #[test]
    fn finished_pr_without_local_commits_is_hard_delete() {
        for (state, reason) in [
            ("MERGED", "merged PR, no local commits"),
            ("CLOSED", "closed PR, no local commits"),
        ] {
            let v = hard_verdict(&finished_pr_checkout(state, Some(0)), false).unwrap();
            assert_eq!(v.kind, VerdictKind::Delete, "{state}");
            assert_eq!(v.source, Source::Hard);
            assert_eq!(v.reason, reason);
        }
    }

    #[test]
    fn finished_pr_with_local_commits_goes_to_jev() {
        for local_only in [Some(2), None] {
            assert!(hard_verdict(&finished_pr_checkout("MERGED", local_only), false).is_none());
        }
    }

    #[test]
    fn empty_branch_reusing_a_finished_pr_name_goes_to_jev() {
        let mut c = finished_pr_checkout("MERGED", Some(0));
        c.unique_commit_count = Some(0);
        c.diverged = false;
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn open_pr_without_local_commits_is_still_kept() {
        let v = hard_verdict(&finished_pr_checkout("OPEN", Some(0)), false).unwrap();
        assert_eq!(v.kind, VerdictKind::Keep);
    }

    #[test]
    fn gone_merged_pr_with_commits_after_the_pr_head_is_not_hard_delete() {
        let mut c = with_pr_at("MERGED", "pr-head", "local-commit-after-merge");
        c.upstream_gone = true;
        c.local_only_commit_count = Some(1);
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn gone_merged_pr_at_the_pr_head_is_still_hard_delete() {
        let mut c = with_pr_at("MERGED", "pr-head", "pr-head");
        c.upstream_gone = true;
        assert_eq!(hard_verdict(&c, false).unwrap().kind, VerdictKind::Delete);
    }

    fn landed_nouls() -> Nouls {
        let mut nouls = real_work_nouls(0.9);
        nouls.work_already_landed = 0.95;
        nouls
    }

    #[test]
    fn jev_delete_never_drops_unpushed_commits_without_asking() {
        let mut c = candidate();
        c.unique_commit_count = Some(2);
        c.local_only_commit_count = Some(2);
        let v = compose_jev_for(&c, landed_nouls());
        assert_eq!(v.kind, VerdictKind::Ask, "reason: {}", v.reason);
    }

    #[test]
    fn jev_delete_of_pushed_commits_stays_delete() {
        let mut c = candidate();
        c.unique_commit_count = Some(2);
        c.local_only_commit_count = Some(0);
        assert_eq!(
            compose_jev_for(&c, landed_nouls()).kind,
            VerdictKind::Delete
        );
    }

    #[test]
    fn finished_pr_rule_spares_a_branch_still_pushed_to_its_upstream() {
        let mut c = finished_pr_checkout("MERGED", Some(0));
        c.tracks_live_upstream = true;
        assert!(hard_verdict(&c, false).is_none());
    }

    #[test]
    fn every_protection_beats_every_delete_trigger_at_once() {
        type Protect = fn(&mut Candidate);
        let protections: [(&str, Protect); 5] = [
            ("head", |c| c.is_head = true),
            ("default", |c| c.is_default = true),
            ("primary", |c| c.is_primary_worktree = true),
            ("karu.keep", |c| c.keep_configured = true),
            ("open PR", |c| {
                c.pr.as_mut().unwrap().state = crate::candidate::PrState::Open
            }),
        ];
        for (name, protect) in protections {
            let mut c = with_pr_at("MERGED", "abc123", "abc123");
            c.worktree_path = Some("/tmp/wt".into());
            c.upstream_gone = true;
            c.merged = true;
            c.diverged = true;
            c.local_only_commit_count = Some(0);
            c.non_merge_unique_count = Some(0);
            c.ref_age_secs = Some(100 * WEEK);
            protect(&mut c);
            let v = hard_verdict(&c, false).unwrap();
            assert_eq!(v.kind, VerdictKind::Keep, "{name}: {}", v.reason);
        }
    }
}
