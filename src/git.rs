use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

pub fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to execute: git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    git_output(dir, args).is_ok()
}

pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git_output(
        dir,
        &["rev-parse", "--show-toplevel"],
    )?))
}

pub fn current_branch(dir: &Path) -> Result<String> {
    git_output(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
}

pub fn has_head_commit(dir: &Path) -> Result<bool> {
    let error = match git_output(dir, &["rev-parse", "--verify", "HEAD^{commit}"]) {
        Ok(_) => return Ok(true),
        Err(error) => error,
    };
    let Ok(branch) = git_output(dir, &["symbolic-ref", "--quiet", "HEAD"]) else {
        return Err(error);
    };
    if !branch.starts_with("refs/heads/") {
        return Err(error);
    }
    let output = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &branch])
        .current_dir(dir)
        .output()
        .context("failed to check the current branch")?;
    if output.status.code() == Some(1) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// What a local branch is, independent of any remote. Upstream state is a
/// separate question (`upstream_states`) because a fetch changes its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRef {
    pub name: String,
    pub sha: String,
    pub last_commit_at: String,
    pub last_subject: String,
}

pub fn local_branch_names(dir: &Path) -> Result<Vec<String>> {
    let raw = git_output(
        dir,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )?;
    Ok(raw
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

pub fn local_branch_refs(dir: &Path) -> Result<Vec<BranchRef>> {
    let raw = git_output(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname:short)%00%(objectname)%00%(committerdate:short)%00%(contents:subject)",
            "refs/heads",
        ],
    )?;
    Ok(parse_branch_refs(&raw))
}

