use serde::{Deserialize, Serialize};

use crate::reason::Reason;

/// Serialized the way GitHub spells it, which is what Jev's questions quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

impl PrState {
    pub fn parse(raw: &str) -> Option<PrState> {
        match raw.to_ascii_uppercase().as_str() {
            "OPEN" => Some(PrState::Open),
            "MERGED" => Some(PrState::Merged),
            "CLOSED" => Some(PrState::Closed),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrInfo {
    pub state: PrState,
    pub title: String,
    pub merged_at: Option<String>,
    pub head_sha: Option<String>,
    pub number: Option<u64>,
}

impl PrInfo {
    pub fn state_label(&self) -> &'static str {
        self.state.label()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub last_commit_at: Option<String>,
    pub last_subject: Option<String>,
    pub unique_subjects: Vec<String>,
    pub behind: Option<i64>,
    pub unique_commit_count: Option<usize>,
    /// Unique commits that no remote ref can reach; deleting the branch loses them.
    pub local_only_commit_count: Option<usize>,
    /// Unique commits with a single parent, i.e. not merges of another branch.
    pub non_merge_unique_count: Option<usize>,
    pub diffstat: Option<String>,
    pub worktree_dirty: bool,
    pub upstream_gone: bool,
    /// The branch tracks a remote branch that still exists.
    pub tracks_live_upstream: bool,
    pub merged: bool,
    pub diverged: bool,
    pub is_head: bool,
    pub is_default: bool,
    pub is_primary_worktree: bool,
    pub keep_configured: bool,
    pub pr: Option<PrInfo>,
    pub sha: Option<String>,
    /// Seconds since the local ref last moved (reflog). None if unknown.
    pub ref_age_secs: Option<u64>,
}

/// Everything Jev is shown about a candidate, and nothing else. It is what
/// gets sent, and also what decides whether an earlier answer still applies.
/// A field belongs here only if some question inspects it (a test checks
/// that). Local paths, commit hashes and the facts only the rules use
/// (`merged`, `diverged`, `tracks_live_upstream`, ...) stay out.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct JevInput {
    pub branch: Option<String>,
    pub last_commit_at: Option<String>,
    pub last_subject: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unique_subjects: Vec<String>,
    pub behind: Option<i64>,
    pub unique_commit_count: Option<usize>,
    pub local_only_commit_count: Option<usize>,
    pub diffstat: Option<String>,
    pub upstream_gone: bool,
    pub pr: Option<JevPr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_age_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct JevPr {
    pub state: PrState,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
}

impl Candidate {
    pub fn jev_input(&self) -> JevInput {
        JevInput {
            branch: self.branch.clone(),
            last_commit_at: self.last_commit_at.clone(),
            last_subject: self.last_subject.clone(),
            unique_subjects: self.unique_subjects.clone(),
            behind: self.behind,
            unique_commit_count: self.unique_commit_count,
            local_only_commit_count: self.local_only_commit_count,
            diffstat: self.diffstat.clone(),
            upstream_gone: self.upstream_gone,
            pr: self.pr.as_ref().map(|pr| JevPr {
                state: pr.state,
                title: pr.title.clone(),
                merged_at: pr.merged_at.clone(),
            }),
            ref_age_secs: self.ref_age_secs,
        }
    }

    /// Last path component of the worktree, which is how people refer to it.
    pub fn worktree_dir_name(&self) -> Option<String> {
        let path = self.worktree_path.as_deref()?;
        Some(
            std::path::Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string()),
        )
    }

    pub fn display_name(&self) -> String {
        self.branch
            .clone()
            .or_else(|| self.worktree_path.clone())
            .unwrap_or_else(|| "(unknown)".into())
    }

    pub fn unique_commits_are_merges_only(&self) -> bool {
        self.unique_commit_count.unwrap_or(0) > 0 && self.non_merge_unique_count == Some(0)
    }

    pub fn gone_with_merged_pr(&self) -> bool {
        self.upstream_gone && !self.has_moved_off_pr_head() && self.has_merged_pr()
    }

    fn pr_state_is(&self, state: PrState) -> bool {
        self.pr.as_ref().is_some_and(|pr| pr.state == state)
    }

    pub fn has_open_pr(&self) -> bool {
        self.pr_state_is(PrState::Open)
    }

    pub fn has_merged_pr(&self) -> bool {
        self.pr_state_is(PrState::Merged)
    }

    /// The branch tip is exactly the commit the PR was made from, so the PR's
    /// state speaks for every commit on the branch.
    pub fn is_at_pr_head(&self) -> bool {
        self.pr
            .as_ref()
            .is_some_and(|pr| pr.head_sha.is_some() && pr.head_sha == self.sha)
    }

    pub fn has_moved_off_pr_head(&self) -> bool {
        self.sha.is_some()
            && self
                .pr
                .as_ref()
                .is_some_and(|pr| pr.head_sha.is_some() && pr.head_sha != self.sha)
    }

    pub fn is_at_merged_pr_head(&self) -> bool {
        self.is_at_pr_head() && self.has_merged_pr()
    }

    pub fn is_at_closed_pr_head(&self) -> bool {
        self.is_at_pr_head() && self.has_closed_pr()
    }

    /// A merged or closed PR, and every unique commit came from outside: a
    /// review checkout or a superseded copy, with no work of our own on it.
    /// A branch still pushed to its own upstream is work in progress, whatever
    /// an earlier PR of the same name says.
    pub fn is_finished_pr_without_local_commits(&self) -> bool {
        self.pr.is_some()
            && !self.has_open_pr()
            && !self.tracks_live_upstream
            && self.unique_commit_count.unwrap_or(0) > 0
            && self.local_only_commit_count == Some(0)
    }

    pub fn has_closed_pr(&self) -> bool {
        self.pr_state_is(PrState::Closed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Keep,
    Delete,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Hard,
    Jev,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub kind: VerdictKind,
    pub source: Source,
    pub reason: Reason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Judged {
    pub candidate: Candidate,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Nouls {
    pub work_already_landed: f64,
    pub name_is_throwaway: f64,
    pub looks_abandoned: f64,
    pub unique_commits_are_noise: f64,
    pub empty_ref_is_leftover: f64,
}

/// One neutral candidate for tests; each test module overrides what it cares
/// about, so a new field only has to be added here.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::Candidate;

    pub fn candidate() -> Candidate {
        Candidate {
            branch: Some("sixeight/tmp-spike".into()),
            worktree_path: None,
            last_commit_at: Some("2026-03-02".into()),
            last_subject: Some("wip".into()),
            unique_subjects: vec!["wip".into()],
            behind: Some(10),
            unique_commit_count: Some(1),
            local_only_commit_count: None,
            non_merge_unique_count: None,
            diffstat: Some("1 file changed".into()),
            worktree_dirty: false,
            upstream_gone: false,
            tracks_live_upstream: false,
            merged: false,
            diverged: true,
            is_head: false,
            is_default: false,
            is_primary_worktree: false,
            keep_configured: false,
            pr: None,
            sha: None,
            ref_age_secs: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_candidate() -> Candidate {
        Candidate {
            branch: Some("sixeight/x".into()),
            worktree_path: Some("/Users/someone/wt".into()),
            unique_subjects: vec!["feat: one".into()],
            behind: Some(12),
            unique_commit_count: Some(3),
            local_only_commit_count: Some(1),
            non_merge_unique_count: Some(2),
            worktree_dirty: true,
            upstream_gone: true,
            tracks_live_upstream: false,
            merged: false,
            pr: Some(PrInfo {
                state: PrState::Closed,
                title: "t".into(),
                merged_at: Some("2026-09-18T00:00:00Z".into()),
                head_sha: Some("abc".into()),
                number: Some(7),
            }),
            sha: Some("def".into()),
            ref_age_secs: Some(3600),
            ..fixtures::candidate()
        }
    }

    /// The exact JSON Jev has been receiving. `JevInput` is now the only thing
    /// that decides it, so this pins the payload field by field.
    #[test]
    fn jev_input_is_the_payload_jev_has_always_received() {
        let full = serde_json::to_value(full_candidate().jev_input()).unwrap();
        assert_eq!(
            full,
            serde_json::json!({
                "branch": "sixeight/x",
                "last_commit_at": "2026-03-02",
                "last_subject": "wip",
                "unique_subjects": ["feat: one"],
                "behind": 12,
                "unique_commit_count": 3,
                "local_only_commit_count": 1,
                "diffstat": "1 file changed",
                "upstream_gone": true,
                "pr": {"state": "CLOSED", "title": "t", "merged_at": "2026-09-18T00:00:00Z"},
                "ref_age_secs": 3600,
            })
        );

        let mut bare = fixtures::candidate();
        bare.unique_subjects = Vec::new();
        let bare = serde_json::to_value(bare.jev_input()).unwrap();
        let keys: Vec<&str> = bare
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(!keys.contains(&"unique_subjects"), "{keys:?}");
        assert!(!keys.contains(&"ref_age_secs"), "{keys:?}");
        assert_eq!(bare["pr"], serde_json::Value::Null);
    }

    #[test]
    fn jev_input_changes_only_when_something_jev_sees_changes() {
        let base = full_candidate();
        let mut local_only = base.clone();
        local_only.worktree_path = None;
        local_only.worktree_dirty = false;
        local_only.sha = None;
        local_only.is_primary_worktree = true;
        assert_eq!(base.jev_input(), local_only.jev_input());

        let mut after_fetch = base.clone();
        after_fetch.upstream_gone = false;
        assert_ne!(base.jev_input(), after_fetch.jev_input());

        // facts the rules use but no question refers to are not part of it
        let mut rules_only = base.clone();
        rules_only.tracks_live_upstream = true;
        rules_only.merged = true;
        rules_only.diverged = false;
        rules_only.non_merge_unique_count = Some(9);
        assert_eq!(base.jev_input(), rules_only.jev_input());
    }

    #[test]
    fn pr_state_keeps_the_wording_jev_and_the_table_see() {
        for (raw, state, label) in [
            ("OPEN", PrState::Open, "open"),
            ("MERGED", PrState::Merged, "merged"),
            ("CLOSED", PrState::Closed, "closed"),
        ] {
            assert_eq!(PrState::parse(raw), Some(state));
            assert_eq!(PrState::parse(&raw.to_lowercase()), Some(state));
            assert_eq!(state.label(), label);
            assert_eq!(serde_json::to_value(state).unwrap(), serde_json::json!(raw));
        }
    }

    #[test]
    fn unknown_pr_state_is_not_a_state() {
        assert_eq!(PrState::parse("DRAFT"), None);
        assert_eq!(PrState::parse(""), None);
    }
}
