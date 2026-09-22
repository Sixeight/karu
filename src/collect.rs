use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use std::time::Instant;

use anyhow::Result;

use crate::candidate::{Candidate, PrInfo, PrState};
use crate::decide;
use crate::git::{self, BranchRef, KaruConfig, UpstreamState, Worktree};
use crate::timing;

const UNIQUE_SUBJECT_LIMIT: usize = 8;

pub fn fetch(root: &Path) -> Result<()> {
    git::fetch_prune(root)
}

/// Everything that does not need the fetch or the PR lookup: most facts
/// compare against the local default branch. The facts a fetch can change
/// (whether an upstream is gone, which commits no remote holds) are read from
/// the refs as they are now; `reread_remote_facts` reads them again.
pub fn local_facts(root: &Path, config: &KaruConfig) -> Result<CollectResult> {
    let started = Instant::now();
    let mut collected = collect_from_git(root.to_path_buf(), config)?;
    collected.read_remote_facts()?;
    timing::report("  local facts", started);
    Ok(collected)
}

impl CollectResult {
    pub fn attach_prs(&mut self, prs: Option<PrMap>) {
        if let Some(prs) = prs {
            attach_prs(&prs, &mut self.candidates);
        }
    }

    pub fn fill_diffstats(&mut self, needed: impl Fn(usize, &Candidate) -> bool) {
        let started = Instant::now();
        let jobs: Vec<_> = self
            .candidates
            .iter()
            .enumerate()
            .filter(|(i, c)| {
                !self.diffstats_read.contains(i)
                    && c.unique_commit_count != Some(0)
                    && !(c.merged && c.diverged)
                    && needed(*i, c)
            })
            .filter_map(|(i, c)| c.sha.as_ref().or(c.branch.as_ref()).map(|rev| (i, rev)))
            .collect();
        if jobs.is_empty() {
            return;
        }
        let root = &self.root;
        let default_branch = &self.default_branch;
        let results: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = jobs
                .iter()
                .map(|(i, rev)| s.spawn(move || (*i, git::diffstat(root, default_branch, rev))))
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for (i, diffstat) in results {
            self.candidates[i].diffstat = diffstat;
            // An empty or unavailable diff is still a completed read.
            self.diffstats_read.insert(i);
        }
        timing::report("  diff summaries", started);
    }

    /// Needs the PRs: a branch with an open PR is kept whatever its worktree
    /// looks like, so its (slow) `git status` is skipped.
    pub fn mark_dirty_worktrees(&mut self) {
        let started = Instant::now();
        mark_dirty_worktrees(&mut self.candidates);
        timing::report("  dirty checks", started);
    }

    pub fn reread_remote_facts(&mut self) -> Result<()> {
        let started = Instant::now();
        self.read_remote_facts()?;
        timing::report("  remote facts, after the fetch", started);
        Ok(())
    }

    fn read_remote_facts(&mut self) -> Result<()> {
        read_remote_facts(
            &self.root,
            &self.default_branch,
            &mut self.candidates,
            &self.unique_shas,
        )
    }
}

/// Unique commits per candidate index. Only `read_remote_facts` needs them, so
/// they stay out of `Candidate`, which lives until the last prompt.
type UniqueShas = HashMap<usize, Vec<String>>;