pub fn parse_branch_refs(raw: &str) -> Vec<BranchRef> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut parts = line.split('\0');
            let name = parts.next()?.to_string();
            let sha = parts.next()?.to_string();
            let last_commit_at = parts.next()?.to_string();
            let last_subject = parts.next()?.to_string();
            if name.is_empty() {
                return None;
            }
            Some(BranchRef {
                name,
                sha,
                last_commit_at,
                last_subject,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamState {
    None,
    Live,
    /// Configured, but the ref it names is gone (deleted remotely and pruned).
    Gone,
}

/// Upstream state per local branch, by checking that the configured upstream
/// ref still exists. `%(upstream:track)` answers the same, but walks history
/// for every branch to count ahead/behind, which nothing here needs.
pub fn upstream_states(dir: &Path) -> Result<HashMap<String, UpstreamState>> {
    let raw = git_output(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(upstream)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    Ok(parse_upstream_states(&raw))
}

pub fn parse_upstream_states(raw: &str) -> HashMap<String, UpstreamState> {
    let rows: Vec<(&str, &str)> = raw
        .lines()
        .filter_map(|line| line.split_once('\0'))
        .collect();
    let existing: HashSet<&str> = rows.iter().map(|(name, _)| *name).collect();
    rows.iter()
        .filter_map(|(name, upstream)| {
            let branch = name.strip_prefix("refs/heads/")?;
            let state = if upstream.is_empty() {
                UpstreamState::None
            } else if existing.contains(upstream) {
                UpstreamState::Live
            } else {
                UpstreamState::Gone
            };
            Some((branch.to_string(), state))
        })
        .collect()
}

pub fn default_branch(dir: &Path) -> Result<String> {
    if let Ok(origin_head) = git_output(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        if let Some(name) = origin_head.strip_prefix("origin/") {
            return Ok(name.to_string());
        }
        return Ok(origin_head);
    }
    if git_ok(dir, &["rev-parse", "--verify", "refs/heads/main"]) {
        return Ok("main".into());
    }
    if git_ok(dir, &["rev-parse", "--verify", "refs/heads/master"]) {
        return Ok("master".into());
    }
    current_branch(dir)
}

/// Seconds, or a number with `s`, `m` or `h`.
fn parse_max_age(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (number, unit) = match raw.char_indices().last()? {
        (i, 's') => (&raw[..i], 1),
        (i, 'm') => (&raw[..i], 60),
        (i, 'h') => (&raw[..i], 60 * 60),
        _ => (raw, 1),
    };
    number.parse::<u64>().ok()?.checked_mul(unit)
}

/// Seconds since this worktree last fetched, from `FETCH_HEAD`'s mtime.
pub fn secs_since_last_fetch(dir: &Path) -> Option<u64> {
    let path = git_output(dir, &["rev-parse", "--git-path", "FETCH_HEAD"]).ok()?;
    let modified = fs::metadata(dir.join(path)).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|age| age.as_secs())
}

pub fn fetch_prune(dir: &Path) -> Result<()> {
    git_output(dir, &["fetch", "--prune"]).map(|_| ())
}

pub fn first_parent_commits(dir: &Path, ref_name: &str) -> HashSet<String> {
    git_output(dir, &["rev-list", "--first-parent", ref_name])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn has_branch_diverged(first_parents: &HashSet<String>, tip: &str) -> bool {
    !first_parents.contains(tip)
}

pub fn merged_branch_refs(dir: &Path, default_branch: &str) -> Result<HashMap<String, String>> {
    let raw = git_output(
        dir,
        &[
            "for-each-ref",
            &format!("--merged={default_branch}"),
            "--format=%(refname:short)%00%(objectname)",
            "refs/heads",
        ],
    )?;
    Ok(raw
        .lines()
        .filter_map(|line| line.split_once('\0'))
        .map(|(name, sha)| (name.to_string(), sha.to_string()))
        .collect())
}

#[derive(Default)]
pub struct UniqueLog {
    /// Newest first, every commit the default branch does not have.
    pub shas: Vec<String>,
    /// Commits with at most one parent.
    pub non_merge_count: usize,
    pub subjects: Vec<String>,
}

/// One `git log` for what used to take a process each: which commits are
/// unique, how many are real (non-merge) work, and the newest subjects.
pub fn unique_log(
    dir: &Path,
    branch: &str,
    default_branch: &str,
    subject_limit: usize,
) -> Option<UniqueLog> {
    let raw = git_output(
        dir,
        &[
            "log",
            "--format=%H%x00%P%x00%s",
            branch,
            "--not",
            default_branch,
        ],
    )
    .ok()?;
    Some(parse_unique_log(&raw, subject_limit))
}

fn parse_unique_log(raw: &str, subject_limit: usize) -> UniqueLog {
    let mut log = UniqueLog::default();
    for line in raw.lines().filter(|line| !line.is_empty()) {
        let mut parts = line.splitn(3, '\0');
        let (Some(sha), Some(parents), Some(subject)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        log.shas.push(sha.to_string());
        if parents.split_whitespace().count() <= 1 {
            log.non_merge_count += 1;
        }
        if log.subjects.len() < subject_limit && !subject.is_empty() {
            log.subjects.push(subject.to_string());
        }
    }
    log
}

pub fn last_commit(dir: &Path, rev: &str) -> Option<(String, String)> {
    let raw = git_output(dir, &["log", "-1", "--format=%cs%x00%s", rev]).ok()?;
    let (date, subject) = raw.split_once('\0')?;
    Some((date.to_string(), subject.to_string()))
}

pub fn git_common_dir(dir: &Path) -> Result<PathBuf> {
    let raw = git_output(dir, &["rev-parse", "--git-common-dir"])?;
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(dir.join(path))
    }
}

pub fn ref_ages_secs(git_dir: &Path) -> HashMap<String, u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = HashMap::new();
    walk_reflogs(&git_dir.join("logs/refs/heads"), "", now, &mut out);
    out
}

fn walk_reflogs(dir: &Path, prefix: &str, now: u64, out: &mut HashMap<String, u64>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            let next = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            walk_reflogs(&path, &next, now, out);
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(line) = raw.lines().next_back() else {
            continue;
        };
        let Some(unix) = parse_reflog_line_unix(line) else {
            continue;
        };
        let key = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if let Some(age) = now.checked_sub(unix) {
            out.insert(key, age);
        }
    }
}

pub fn parse_reflog_line_unix(line: &str) -> Option<u64> {
    let idx = line.find("> ")?;
    let rest = line.get(idx + 2..)?;
    rest.split_whitespace().next()?.parse().ok()
}

/// Of the commits reachable from `revs`, those no fetched ref can reach: not
/// the remotes, not `git maintenance` prefetches, not pull refs (`refs/pull/*`,
/// or a review tool's `refs/<tool>/pull/*`). Locally made refs (backups, other
/// worktrees' HEADs) do not count as "outside", so `--all` is deliberately not
/// used. One walk for every branch instead of one per branch.
pub fn commits_on_no_remote(
    dir: &Path,
    default_branch: &str,
    revs: &[String],
) -> Option<HashSet<String>> {
    if revs.is_empty() {
        return Some(HashSet::new());
    }
    let mut args = vec!["rev-list"];
    args.extend(revs.iter().map(String::as_str));
    args.extend([
        "--not",
        default_branch,
        "--remotes",
        "--glob=refs/prefetch/*",
        "--glob=refs/pull/*",
        "--glob=refs/*/pull/*",
    ]);
    Some(
        git_output(dir, &args)
            .ok()?
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

pub fn ahead_behind(dir: &Path, default_branch: &str, branch: &str) -> Option<(i64, i64)> {
    let raw = git_output(
        dir,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{default_branch}...{branch}"),
        ],
    )
    .ok()?;
    let (behind, ahead) = raw.split_once('\t')?;
    Some((ahead.parse().ok()?, behind.parse().ok()?))
}

pub fn diffstat(dir: &Path, default_branch: &str, branch: &str) -> Option<String> {
    let raw = git_output(
        dir,
        &[
            "diff",
            "--shortstat",
            &format!("{default_branch}...{branch}"),
        ],
    )
    .ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// A worktree whose directory is gone has nothing uncommitted to lose; any
/// other failure to ask counts as dirty, to stay on the safe side.
pub fn is_dirty(dir: &Path) -> bool {
    if !dir.exists() {
        return false;
    }
    git_output(dir, &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(true)
}

/// karu's own settings, read with a single `git config` call.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct KaruConfig {
    /// `karu.keep`, every value.
    pub keep: Vec<String>,
    /// `karu.staleDays`, the last valid value. 0 means "never stale".
    pub stale_days: Option<u64>,
    /// `karu.fetchMaxAge` in seconds, the last valid value. 0 means off.
    pub fetch_max_age_secs: Option<u64>,
    /// `karu.jev`. Sending anything to Jev is opt-in per repository, so
    /// `None` (unset) means off just as `Some(false)` does.
    pub jev: Option<bool>,
}

pub fn karu_config(dir: &Path) -> KaruConfig {
    // exits non-zero when nothing matches, which just means "no settings"
    parse_karu_config(&git_output(dir, &["config", "--get-regexp", r"^karu\."]).unwrap_or_default())
}

fn parse_karu_config(raw: &str) -> KaruConfig {
    let mut config = KaruConfig::default();
    for line in raw.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "karu.keep" if !value.is_empty() => config.keep.push(value.to_string()),
            "karu.staledays" => config.stale_days = value.trim().parse().ok(),
            "karu.fetchmaxage" => config.fetch_max_age_secs = parse_max_age(value),
            "karu.jev" => config.jev = parse_bool(value),
            _ => {}
        }
    }
    config
}

/// The spellings `git config --bool` accepts. A key with no value at all is
/// not an answer, so it stays `None`: this gates what leaves the machine.
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

pub fn worktree_remove(dir: &Path, path: &str, force: bool) -> Result<()> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path);
    git_output(dir, &args).map(|_| ())
}