fn read_remote_facts(
    root: &Path,
    default_branch: &str,
    candidates: &mut [Candidate],
    unique_shas: &UniqueShas,
) -> Result<()> {
    // one walk answers "which commits does no remote hold" for every branch
    let revs: Vec<String> = unique_shas
        .keys()
        .filter_map(|i| {
            let c = &candidates[*i];
            c.branch.clone().or_else(|| c.sha.clone())
        })
        .collect();
    let (upstreams, on_no_remote) = std::thread::scope(|s| {
        let upstreams = s.spawn(|| git::upstream_states(root));
        let on_no_remote = git::commits_on_no_remote(root, default_branch, &revs);
        (upstreams.join(), on_no_remote)
    });
    let upstreams = upstreams.map_err(|_| anyhow::anyhow!("reading upstream state panicked"))??;

    for (i, candidate) in candidates.iter_mut().enumerate() {
        let upstream = candidate.branch.as_ref().and_then(|b| upstreams.get(b));
        candidate.upstream_gone = upstream == Some(&UpstreamState::Gone);
        candidate.tracks_live_upstream = upstream == Some(&UpstreamState::Live);

        if candidate.unique_commit_count == Some(0) {
            candidate.local_only_commit_count = Some(0);
        } else if let Some(shas) = unique_shas.get(&i) {
            candidate.local_only_commit_count = on_no_remote
                .as_ref()
                .map(|local_only| shas.iter().filter(|sha| local_only.contains(*sha)).count());
        }
    }
    Ok(())
}

/// Looks up the newest PR of each local branch. It only needs local branch
/// names, so it can run while `fetch` is still talking to the remote.
pub fn lookup_prs(root: &Path) -> Option<PrMap> {
    pr_map(root)
}

fn collect_from_git(root: PathBuf, config: &KaruConfig) -> Result<CollectResult> {
    let default_branch = git::default_branch(&root)?;
    let head = git::current_branch(&root).unwrap_or_else(|_| "HEAD".into());
    let keep_patterns = &config.keep;
    let worktrees = git::worktree_list(&root)?;
    let primary_path = worktrees
        .first()
        .map(|wt| wt.path.clone())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    let refs = git::local_branch_refs(&root)?;
    let first_parents = git::first_parent_commits(&root, &default_branch);
    let ages = git::git_common_dir(&root)
        .map(|git_dir| git::ref_ages_secs(&git_dir))
        .unwrap_or_default();

    let mut by_branch: HashMap<String, &Worktree> = HashMap::new();
    for wt in &worktrees {
        if let Some(branch) = &wt.branch {
            by_branch.insert(branch.clone(), wt);
        }
    }

    let merged_by_name = ancestor_merged(&root, &default_branch, &refs, &first_parents);

    let mut candidates = Vec::with_capacity(refs.len());
    for info in &refs {
        let wt = by_branch.get(&info.name);
        let worktree_path = wt.map(|w| w.path.clone());
        let diverged = git::has_branch_diverged(&first_parents, &info.sha);
        let is_head = info.name == head
            || worktree_path
                .as_deref()
                .is_some_and(|p| Path::new(p) == root.as_path());
        let is_default = info.name == default_branch;
        let keep_configured = keep_patterns.iter().any(|p| wildcard_match(p, &info.name));
        let merged = !diverged || merged_by_name.get(&info.name).copied().unwrap_or(false);

        candidates.push(Candidate {
            branch: Some(info.name.clone()),
            worktree_path: worktree_path.clone(),
            last_commit_at: Some(info.last_commit_at.clone()).filter(|s| !s.is_empty()),
            last_subject: Some(info.last_subject.clone()).filter(|s| !s.is_empty()),
            unique_subjects: Vec::new(),
            behind: None,
            unique_commit_count: None,
            local_only_commit_count: None,
            non_merge_unique_count: None,
            diffstat: None,
            worktree_dirty: false,
            upstream_gone: false,
            tracks_live_upstream: false,
            merged,
            diverged,
            is_head,
            is_default,
            is_primary_worktree: worktree_path.as_deref() == Some(primary_path.as_str()),
            keep_configured,
            pr: None,
            sha: Some(info.sha.clone()),
            ref_age_secs: ages.get(&info.name).copied(),
        });
    }

    let mut unique_shas = fill_jev_facts(&root, &default_branch, &mut candidates);

    for wt in &worktrees {
        if wt.branch.is_some() {
            continue;
        }
        let (candidate, shas) = detached_candidate(&root, &default_branch, wt, &primary_path);
        if !shas.is_empty() {
            unique_shas.insert(candidates.len(), shas);
        }
        candidates.push(candidate);
    }

    Ok(CollectResult {
        root,
        default_branch,
        candidates,
        unique_shas,
        diffstats_read: HashSet::new(),
    })
}

/// Runs `git status` only for worktrees whose verdict it can change; on a
/// large repository this check dominates the collection time.
fn mark_dirty_worktrees(candidates: &mut [Candidate]) {
    let paths: Vec<(usize, String)> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| decide::dirty_can_change_verdict(c))
        .filter_map(|(i, c)| c.worktree_path.clone().map(|path| (i, path)))
        .collect();
    let dirty: Vec<(usize, bool)> = std::thread::scope(|s| {
        let handles: Vec<_> = paths
            .iter()
            .map(|(i, path)| s.spawn(move || (*i, git::is_dirty(Path::new(path)))))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    for (i, is_dirty) in dirty {
        candidates[i].worktree_dirty = is_dirty;
    }
}

fn ancestor_merged(
    root: &Path,
    default_branch: &str,
    refs: &[BranchRef],
    first_parents: &HashSet<String>,
) -> HashMap<String, bool> {
    if !refs
        .iter()
        .any(|info| git::has_branch_diverged(first_parents, &info.sha))
    {
        return HashMap::new();
    }
    let merged = git::merged_branch_refs(root, default_branch).unwrap_or_default();
    refs.iter()
        .map(|info| {
            // A name that moved since collection is not proof about this tip.
            let is_merged = merged.get(&info.name) == Some(&info.sha);
            (info.name.clone(), is_merged)
        })
        .collect()
}

fn fill_jev_facts(root: &Path, default_branch: &str, candidates: &mut [Candidate]) -> UniqueShas {
    let jobs: Vec<(usize, String)> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            !c.is_head
                && !c.is_default
                && !c.keep_configured
                && !c.is_primary_worktree
                && !(c.merged && c.diverged)
        })
        .filter_map(|(i, c)| c.branch.clone().map(|b| (i, b)))
        .collect();

    let results: HashMap<usize, UniqueFacts> = std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|(i, branch)| s.spawn(move || (*i, unique_facts(root, default_branch, branch))))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    let mut unique_shas = UniqueShas::new();
    for (i, facts) in results {
        let shas = facts.apply(&mut candidates[i]);
        if !shas.is_empty() {
            unique_shas.insert(i, shas);
        }
    }
    unique_shas
}

struct UniqueFacts {
    behind: Option<i64>,
    unique_commit_count: Option<usize>,
    /// `None` when the log could not be read, which is not "no commits".
    log: Option<git::UniqueLog>,
}

impl UniqueFacts {
    /// Writes the facts onto the candidate and hands back the commits
    /// themselves, which only the post-fetch pass needs.
    fn apply(self, candidate: &mut Candidate) -> Vec<String> {
        candidate.behind = self.behind;
        candidate.unique_commit_count = self.unique_commit_count;
        let Some(log) = self.log else {
            return Vec::new();
        };
        candidate.non_merge_unique_count = Some(log.non_merge_count);
        candidate.unique_subjects = log.subjects;
        log.shas
    }
}

fn unique_facts(root: &Path, default_branch: &str, branch: &str) -> UniqueFacts {
    let (ahead, behind) = git::ahead_behind(root, default_branch, branch)
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((None, None));
    let unique_commit_count = ahead.map(|a| a.max(0) as usize);
    if unique_commit_count == Some(0) {
        return UniqueFacts {
            behind,
            unique_commit_count,
            log: Some(git::UniqueLog::default()),
        };
    }
    UniqueFacts {
        behind,
        unique_commit_count,
        log: git::unique_log(root, branch, default_branch, UNIQUE_SUBJECT_LIMIT),
    }
}