/// Always `-D`: `-d` checks against HEAD or the upstream, not the default
/// branch the verdict was based on, so it rejects branches karu verified.
pub fn branch_delete(dir: &Path, branch: &str) -> Result<()> {
    git_output(dir, &["branch", "-D", branch]).map(|_| ())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    pub branch: Option<String>,
}

pub fn worktree_list(dir: &Path) -> Result<Vec<Worktree>> {
    let raw = git_output(dir, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_porcelain(&raw))
}

pub fn parse_worktree_porcelain(raw: &str) -> Vec<Worktree> {
    let mut result = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;

    for line in raw.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            flush_worktree(&mut result, &mut current_path, &mut current_branch);
            current_path = Some(path.to_string());
            current_branch = None;
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            current_branch = Some(
                branch_ref
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch_ref)
                    .to_string(),
            );
        } else if line.is_empty() {
            flush_worktree(&mut result, &mut current_path, &mut current_branch);
        }
    }
    flush_worktree(&mut result, &mut current_path, &mut current_branch);
    result
}

fn flush_worktree(
    result: &mut Vec<Worktree>,
    current_path: &mut Option<String>,
    current_branch: &mut Option<String>,
) {
    if let Some(path) = current_path.take() {
        result.push(Worktree {
            path,
            branch: current_branch.take(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn setup_repo() -> tempfile::TempDir {
        let repo = tempfile::TempDir::new().unwrap();
        git_output(repo.path(), &["init", "-b", "main"]).unwrap();
        git_output(repo.path(), &["config", "user.email", "test@example.com"]).unwrap();
        git_output(repo.path(), &["config", "user.name", "Test"]).unwrap();
        git_output(repo.path(), &["config", "commit.gpgsign", "false"]).unwrap();
        let root = commit(repo.path(), "initial", &[]);
        git_output(repo.path(), &["update-ref", "refs/heads/main", &root]).unwrap();
        repo
    }

    fn commit(dir: &Path, subject: &str, parents: &[&str]) -> String {
        let tree = git_output(dir, &["mktree"]).unwrap();
        let mut args = vec!["commit-tree", &tree, "-m", subject];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        git_output(dir, &args).unwrap()
    }

    #[test]
    fn merged_branch_refs_returns_only_ancestor_local_branch_tips() {
        let repo = setup_repo();
        let dir = repo.path();
        let root = git_output(dir, &["rev-parse", "main"]).unwrap();
        let feature = commit(dir, "feature", &[&root]);
        let main = commit(dir, "merge feature", &[&root, &feature]);
        let unmerged = commit(dir, "unmerged", &[&root]);
        for (name, sha) in [
            ("refs/heads/main", &main),
            ("refs/heads/behind", &root),
            ("refs/heads/topic/merged", &feature),
            ("refs/heads/topic/unmerged", &unmerged),
            ("refs/remotes/origin/merged", &main),
            ("refs/tags/merged", &main),
        ] {
            git_output(dir, &["update-ref", name, sha]).unwrap();
        }

        assert_eq!(
            merged_branch_refs(dir, "main").unwrap(),
            HashMap::from([
                ("main".to_string(), main),
                ("behind".to_string(), root),
                ("topic/merged".to_string(), feature),
            ])
        );
    }

    #[test]
    fn merged_branch_refs_fails_when_default_branch_is_missing() {
        let repo = setup_repo();
        assert!(merged_branch_refs(repo.path(), "missing").is_err());
    }

    #[test]
    fn merged_branch_refs_never_marks_a_missing_branch_object_as_merged() {
        let repo = setup_repo();
        fs::write(
            repo.path().join(".git/refs/heads/broken"),
            format!("{}\n", "f".repeat(40)),
        )
        .unwrap();

        let merged = merged_branch_refs(repo.path(), "main").unwrap();
        assert!(merged.contains_key("main"));
        assert!(!merged.contains_key("broken"));
        assert!(merged_branch_refs(repo.path(), "broken").is_err());
    }

    #[test]
    fn parse_branched_and_detached_worktrees() {
        let raw = "\
worktree /repo
HEAD abc
branch refs/heads/main

worktree /repo-worktrees/feature
HEAD def
branch refs/heads/sixeight/feature

worktree /repo-worktrees/detached
HEAD ghi
detached
";
        let list = parse_worktree_porcelain(raw);
        assert_eq!(
            list,
            vec![
                Worktree {
                    path: "/repo".into(),
                    branch: Some("main".into()),
                },
                Worktree {
                    path: "/repo-worktrees/feature".into(),
                    branch: Some("sixeight/feature".into()),
                },
                Worktree {
                    path: "/repo-worktrees/detached".into(),
                    branch: None,
                },
            ]
        );
    }

    #[test]
    fn parse_worktree_path_with_spaces_locked_and_bare() {
        let raw = "\
worktree /repo with spaces
HEAD abc
bare

worktree /locked
HEAD def
branch refs/heads/wip
locked

worktree /already-on-heads
HEAD ghi
branch main
";
        let list = parse_worktree_porcelain(raw);
        assert_eq!(
            list,
            vec![
                Worktree {
                    path: "/repo with spaces".into(),
                    branch: None,
                },
                Worktree {
                    path: "/locked".into(),
                    branch: Some("wip".into()),
                },
                Worktree {
                    path: "/already-on-heads".into(),
                    branch: Some("main".into()),
                },
            ]
        );
    }

    #[test]
    fn empty_tip_is_diverged() {
        let mut parents = HashSet::new();
        parents.insert("abc".into());
        assert!(has_branch_diverged(&parents, ""));
        assert!(!has_branch_diverged(&parents, "abc"));
        assert!(has_branch_diverged(&HashSet::new(), "abc"));
    }

    #[test]
    fn one_log_answers_shas_merges_and_subjects() {
        let raw = "\
c3\x00p2 pmain\x00Merge branch 'main' into x
c2\x00p1\x00feat: two
c1\x00p0\x00feat: one, with \x00 nothing odd
";
        let log = parse_unique_log(raw, 2);
        assert_eq!(log.shas, ["c3", "c2", "c1"]);
        assert_eq!(log.non_merge_count, 2);
        assert_eq!(log.subjects, ["Merge branch 'main' into x", "feat: two"]);
    }

    #[test]
    fn empty_log_is_no_commits() {
        let log = parse_unique_log("", 8);
        assert!(log.shas.is_empty());
        assert_eq!(log.non_merge_count, 0);
        assert!(log.subjects.is_empty());
    }

    #[test]
    fn root_commit_without_parents_is_not_a_merge() {
        let log = parse_unique_log("c1\x00\x00initial\n", 8);
        assert_eq!(log.non_merge_count, 1);
    }

    #[test]
    fn karu_config_is_read_in_one_go() {
        // `git config --get-regexp` lowercases the key and keeps the value as is
        let raw = "\
karu.keep release-*
karu.keep sixeight/long lived
karu.staledays 3
karu.staledays 14
karu.fetchmaxage 5m
";
        let config = parse_karu_config(raw);
        assert_eq!(config.keep, ["release-*", "sixeight/long lived"]);
        assert_eq!(config.stale_days, Some(14));
        assert_eq!(config.fetch_max_age_secs, Some(300));

        let empty = parse_karu_config("");
        assert!(empty.keep.is_empty());
        assert_eq!(empty.stale_days, None);
        assert_eq!(empty.fetch_max_age_secs, None);
    }

    /// The one setting that decides whether anything leaves the machine, so
    /// everything but a clear yes has to read as off.
    #[test]
    fn jev_is_on_only_when_it_is_clearly_said() {
        for on in ["true", "yes", "on", "1", "TRUE", " true "] {
            assert_eq!(parse_karu_config(&format!("karu.jev {on}")).jev, Some(true));
        }
        for off in ["false", "no", "off", "0"] {
            assert_eq!(
                parse_karu_config(&format!("karu.jev {off}")).jev,
                Some(false)
            );
        }
        // a key with no value, or one git never wrote: not an answer
        assert_eq!(parse_karu_config("karu.jev").jev, None);
        assert_eq!(parse_karu_config("karu.jev maybe").jev, None);
        assert_eq!(parse_karu_config("").jev, None);
    }

    #[test]
    fn max_age_reads_plain_seconds_and_units() {
        assert_eq!(parse_max_age("90"), Some(90));
        assert_eq!(parse_max_age("90s"), Some(90));
        assert_eq!(parse_max_age("5m"), Some(300));
        assert_eq!(parse_max_age(" 2h "), Some(7200));
        for invalid in ["", "m", "5 minutes", "-1", "1d2h"] {
            assert_eq!(parse_max_age(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn parse_branch_refs_reads_name_sha_date_subject() {
        let raw = "\
main\x00abc\x002026-09-18\x00initial
sixeight/tmp\x00def\x002026-09-10\x00wip
";
        let list = parse_branch_refs(raw);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].name, "sixeight/tmp");
        assert_eq!(list[1].sha, "def");
        assert_eq!(list[1].last_subject, "wip");
    }

    #[test]
    fn upstream_is_gone_when_its_ref_no_longer_exists() {
        let raw = "\
refs/heads/main\x00refs/remotes/origin/main
refs/heads/sixeight/gone\x00refs/remotes/origin/sixeight/gone
refs/heads/sixeight/no-upstream\x00
refs/heads/sixeight/tracks-local\x00refs/heads/main
refs/remotes/origin/main\x00
";
        let states = parse_upstream_states(raw);
        assert_eq!(states["main"], UpstreamState::Live);
        assert_eq!(states["sixeight/gone"], UpstreamState::Gone);
        assert_eq!(states["sixeight/no-upstream"], UpstreamState::None);
        assert_eq!(states["sixeight/tracks-local"], UpstreamState::Live);
        assert!(!states.contains_key("origin/main"));
    }

    #[test]
    fn parse_reflog_line_unix_timestamp() {
        let line = "0000000000000000000000000000000000000000 abcdef Test <test@example.com> 1720000000 +0900\tbranch: Created from HEAD";
        assert_eq!(parse_reflog_line_unix(line), Some(1720000000));
        assert_eq!(parse_reflog_line_unix("no timestamp here"), None);
    }
}