fn detached_candidate(
    root: &Path,
    default_branch: &str,
    wt: &Worktree,
    primary_path: &str,
) -> (Candidate, Vec<String>) {
    let path = Path::new(&wt.path);
    let facts = unique_facts(path, default_branch, "HEAD");
    let (last_commit_at, last_subject) = git::last_commit(path, "HEAD")
        .map(|(d, s)| (Some(d), Some(s)))
        .unwrap_or((None, None));
    let mut candidate = Candidate {
        branch: None,
        worktree_path: Some(wt.path.clone()),
        last_commit_at,
        last_subject,
        unique_subjects: Vec::new(),
        behind: None,
        unique_commit_count: None,
        local_only_commit_count: None,
        non_merge_unique_count: None,
        diffstat: None,
        worktree_dirty: false,
        upstream_gone: false,
        tracks_live_upstream: false,
        merged: false,
        diverged: facts.unique_commit_count.unwrap_or(0) > 0,
        is_head: path == root,
        is_default: false,
        is_primary_worktree: wt.path == primary_path,
        keep_configured: false,
        pr: None,
        sha: git::git_output(path, &["rev-parse", "HEAD"]).ok(),
        ref_age_secs: None,
    };
    let shas = facts.apply(&mut candidate);
    (candidate, shas)
}

pub struct CollectResult {
    pub root: PathBuf,
    pub default_branch: String,
    pub candidates: Vec<Candidate>,
    unique_shas: UniqueShas,
    diffstats_read: HashSet<usize>,
}

/// Small chunks sent together beat one big query on latency.
const PR_QUERY_CHUNK: usize = 10;

pub type PrMap = HashMap<String, PrInfo>;

fn pr_map(root: &Path) -> Option<PrMap> {
    if !github_origin(root) {
        return None;
    }
    let branches = git::local_branch_names(root).ok()?;
    if branches.is_empty() {
        return None;
    }
    gh_pr_map_for(root, &branches).or_else(|| gh_pr_map(root))
}

fn attach_prs(map: &PrMap, candidates: &mut [Candidate]) {
    for candidate in candidates {
        if let Some(branch) = &candidate.branch {
            candidate.pr = map.get(branch).cloned();
        }
    }
}

fn github_origin(dir: &Path) -> bool {
    crate::git::git_output(dir, &["remote", "get-url", "origin"])
        .map(|url| url.contains("github.com"))
        .unwrap_or(false)
}

fn gh_pr_map_for(dir: &Path, branches: &[String]) -> Option<PrMap> {
    let chunks: Vec<Option<PrMap>> = std::thread::scope(|s| {
        let handles: Vec<_> = branches
            .chunks(PR_QUERY_CHUNK)
            .map(|chunk| s.spawn(move || gh_pr_chunk(dir, chunk)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().ok().flatten())
            .collect()
    });
    let mut map = HashMap::new();
    for chunk in chunks {
        map.extend(chunk?);
    }
    Some(map)
}

fn gh_pr_chunk(dir: &Path, branches: &[String]) -> Option<PrMap> {
    let output = std::process::Command::new("gh")
        .args([
            "api",
            "graphql",
            "-F",
            "owner={owner}",
            "-F",
            "name={repo}",
            "-f",
            &format!("query={}", pr_query(branches)),
        ])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_pr_response(&serde_json::from_slice(&output.stdout).ok()?, branches)
}

fn pr_query(branches: &[String]) -> String {
    let fields: String = branches
        .iter()
        .enumerate()
        .map(|(i, branch)| {
            format!(
                "b{i}: pullRequests(headRefName: {}, first: 1, orderBy: {{field: CREATED_AT, direction: DESC}}) {{ nodes {{ number state title mergedAt headRefOid }} }}\n",
                serde_json::Value::from(branch.as_str())
            )
        })
        .collect();
    format!(
        "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{\n{fields}}} }}"
    )
}

fn parse_pr_response(value: &serde_json::Value, branches: &[String]) -> Option<PrMap> {
    let repository = value.get("data")?.get("repository")?;
    let mut map = HashMap::new();
    for (i, branch) in branches.iter().enumerate() {
        let Some(node) = repository
            .get(format!("b{i}"))
            .and_then(|prs| prs.get("nodes"))
            .and_then(|nodes| nodes.get(0))
        else {
            continue;
        };
        if let Some(pr) = pr_info(node) {
            map.insert(branch.clone(), pr);
        }
    }
    Some(map)
}

fn pr_info(item: &serde_json::Value) -> Option<PrInfo> {
    Some(PrInfo {
        state: PrState::parse(item.get("state")?.as_str()?)?,
        title: item.get("title")?.as_str()?.to_string(),
        merged_at: item
            .get("mergedAt")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        head_sha: item
            .get("headRefOid")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        number: item.get("number").and_then(|v| v.as_u64()),
    })
}

/// Whole-repository listing; slow on busy repositories, so it only backs up
/// the per-branch query.
fn gh_pr_map(dir: &Path) -> Option<HashMap<String, PrInfo>> {
    let output = std::process::Command::new("gh")
        .args([
            "pr",
            "list",
            "--state",
            "all",
            "--json",
            "headRefName,state,title,mergedAt,headRefOid,number",
            "--limit",
            "1000",
        ])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_pr_list(&serde_json::from_slice(&output.stdout).ok()?)
}

fn parse_pr_list(value: &serde_json::Value) -> Option<HashMap<String, PrInfo>> {
    let mut map = HashMap::new();
    for item in value.as_array()? {
        let branch = item.get("headRefName")?.as_str()?.to_string();
        if let Some(pr) = pr_info(item) {
            map.entry(branch).or_insert(pr);
        }
    }
    Some(map)
}

pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    if !parts[0].is_empty() {
        if let Some(stripped) = rest.strip_prefix(parts[0]) {
            rest = stripped;
        } else {
            return false;
        }
    }
    for (i, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            if i == parts.len() - 1 {
                return true;
            }
            continue;
        }
        if i == parts.len() - 1 {
            return rest.ends_with(part);
        }
        if let Some(idx) = rest.find(part) {
            rest = &rest[idx + part.len()..];
        } else {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_unmerged_branch() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        git::git_output(root, &["init", "--bare", "-b", "main"]).unwrap();
        let tree = git::git_output(root, &["mktree"]).unwrap();
        let commit = |subject, parents: &[&str]| {
            let mut args = vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                &tree,
                "-m",
                subject,
            ];
            for parent in parents {
                args.extend(["-p", parent]);
            }
            git::git_output(root, &args).unwrap()
        };
        let initial = commit("initial", &[]);
        let main = commit("main", &[&initial]);
        let feature = commit("feature", &[&initial]);
        for (name, sha) in [("refs/heads/main", main), ("refs/heads/feature", feature)] {
            git::git_output(root, &["update-ref", name, &sha]).unwrap();
        }
        repo
    }

    #[test]
    fn moving_a_branch_to_main_does_not_prove_its_collected_tip_was_merged() {
        let repo = repo_with_unmerged_branch();
        let root = repo.path();
        let refs = git::local_branch_refs(root).unwrap();
        let first_parents = git::first_parent_commits(root, "main");
        git::git_output(root, &["update-ref", "refs/heads/feature", "main"]).unwrap();

        let merged = ancestor_merged(root, "main", &refs, &first_parents);
        assert_eq!(merged.get("feature"), Some(&false));
    }

    #[test]
    fn failed_ancestor_lookup_does_not_mark_any_branch_merged() {
        let repo = repo_with_unmerged_branch();
        let root = repo.path();
        let refs = git::local_branch_refs(root).unwrap();
        let first_parents = git::first_parent_commits(root, "main");

        let merged = ancestor_merged(root, "missing", &refs, &first_parents);
        assert_eq!(merged.get("feature"), Some(&false));
        assert!(merged.values().all(|value| !value));
    }

    #[test]
    fn pr_with_an_unknown_state_is_left_out_without_losing_the_others() {
        let branches = vec!["sixeight/odd".to_string(), "sixeight/x".to_string()];
        let response = serde_json::json!({"data": {"repository": {
            "b0": {"nodes": [{"state": "SOMETHING_NEW", "title": "t", "mergedAt": null,
                              "headRefOid": "aaa", "number": 1}]},
            "b1": {"nodes": [{"state": "OPEN", "title": "t", "mergedAt": null,
                              "headRefOid": "bbb", "number": 2}]},
        }}});
        let map = parse_pr_response(&response, &branches).unwrap();
        assert!(!map.contains_key("sixeight/odd"));
        assert_eq!(map["sixeight/x"].state, PrState::Open);
    }

    #[test]
    fn pr_query_asks_only_for_the_given_branches() {
        let branches = vec!["sixeight/x".to_string(), "we\"ird".to_string()];
        let query = pr_query(&branches);
        assert!(query.contains(r#"b0: pullRequests(headRefName: "sixeight/x""#));
        assert!(query.contains(r#"b1: pullRequests(headRefName: "we\"ird""#));
        assert!(query.contains("first: 1"));
        assert!(query.contains("direction: DESC"));
        assert!(!query.contains("b2:"));
    }

    #[test]
    fn pr_response_maps_aliases_back_to_branches() {
        let branches = vec!["sixeight/x".to_string(), "sixeight/no-pr".to_string()];
        let response = serde_json::json!({"data": {"repository": {
            "b0": {"nodes": [{"state": "MERGED", "title": "t",
                              "mergedAt": "2026-09-18T12:34:37Z", "headRefOid": "abc",
                              "number": 7}]},
            "b1": {"nodes": []},
        }}});
        let map = parse_pr_response(&response, &branches).unwrap();
        assert_eq!(map.len(), 1);
        let pr = &map["sixeight/x"];
        assert_eq!(pr.state, PrState::Merged);
        assert_eq!(pr.head_sha.as_deref(), Some("abc"));
        assert_eq!(pr.merged_at.as_deref(), Some("2026-09-18T12:34:37Z"));
        assert_eq!(pr.number, Some(7));
    }

    #[test]
    fn newest_pr_wins_and_carries_its_head_commit() {
        let listed_newest_first = serde_json::json!([
            {"headRefName": "sixeight/x", "state": "OPEN", "title": "second try",
             "mergedAt": null, "headRefOid": "bbb"},
            {"headRefName": "sixeight/x", "state": "CLOSED", "title": "first try",
             "mergedAt": null, "headRefOid": "aaa"},
        ]);
        let map = parse_pr_list(&listed_newest_first).unwrap();
        let pr = &map["sixeight/x"];
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.head_sha.as_deref(), Some("bbb"));
    }

    #[test]
    fn wildcard_exact() {
        assert!(wildcard_match("main", "main"));
        assert!(!wildcard_match("main", "main2"));
    }

    #[test]
    fn wildcard_star() {
        assert!(wildcard_match("sixeight/*", "sixeight/foo"));
        assert!(wildcard_match("release-*", "release-1"));
        assert!(!wildcard_match("sixeight/*", "other/foo"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "aXXXbYYYc"));
        assert!(!wildcard_match("a*b*c", "aXXXc"));
    }

    #[test]
    fn wildcard_empty_and_boundary() {
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("", ""));
        assert!(!wildcard_match("", "x"));
        assert!(wildcard_match("foo*", "foo"));
        assert!(wildcard_match("*foo", "foo"));
        assert!(wildcard_match("*foo*", "foo"));
        assert!(wildcard_match("a*b*c", "abc"));
        assert!(wildcard_match("release-*", "release-"));
        assert!(!wildcard_match("sixeight/*", "sixeight"));
        assert!(!wildcard_match("sixeight/*", "sixeightfoo"));
        assert!(wildcard_match("**", "anything"));
        assert!(wildcard_match("*/*", "a/b"));
        assert!(wildcard_match("*/*", "a/b/c"));
        assert!(!wildcard_match("*/*", "ab"));
    }
}
