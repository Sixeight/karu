use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

fn isolate_git_env(cmd: &mut Command) {
    isolate_process_git();
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR");
}

fn isolate_process_git() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_TERMINAL_PROMPT", "0");
    });
}

fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    isolate_git_env(&mut cmd);
    cmd
}

fn run_git(dir: &Path, args: &[&str]) {
    let output = git_cmd()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "ERROR: git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn setup_repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    run_git(&repo, &["init", "-b", "main"]);
    run_git(&repo, &["config", "user.email", "test@example.com"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    run_git(&repo, &["config", "commit.gpgsign", "false"]);
    fs::write(repo.join("README.md"), "# test\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "initial"]);
    (tmp, repo)
}

/// `setup_repo` plus an empty bare repository wired up as `origin`.
fn setup_repo_with_remote() -> (TempDir, PathBuf, PathBuf) {
    let (tmp, repo) = setup_repo();
    let remote = tmp.path().join("remote.git");
    run_git(
        tmp.path(),
        &["init", "--bare", "-q", remote.to_str().unwrap()],
    );
    run_git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    (tmp, repo, remote)
}

fn karu_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_karu"))
}

fn run_sei(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(karu_bin());
    isolate_git_env(&mut cmd);
    cmd.args(args)
        .current_dir(repo)
        .env_remove("TYPESAFE_API_KEY")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run karu")
}

fn json_report(repo: &Path) -> serde_json::Value {
    run_json(repo, &["--json", "--no-fetch"])
}

fn traced_git_commands(trace: &Path) -> Vec<serde_json::Value> {
    let commands: Vec<_> = fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["event"] == "start")
        .map(|event| event["argv"].clone())
        .collect();
    assert!(!commands.is_empty(), "Git tracing recorded no commands");
    commands
}

fn verdict_for<'a>(report: &'a serde_json::Value, branch: &str) -> &'a serde_json::Value {
    report
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["branch"] == branch)
        .unwrap_or_else(|| panic!("missing branch {branch} in {report}"))
}

#[test]
fn unborn_repo_exits_successfully_without_fetching() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    run_git(&repo, &["init", "-b", "main"]);
    run_git(&repo, &["remote", "add", "origin", "../missing.git"]);
    fs::write(repo.join("untracked.txt"), "keep me\n").unwrap();

    for bin in [karu_bin(), PathBuf::from(env!("CARGO_BIN_EXE_git-karu"))] {
        for args in [vec![], vec!["--json"], vec!["--yes", "--force"]] {
            let trace = tmp.path().join("git-trace.json");
            fs::write(&trace, "").unwrap();
            let mut cmd = Command::new(&bin);
            isolate_git_env(&mut cmd);
            let output = cmd
                .args(&args)
                .current_dir(&repo)
                .env("GIT_TRACE2_EVENT", &trace)
                .env_remove("TYPESAFE_API_KEY")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{bin:?} {args:?}: {stderr}");
            assert!(stderr.contains("no commits"), "{stderr}");
            assert!(!stderr.contains("Error:"), "{stderr}");
            if args.contains(&"--json") {
                let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(report, serde_json::json!([]));
            } else {
                assert!(output.stdout.is_empty());
            }
            assert!(
                traced_git_commands(&trace)
                    .iter()
                    .all(|argv| { !argv.as_array().unwrap().iter().any(|arg| arg == "fetch") }),
                "an unborn branch should not fetch"
            );
            assert_eq!(
                fs::read_to_string(repo.join("untracked.txt")).unwrap(),
                "keep me\n"
            );
        }
    }
}

#[test]
fn unborn_branch_with_existing_history_is_a_successful_noop() {
    let (_tmp, repo) = merged_feature_repo();
    run_git(&repo, &["checkout", "--orphan", "docs"]);
    let branches = branch_names(&repo);
    let output = run_sei(&repo, &["--yes", "--no-fetch"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("current branch has no commits"), "{stderr}");
    assert!(!stderr.contains("Error:"), "{stderr}");
    assert_eq!(branch_names(&repo), branches);
}

#[test]
fn broken_head_is_still_an_error() {
    for symbolic in [false, true] {
        let (_tmp, repo) = setup_repo();
        let path = if symbolic {
            ".git/refs/heads/main"
        } else {
            ".git/HEAD"
        };
        fs::write(
            repo.join(path),
            "1111111111111111111111111111111111111111\n",
        )
        .unwrap();
        let output = run_sei(&repo, &["--json", "--no-fetch"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("Error:"), "{stderr}");
        assert!(!stderr.contains("no commits"), "{stderr}");
    }
}

#[test]
fn outside_a_repository_is_still_an_error() {
    let tmp = TempDir::new().unwrap();
    let output = run_sei(tmp.path(), &["--json", "--no-fetch"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("Error:"), "{stderr}");
    assert!(stderr.contains("not a git repository"), "{stderr}");
}

#[test]
fn nothing_to_delete_exits_without_an_error() {
    let (_tmp, repo) = setup_repo();
    let output = run_sei(&repo, &["--no-fetch"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("nothing to delete"), "{stderr}");
    assert!(!stderr.contains("Error:"), "{stderr}");
}

#[test]
fn merged_branch_is_hard_delete() {
    let (_tmp, repo) = merged_feature_repo();

    let report = json_report(&repo);
    let item = verdict_for(&report, "feature");
    assert_eq!(item["verdict"], "delete");
    assert_eq!(item["source"], "hard");
    assert_eq!(item["reason"], "merged");
}

#[test]
fn squash_merged_branch_is_not_hard_deleted() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "feature"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add a"]);
    run_git(&repo, &["checkout", "main"]);
    run_git(&repo, &["merge", "--squash", "feature"]);
    run_git(&repo, &["commit", "-m", "squash feature"]);

    let report = json_report(&repo);
    let item = verdict_for(&report, "feature");
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["source"], "jev");
}

#[test]
fn unchanged_branch_is_kept() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["branch", "untouched"]);

    let report = json_report(&repo);
    let item = verdict_for(&report, "untouched");
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["source"], "jev");
    assert_eq!(item["reason"], "jev skipped: no API key");
}

fn set_reflog_age(repo: &Path, branch: &str, age_secs: u64) {
    let path = repo.join(".git/logs/refs/heads").join(branch);
    let raw = fs::read_to_string(&path).unwrap();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(age_secs)
        .to_string();
    let rewritten = raw
        .lines()
        .map(|line| {
            let Some(idx) = line.find("> ") else {
                return line.to_string();
            };
            let prefix = &line[..idx + 2];
            let rest = &line[idx + 2..];
            let Some((_, after)) = rest.split_once(' ') else {
                return line.to_string();
            };
            format!("{prefix}{ts} {after}")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(path, rewritten).unwrap();
}

#[test]
fn unchanged_stale_branch_goes_to_jev() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["branch", "untouched"]);
    set_reflog_age(&repo, "untouched", 24 * 60 * 60);

    let report = json_report(&repo);
    let item = verdict_for(&report, "untouched");
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["source"], "jev");
    assert_eq!(item["reason"], "jev skipped: no API key");
}

#[test]
fn long_idle_branch_is_asked_unless_configured_off() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "sixeight/forgotten"]);
    fs::write(repo.join("f.txt"), "f\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "feat: half done"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    set_reflog_age(&repo, "sixeight/forgotten", 8 * 24 * 60 * 60);

    let item = verdict_for(&json_report(&repo), "sixeight/forgotten").clone();
    assert_eq!(item["verdict"], "ask");
    assert_eq!(item["source"], "hard");
    assert_eq!(item["reason"], "idle for a long time");

    run_git(&repo, &["config", "karu.staleDays", "30"]);
    let item = verdict_for(&json_report(&repo), "sixeight/forgotten").clone();
    assert_eq!(item["verdict"], "keep");

    run_git(&repo, &["config", "karu.staleDays", "0"]);
    let item = verdict_for(&json_report(&repo), "sixeight/forgotten").clone();
    assert_eq!(item["verdict"], "keep");
}

#[test]
fn default_and_head_are_kept() {
    let (_tmp, repo) = setup_repo();
    let report = json_report(&repo);
    let item = verdict_for(&report, "main");
    assert_eq!(item["verdict"], "keep");
    let reason = item["reason"].as_str().unwrap();
    assert!(reason == "current HEAD" || reason == "default branch");
}

#[test]
fn leftover_without_api_key_is_kept() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "sixeight/tmp-spike"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "wip"]);
    run_git(&repo, &["checkout", "main"]);

    let report = json_report(&repo);
    let item = verdict_for(&report, "sixeight/tmp-spike");
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["source"], "jev");
    assert_eq!(item["reason"], "jev skipped: no API key");
}

#[test]
fn dirty_worktree_is_asked_not_deleted() {
    let (tmp, repo) = merged_feature_repo();

    let wt = tmp.path().join("feature-wt");
    run_git(&repo, &["worktree", "add", wt.to_str().unwrap(), "feature"]);
    fs::write(wt.join("dirty.txt"), "nope\n").unwrap();

    let report = json_report(&repo);
    let item = verdict_for(&report, "feature");
    assert_eq!(item["verdict"], "ask");
    assert_eq!(item["reason"], "merged; worktree has uncommitted changes");

    // asked, never deleted on its own: --yes only skips the bulk confirmation
    let output = run_sei(&repo, &["--yes", "--no-fetch"]);
    assert!(output.status.success());
    assert!(
        wt.join("dirty.txt").exists(),
        "ERROR: dirty worktree was removed"
    );
}

#[test]
fn apply_deletes_merged_branch_and_worktree() {
    let (tmp, repo) = merged_feature_repo();

    let wt = tmp.path().join("feature-wt");
    run_git(&repo, &["worktree", "add", wt.to_str().unwrap(), "feature"]);

    let output = run_sei(&repo, &["--yes", "--no-fetch"]);
    assert!(
        output.status.success(),
        "ERROR: karu apply failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!wt.exists(), "ERROR: worktree still present");

    let names = branch_names(&repo);
    assert!(
        !names.lines().any(|b| b == "feature"),
        "ERROR: feature branch still present: {names}"
    );
}

#[test]
fn empty_commit_branch_is_not_hard_deleted() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "empty-only"]);
    run_git(&repo, &["commit", "--allow-empty", "-m", "empty"]);
    run_git(&repo, &["checkout", "main"]);

    let report = json_report(&repo);
    let item = verdict_for(&report, "empty-only");
    assert_ne!(
        item["reason"], "squash-merged",
        "ERROR: empty unique commit treated as squash-merged: {item}"
    );
    assert_eq!(item["verdict"], "keep");
}

#[test]
fn commit_and_revert_is_not_hard_deleted() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "explored"]);
    fs::write(repo.join("z.txt"), "z\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add z"]);
    run_git(&repo, &["rm", "z.txt"]);
    run_git(&repo, &["commit", "-m", "revert z"]);
    run_git(&repo, &["checkout", "main"]);

    let report = json_report(&repo);
    let item = verdict_for(&report, "explored");
    assert_ne!(
        item["reason"], "squash-merged",
        "ERROR: reverted unique work treated as squash-merged: {item}"
    );
    assert_eq!(item["verdict"], "keep");
}

#[test]
fn keep_glob_prevents_delete_of_merged_branch() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "release-1"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add a"]);
    run_git(&repo, &["checkout", "main"]);
    run_git(
        &repo,
        &["merge", "--no-ff", "-m", "merge release-1", "release-1"],
    );
    run_git(&repo, &["config", "--add", "karu.keep", "release-*"]);

    let report = json_report(&repo);
    let item = verdict_for(&report, "release-1");
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["reason"], "karu.keep");
}

#[test]
fn json_does_not_delete_merged_branch() {
    let (_tmp, repo) = merged_feature_repo();

    let _ = json_report(&repo);
    let names = branch_names(&repo);
    assert!(
        names.lines().any(|b| b == "feature"),
        "ERROR: json mode deleted feature: {names}"
    );
}

#[test]
fn answering_no_deletes_nothing() {
    let (_tmp, repo) = merged_feature_repo();

    let output = run_with_input(&repo, "n\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "ERROR: answering no should exit 0: {stderr}"
    );
    assert!(
        stderr.contains("Proceed? [y/N]"),
        "ERROR: confirmation was not asked: {stderr}"
    );

    let names = branch_names(&repo);
    assert!(
        names.lines().any(|b| b == "feature"),
        "ERROR: aborted apply deleted feature: {names}"
    );
}

#[test]
fn capital_yes_deletes_nothing() {
    let (_tmp, repo) = merged_feature_repo();

    let output = run_with_input(&repo, "YES\n");
    assert!(output.status.success());

    let names = branch_names(&repo);
    assert!(
        names.lines().any(|b| b == "feature"),
        "ERROR: YES confirmation deleted feature: {names}"
    );
}

#[test]
fn force_deletes_dirty_merged_worktree() {
    let (tmp, repo) = merged_feature_repo();

    let wt = tmp.path().join("feature-wt");
    run_git(&repo, &["worktree", "add", wt.to_str().unwrap(), "feature"]);
    fs::write(wt.join("dirty.txt"), "nope\n").unwrap();

    let output = run_sei(&repo, &["--yes", "--no-fetch", "--force"]);
    assert!(
        output.status.success(),
        "ERROR: karu apply --force failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!wt.exists(), "ERROR: dirty worktree still present");
}

#[test]
fn detached_worktree_without_unique_commits_is_kept() {
    let (tmp, repo) = setup_repo();
    let wt = tmp.path().join("detached-wt");
    let head = git_cmd()
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    run_git(
        &repo,
        &["worktree", "add", "--detach", wt.to_str().unwrap(), &sha],
    );

    let report = json_report(&repo);
    let item = report
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["branch"].is_null())
        .unwrap_or_else(|| panic!("missing detached worktree in {report}"));
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["source"], "jev");
    assert_eq!(item["reason"], "jev skipped: no API key");
    assert!(wt.exists(), "ERROR: parked detached worktree was removed");
}

#[test]
fn detached_worktree_with_unique_commits_is_kept_without_api_key() {
    let (tmp, repo) = setup_repo();
    let wt = tmp.path().join("detached-wt");
    run_git(
        &repo,
        &["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"],
    );
    fs::write(wt.join("solo.txt"), "solo\n").unwrap();
    run_git(&wt, &["add", "."]);
    run_git(&wt, &["commit", "-m", "solo"]);

    let report = json_report(&repo);
    let canonical = wt.canonicalize().unwrap_or(wt.clone());
    let item = report
        .as_array()
        .unwrap()
        .iter()
        .find(|item| {
            item["worktree"]
                .as_str()
                .is_some_and(|p| Path::new(p) == canonical || Path::new(p) == wt)
        })
        .unwrap_or_else(|| panic!("missing detached worktree in {report}"));
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["source"], "jev");
    assert!(wt.exists(), "ERROR: unique detached worktree was removed");
}

#[test]
fn slash_named_merged_branch_is_deleted() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "sixeight/landed"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add a"]);
    run_git(&repo, &["checkout", "main"]);
    run_git(
        &repo,
        &["merge", "--no-ff", "-m", "merge", "sixeight/landed"],
    );

    let output = run_sei(&repo, &["--yes", "--no-fetch"]);
    assert!(
        output.status.success(),
        "ERROR: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let names = branch_names(&repo);
    assert!(
        !names.lines().any(|b| b == "sixeight/landed"),
        "ERROR: slash branch still present: {names}"
    );
}

fn repo_with_pruned_remote_branch() -> (TempDir, PathBuf) {
    let (tmp, repo, remote) = setup_repo_with_remote();
    run_git(&repo, &["branch", "topic"]);
    run_git(&repo, &["push", "-q", "origin", "main", "topic"]);
    run_git(&repo, &["fetch", "-q", "origin"]);
    run_git(&remote, &["branch", "-D", "topic"]);
    (tmp, repo)
}

fn has_remote_ref(repo: &Path, name: &str) -> bool {
    git_cmd()
        .args(["rev-parse", "--verify", "-q", name])
        .current_dir(repo)
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn json_reports_commits_that_exist_only_locally() {
    let (_tmp, repo, _remote) = setup_repo_with_remote();
    for branch in ["pushed", "unpushed"] {
        run_git(&repo, &["checkout", "-q", "-b", branch, "main"]);
        fs::write(repo.join(format!("{branch}.txt")), "x\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-q", "-m", branch]);
    }
    run_git(&repo, &["checkout", "-q", "main"]);
    run_git(&repo, &["push", "-q", "origin", "main", "pushed"]);

    let report = json_report(&repo);
    assert_eq!(verdict_for(&report, "pushed")["local_only"], 0);
    assert_eq!(verdict_for(&report, "unpushed")["local_only"], 1);
}

#[test]
fn commits_fetched_into_any_ref_are_not_local_only() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "review/pr-1", "main"]);
    fs::write(repo.join("r.txt"), "r\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "someone else's work"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    // what a review tool leaves behind after fetching pull/1/head
    run_git(&repo, &["update-ref", "refs/rui/pull/1", "review/pr-1"]);

    let report = json_report(&repo);
    assert_eq!(verdict_for(&report, "review/pr-1")["local_only"], 0);
}

#[test]
fn locally_made_refs_and_worktree_heads_do_not_hide_local_commits() {
    let (tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "sixeight/mine", "main"]);
    fs::write(repo.join("m.txt"), "m\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "my own work"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    run_git(&repo, &["update-ref", "refs/backup/mine", "sixeight/mine"]);
    let wt = tmp.path().join("detached-wt");
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            wt.to_str().unwrap(),
            "sixeight/mine",
        ],
    );

    let report = json_report(&repo);
    assert_eq!(verdict_for(&report, "sixeight/mine")["local_only"], 1);
}

#[test]
fn merged_branch_is_deleted_even_when_head_lacks_the_merge() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["branch", "behind"]);
    run_git(&repo, &["checkout", "-q", "-b", "feature"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "add a"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    run_git(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );
    // `git branch -d` judges against HEAD, which does not contain the merge
    run_git(&repo, &["checkout", "-q", "behind"]);

    let output = run_sei(&repo, &["--yes", "--no-fetch"]);
    assert!(
        output.status.success(),
        "ERROR: karu failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let names = branch_names(&repo);
    assert!(
        !names.lines().any(|b| b == "feature"),
        "ERROR: merged feature branch still present: {names}"
    );
}

/// A deletion is only undoable if the user is told which commit to restore.
#[test]
fn deleted_branch_is_reported_with_its_tip() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "feature"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "add a"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    let sha = String::from_utf8(
        git_cmd()
            .args(["rev-parse", "feature"])
            .current_dir(&repo)
            .output()
            .expect("failed to run git")
            .stdout,
    )
    .unwrap();
    let short = sha.trim()[..7].to_string();
    run_git(&repo, &["merge", "-q", "--no-ff", "-m", "merge", "feature"]);

    let output = run_sei(&repo, &["--yes", "--no-fetch"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("Deleted feature ({short})")),
        "ERROR: tip missing from the deletion line: {stderr}"
    );
}

#[test]
fn ordinary_commit_titled_merge_is_not_deleted() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "sixeight/sorting"]);
    fs::write(repo.join("sort.txt"), "s\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "Merge sort implementation"]);
    run_git(&repo, &["checkout", "-q", "main"]);

    let item = verdict_for(&json_report(&repo), "sixeight/sorting").clone();
    assert_ne!(item["verdict"], "delete", "ERROR: {item}");
}

fn forgotten_branch_repo() -> (TempDir, PathBuf) {
    let (tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "sixeight/forgotten"]);
    fs::write(repo.join("f.txt"), "f\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "feat: half done"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    set_reflog_age(&repo, "sixeight/forgotten", 30 * 24 * 60 * 60);
    (tmp, repo)
}

fn run_with_input(repo: &Path, input: &str) -> std::process::Output {
    run_with_args_and_input(repo, &["--no-fetch"], input)
}

fn run_with_args_and_input(repo: &Path, args: &[&str], input: &str) -> std::process::Output {
    use std::io::Write;
    let mut cmd = Command::new(karu_bin());
    isolate_git_env(&mut cmd);
    let mut child = cmd
        .args(args)
        .current_dir(repo)
        .env_remove("TYPESAFE_API_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn no_jev_json_skips_diffstat_for_an_asked_branch() {
    let (tmp, repo) = forgotten_branch_repo();
    let trace = tmp.path().join("git-trace.jsonl");
    let mut cmd = Command::new(karu_bin());
    isolate_git_env(&mut cmd);
    let output = cmd
        .args(["--json", "--no-fetch", "--no-jev"])
        .current_dir(&repo)
        .env("GIT_TRACE2_EVENT", &trace)
        .env_remove("TYPESAFE_API_KEY")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verdict_for(&report, "sixeight/forgotten")["verdict"], "ask");
    let commands = traced_git_commands(&trace);
    assert!(
        commands
            .iter()
            .all(|args| !args.as_array().unwrap().iter().any(|arg| arg == "diff")),
        "JSON without Jev does not need a diffstat: {commands:?}"
    );
}

#[test]
fn no_jev_asked_branch_keeps_the_diffstat_in_its_card() {
    let (_tmp, repo) = forgotten_branch_repo();
    let output = run_with_args_and_input(&repo, &["--no-fetch", "--no-jev"], "n\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("[1/1] sixeight/forgotten"), "{stderr}");
    assert!(stderr.contains("1 commit · 1 file +1"), "{stderr}");
    assert!(
        branch_names(&repo)
            .lines()
            .any(|branch| branch == "sixeight/forgotten")
    );
}

#[test]
fn asked_branch_shows_a_card_and_can_be_inspected_before_deleting() {
    let (_tmp, repo) = forgotten_branch_repo();
    let output = run_with_input(&repo, "l\ny\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "ERROR: karu failed: {stderr}");
    assert!(stderr.contains("[1/1] sixeight/forgotten"), "{stderr}");
    assert!(stderr.contains("would lose: 1 unpushed commit"), "{stderr}");
    assert!(
        stderr.contains("feat: half done") && stdout.is_empty(),
        "ERROR: [l]og should show the commits next to the prompt: {stderr} / {stdout}"
    );
    assert!(
        !branch_names(&repo)
            .lines()
            .any(|b| b == "sixeight/forgotten"),
        "ERROR: confirmed branch still present"
    );
}

#[test]
fn declining_the_bulk_delete_still_walks_through_the_asked_ones() {
    let (_tmp, repo) = forgotten_branch_repo();
    run_git(&repo, &["checkout", "-q", "-b", "feature"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "add a"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    run_git(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );

    // "n" to the bulk delete of `feature`, then "y" to the asked branch
    let output = run_with_input(&repo, "n\ny\n");
    assert!(output.status.success());
    let names = branch_names(&repo);
    assert!(
        names.lines().any(|b| b == "feature"),
        "ERROR: declined bulk delete was carried out: {names}"
    );
    assert!(
        !names.lines().any(|b| b == "sixeight/forgotten"),
        "ERROR: confirmed ask was not deleted: {names}"
    );
}

#[test]
fn questions_come_in_the_order_the_table_shows() {
    let (_tmp, repo) = setup_repo();
    // created in name order, so without sorting `aaa-newer` would be asked first
    for (branch, days) in [("sixeight/aaa-newer", 10), ("sixeight/zzz-older", 40)] {
        run_git(&repo, &["checkout", "-q", "-b", branch, "main"]);
        fs::write(repo.join(format!("{days}.txt")), "x\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-q", "-m", "feat: half done"]);
        run_git(&repo, &["checkout", "-q", "main"]);
        set_reflog_age(&repo, branch, days * 24 * 60 * 60);
    }

    let output = run_with_input(&repo, "n\nq\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let table_older = stderr.find("sixeight/zzz-older").expect("older in table");
    let table_newer = stderr.find("sixeight/aaa-newer").expect("newer in table");
    assert!(table_older < table_newer, "ERROR: table order: {stderr}");
    assert!(
        stderr.contains("[1/2] sixeight/zzz-older"),
        "ERROR: {stderr}"
    );
    assert!(
        stderr.contains("[2/2] sixeight/aaa-newer"),
        "ERROR: {stderr}"
    );
}

#[test]
fn looking_without_confirming_deletes_nothing() {
    let (_tmp, repo) = forgotten_branch_repo();
    for input in ["l\nd\n", "n\n", "q\n", "YES\n"] {
        let output = run_with_input(&repo, input);
        assert!(output.status.success());
        assert!(
            branch_names(&repo)
                .lines()
                .any(|b| b == "sixeight/forgotten"),
            "ERROR: {input:?} deleted the branch"
        );
    }
}

#[test]
fn yes_flag_never_deletes_what_is_only_asked() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "sixeight/forgotten"]);
    fs::write(repo.join("f.txt"), "f\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "feat: half done"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    set_reflog_age(&repo, "sixeight/forgotten", 30 * 24 * 60 * 60);

    let output = run_sei(&repo, &["--yes", "--no-fetch"]);
    assert!(output.status.success());
    let names = branch_names(&repo);
    assert!(
        names.lines().any(|b| b == "sixeight/forgotten"),
        "ERROR: --yes deleted an asked branch: {names}"
    );
}

#[test]
fn facts_about_the_remote_are_read_after_the_fetch() {
    let (_tmp, repo, remote) = setup_repo_with_remote();
    run_git(&repo, &["checkout", "-q", "-b", "topic", "main"]);
    fs::write(repo.join("t.txt"), "t\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "topic work"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    run_git(&repo, &["push", "-q", "-u", "origin", "main", "topic"]);
    // the remote branch disappears; the local tracking ref is now out of date
    run_git(&remote, &["branch", "-D", "topic"]);

    let stale = verdict_for(&run_json(&repo, &["--json", "--no-fetch"]), "topic").clone();
    assert_eq!(
        stale["local_only"], 0,
        "without a fetch the old ref still counts"
    );

    let fresh = verdict_for(&run_json(&repo, &["--json"]), "topic").clone();
    assert_eq!(
        fresh["local_only"], 1,
        "ERROR: after the prune no remote holds the commit: {fresh}"
    );
}

fn run_json(repo: &Path, args: &[&str]) -> serde_json::Value {
    let output = run_sei(repo, args);
    assert!(
        output.status.success(),
        "ERROR: karu failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json")
}

#[test]
fn recent_fetch_is_reused_only_when_configured() {
    let (_tmp, repo) = repo_with_pruned_remote_branch();
    // this fetch leaves a fresh FETCH_HEAD without pruning anything, whatever
    // `fetch.prune` says in the config of whoever runs the tests
    run_git(&repo, &["-c", "fetch.prune=false", "fetch", "-q", "origin"]);
    assert!(has_remote_ref(&repo, "refs/remotes/origin/topic"));

    run_git(&repo, &["config", "karu.fetchMaxAge", "10m"]);
    let output = run_sei(&repo, &["--json"]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        has_remote_ref(&repo, "refs/remotes/origin/topic"),
        "ERROR: fetched although the last fetch was recent: {stderr}"
    );
    assert!(stderr.contains("fetch skipped"), "ERROR: {stderr}");

    run_git(&repo, &["config", "--unset", "karu.fetchMaxAge"]);
    run_sei(&repo, &["--json"]);
    assert!(
        !has_remote_ref(&repo, "refs/remotes/origin/topic"),
        "ERROR: without the setting every run fetches"
    );
}

fn wait_for_trash_to_go(parent: &Path) -> Vec<String> {
    let trash = || -> Vec<String> {
        fs::read_dir(parent)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".karu-trash-"))
            .collect()
    };
    for _ in 0..100 {
        if trash().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    trash()
}

#[test]
fn worktree_files_are_deleted_after_karu_returns() {
    let (tmp, repo) = merged_feature_repo();
    let wt = tmp.path().join("feature-wt");
    run_git(
        &repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "feature"],
    );
    fs::create_dir(wt.join("build")).unwrap();
    fs::write(wt.join("build/out.bin"), "x").unwrap();
    fs::write(repo.join(".git/info/exclude"), "build/\n").unwrap();

    let output = run_sei(&repo, &["--yes", "--no-fetch"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "ERROR: {stderr}");
    assert!(!wt.exists(), "ERROR: worktree path is still taken");
    assert!(
        !branch_names(&repo).lines().any(|b| b == "feature"),
        "ERROR: branch still present"
    );
    assert!(stderr.contains("in the background"), "ERROR: {stderr}");
    assert_eq!(wait_for_trash_to_go(tmp.path()), Vec::<String>::new());
}

#[test]
fn json_never_sweeps_but_a_normal_run_does() {
    let (tmp, repo) = merged_feature_repo();
    let wt = tmp.path().join("feature-wt");
    run_git(
        &repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "feature"],
    );
    let leftover = tmp.path().join(".karu-trash-1-0-old");
    fs::create_dir_all(leftover.join("deep")).unwrap();

    json_report(&repo);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(leftover.exists(), "ERROR: --json deleted something");

    run_sei(&repo, &["--no-fetch"]);
    assert_eq!(wait_for_trash_to_go(tmp.path()), Vec::<String>::new());
    assert!(wt.exists(), "ERROR: declined deletion removed the worktree");
}

#[test]
fn branch_of_a_worktree_deleted_by_hand_is_deleted() {
    let (tmp, repo) = merged_feature_repo();
    let wt = tmp.path().join("feature-wt");
    run_git(
        &repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "feature"],
    );
    fs::remove_dir_all(&wt).unwrap();

    let item = verdict_for(&json_report(&repo), "feature").clone();
    assert_eq!(
        item["verdict"], "delete",
        "ERROR: a missing worktree is not dirty: {item}"
    );

    let output = run_sei(&repo, &["--yes", "--no-fetch"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "ERROR: {stderr}");
    assert!(
        !branch_names(&repo).lines().any(|b| b == "feature"),
        "ERROR: branch survived: {stderr}"
    );
    let worktrees = git_cmd()
        .args(["worktree", "list"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&worktrees.stdout).lines().count(),
        1,
        "ERROR: the stale registration is still there"
    );
}

#[test]
fn fetches_and_prunes_by_default() {
    let (_tmp, repo) = repo_with_pruned_remote_branch();
    let output = run_sei(&repo, &["--json"]);
    assert!(
        output.status.success(),
        "ERROR: karu failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !has_remote_ref(&repo, "refs/remotes/origin/topic"),
        "ERROR: default run did not prune"
    );
}

#[test]
fn no_fetch_leaves_remote_refs_alone() {
    let (_tmp, repo) = repo_with_pruned_remote_branch();
    let output = run_sei(&repo, &["--json", "--no-fetch"]);
    assert!(output.status.success());
    assert!(
        has_remote_ref(&repo, "refs/remotes/origin/topic"),
        "ERROR: fetched despite --no-fetch"
    );
}

/// A key in the environment is not by itself a choice about karu: without
/// `karu.jev` the repository's branch names stay on the machine.
#[test]
fn a_key_alone_sends_nothing() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "sixeight/tmp-spike"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "wip"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    let jev = FakeJev::start(0.95);

    let mut cmd = Command::new(karu_bin());
    isolate_git_env(&mut cmd);
    let output = cmd
        .args(["--json", "--no-fetch"])
        .current_dir(&repo)
        .env("TYPESAFE_API_KEY", "test-key")
        .env("TYPESAFE_BASE_URL", &jev.url)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());

    assert!(
        jev.asked().is_empty(),
        "ERROR: sent without karu.jev: {:?}",
        jev.asked()
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let item = verdict_for(&report, "sixeight/tmp-spike");
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["reason"], "jev skipped: not enabled");
}

/// `--no-jev` has to beat everything that would otherwise send: the key in
/// the environment and the repository that opted in.
#[test]
fn no_jev_beats_the_opted_in_repository() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "sixeight/tmp-spike"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "wip"]);
    run_git(&repo, &["checkout", "-q", "main"]);
    let jev = FakeJev::start(0.95);

    // FakeJev::run turns karu.jev on, so the flag is the only thing stopping it
    let report = jev.run(&repo, &["--json", "--no-fetch", "--no-jev"]);

    assert!(
        jev.asked().is_empty(),
        "ERROR: sent despite --no-jev: {:?}",
        jev.asked()
    );
    let item = verdict_for(&report, "sixeight/tmp-spike");
    assert_eq!(item["verdict"], "keep");
}

/// A stand-in for the Jev API: answers every question with `noul`, and keeps
/// the bodies it was sent so tests can count and inspect the requests.
struct FakeJev {
    url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl FakeJev {
    fn start(noul: f64) -> FakeJev {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).ok();
                let body = String::from_utf8_lossy(&body).into_owned();
                let request: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let answers: serde_json::Map<String, serde_json::Value> = request["questions"]
                    .as_object()
                    .map(|questions| {
                        questions
                            .keys()
                            .map(|key| {
                                (
                                    key.clone(),
                                    serde_json::json!({"type": "noul", "noul": noul}),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                seen.lock().unwrap().push(body);
                let reply =
                    serde_json::json!({"model": "jev-latest", "answers": answers}).to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        FakeJev { url, requests }
    }

    /// Branch names of the candidates in each request, in the order received.
    fn asked(&self) -> Vec<Vec<String>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|body| {
                let request: serde_json::Value = serde_json::from_str(body).unwrap();
                request["state"]["candidates"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["branch"].as_str().unwrap_or("").to_string())
                    .collect()
            })
            .collect()
    }

    fn run(&self, repo: &Path, args: &[&str]) -> serde_json::Value {
        // asking Jev at all is opt-in per repository
        run_git(repo, &["config", "karu.jev", "true"]);
        let mut cmd = Command::new(karu_bin());
        isolate_git_env(&mut cmd);
        let output = cmd
            .args(args)
            .current_dir(repo)
            .env("TYPESAFE_API_KEY", "test-key")
            .env("TYPESAFE_BASE_URL", &self.url)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ERROR: karu failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("json")
    }
}

fn accept_jev_request(
    listener: &std::net::TcpListener,
) -> anyhow::Result<(std::net::TcpStream, serde_json::Value)> {
    use std::io::{BufRead, BufReader, Read};
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                anyhow::ensure!(Instant::now() < deadline, "Jev request did not arrive");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error.into()),
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut length = None;
    loop {
        let mut line = String::new();
        anyhow::ensure!(reader.read_line(&mut line)? > 0, "incomplete HTTP headers");
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let mut body = vec![0; length.ok_or_else(|| anyhow::anyhow!("missing Content-Length"))?];
    reader.read_exact(&mut body)?;
    Ok((stream, serde_json::from_slice(&body)?))
}

fn reply_to_jev(
    mut stream: std::net::TcpStream,
    request: &serde_json::Value,
    noul: f64,
) -> std::io::Result<()> {
    use std::io::Write;

    let answers: serde_json::Map<String, serde_json::Value> = request["questions"]
        .as_object()
        .unwrap()
        .keys()
        .map(|key| {
            (
                key.clone(),
                serde_json::json!({"type": "noul", "noul": noul}),
            )
        })
        .collect();
    let reply = serde_json::json!({"model": "jev-latest", "answers": answers}).to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    )
}

fn wait_for_karu(mut child: std::process::Child) -> std::process::Output {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "karu did not finish: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `main` and two branches with real work. `topic` is pushed and tracked;
/// `other` never left this machine.
fn repo_with_two_jev_candidates() -> (TempDir, PathBuf, PathBuf) {
    let (tmp, repo, remote) = setup_repo_with_remote();
    for branch in ["topic", "other"] {
        run_git(&repo, &["checkout", "-q", "-b", branch, "main"]);
        fs::write(repo.join(format!("{branch}.txt")), "x\n").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-q", "-m", "feat: real work"]);
    }
    run_git(&repo, &["checkout", "-q", "main"]);
    run_git(&repo, &["push", "-q", "-u", "origin", "main", "topic"]);
    // A local remote answers in milliseconds; a real one takes a second or
    // more, which is what lets Jev be asked before the fetch is done.
    run_git(
        &repo,
        &[
            "config",
            "remote.origin.uploadpack",
            "sleep 1; git-upload-pack",
        ],
    );
    (tmp, repo, remote)
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

#[test]
fn jev_is_asked_once_without_a_fetch() {
    let (_tmp, repo, _remote) = repo_with_two_jev_candidates();
    let jev = FakeJev::start(0.1);
    let report = jev.run(&repo, &["--json", "--no-fetch"]);
    let asked = jev.asked();
    assert_eq!(asked.len(), 1, "ERROR: requests: {asked:?}");
    assert_eq!(sorted(asked[0].clone()), ["other", "topic"]);
    assert_eq!(verdict_for(&report, "topic")["source"], "jev");
}

#[test]
fn jev_receives_the_diffstat_for_its_candidates() {
    let (_tmp, repo, _remote) = repo_with_two_jev_candidates();
    let jev = FakeJev::start(0.1);
    jev.run(&repo, &["--json", "--no-fetch"]);
    let requests = jev.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
    let candidates = request["state"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    for candidate in candidates {
        assert_eq!(
            candidate["diffstat"], "1 file changed, 1 insertion(+)",
            "{candidate}"
        );
    }
}

#[test]
fn open_pr_skips_diffstat_while_jev_candidates_receive_it() {
    use karu::candidate::{PrInfo, PrState};
    use karu::{collect, decide, git};

    let (_tmp, repo, _remote) = repo_with_two_jev_candidates();
    let mut collected = collect::local_facts(&repo, &git::KaruConfig::default()).unwrap();
    assert!(collected.candidates.iter().all(|c| c.diffstat.is_none()));
    collected.attach_prs(Some(collect::PrMap::from([(
        "topic".into(),
        PrInfo {
            state: PrState::Open,
            title: "Topic work".into(),
            merged_at: None,
            head_sha: None,
            number: Some(1),
        },
    )])));
    collected
        .fill_diffstats(|_, c| decide::without_jev(c, false, decide::STALE_AFTER_SECS).is_none());

    let topic = collected
        .candidates
        .iter()
        .find(|c| c.branch.as_deref() == Some("topic"))
        .unwrap();
    assert!(topic.has_open_pr());
    assert_eq!(topic.diffstat, None);
    let other = collected
        .candidates
        .iter()
        .find(|c| c.branch.as_deref() == Some("other"))
        .unwrap();
    assert_eq!(
        other.diffstat.as_deref(),
        Some("1 file changed, 1 insertion(+)")
    );
}

#[test]
fn empty_diffstat_is_read_once_across_jev_phases() {
    let (tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-q", "-b", "topic"]);
    run_git(
        &repo,
        &["commit", "--allow-empty", "-q", "-m", "checkpoint"],
    );
    run_git(&repo, &["checkout", "-q", "main"]);
    run_git(&repo, &["config", "karu.jev", "true"]);
    let jev = FakeJev::start(0.1);
    let trace = tmp.path().join("git-trace.jsonl");
    let mut cmd = Command::new(karu_bin());
    isolate_git_env(&mut cmd);
    let output = cmd
        .args(["--json", "--no-fetch"])
        .current_dir(&repo)
        .env("TYPESAFE_API_KEY", "test-key")
        .env("TYPESAFE_BASE_URL", &jev.url)
        .env("GIT_TRACE2_EVENT", &trace)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = jev.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
    let candidates = request["state"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["branch"], "topic");
    assert_eq!(candidates[0]["unique_commit_count"], 1);
    assert_eq!(candidates[0]["diffstat"], serde_json::Value::Null);
    let commands = traced_git_commands(&trace);
    let diffs: Vec<_> = commands
        .iter()
        .filter(|args| args.as_array().unwrap().iter().any(|arg| arg == "diff"))
        .collect();
    assert_eq!(diffs.len(), 1, "empty diffstats must be cached: {diffs:?}");
}

#[test]
fn jev_is_asked_once_when_the_fetch_changes_nothing() {
    let (_tmp, repo, _remote) = repo_with_two_jev_candidates();
    let jev = FakeJev::start(0.1);
    jev.run(&repo, &["--json"]);
    let asked = jev.asked();
    assert_eq!(asked.len(), 1, "ERROR: requests: {asked:?}");
}

#[test]
fn only_what_the_fetch_changed_is_asked_again() {
    let (_tmp, repo, remote) = repo_with_two_jev_candidates();
    // the remote branch disappears; only a fetch can tell
    run_git(&remote, &["branch", "-D", "topic"]);

    let jev = FakeJev::start(0.1);
    let report = jev.run(&repo, &["--json"]);
    let asked = jev.asked();
    assert_eq!(asked.len(), 2, "ERROR: requests: {asked:?}");
    assert_eq!(sorted(asked[0].clone()), ["other", "topic"]);
    assert_eq!(
        asked[1],
        ["topic"],
        "ERROR: only the changed branch is re-asked"
    );

    // and the verdict rests on the facts from after the fetch
    assert_eq!(verdict_for(&report, "topic")["local_only"], 1);
    assert_eq!(
        verdict_for(&report, "topic")["reason"],
        "commits exist only locally"
    );
    let second: serde_json::Value = serde_json::from_str(&jev.requests.lock().unwrap()[1]).unwrap();
    assert_eq!(second["state"]["candidates"][0]["upstream_gone"], true);
}

#[test]
fn changed_jev_input_is_sent_before_the_pending_answer_returns() {
    let (tmp, repo, remote) = repo_with_two_jev_candidates();
    run_git(&remote, &["branch", "-D", "topic"]);
    run_git(&repo, &["config", "karu.jev", "true"]);

    let received = tmp.path().join("jev-request-received");
    let upload_pack = tmp.path().join("upload-pack.sh");
    fs::write(
        &upload_pack,
        format!(
            "#!/bin/sh\ni=0\nwhile [ ! -f '{}' ]; do\n  i=$((i + 1))\n  [ $i -lt 1000 ] || exit 1\n  sleep 0.01\ndone\nexec git-upload-pack \"$@\"\n",
            received.display()
        ),
    )
    .unwrap();
    run_git(
        &repo,
        &[
            "config",
            "remote.origin.uploadpack",
            &format!("sh '{}'", upload_pack.display()),
        ],
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || -> anyhow::Result<_> {
        let (pending_stream, pending) = accept_jev_request(&listener)?;
        fs::write(received, b"received")?;
        let (changed_stream, changed) = accept_jev_request(&listener)?;
        reply_to_jev(changed_stream, &changed, 0.1)?;
        reply_to_jev(pending_stream, &pending, 0.95)?;
        Ok((pending, changed))
    });

    let mut cmd = Command::new(karu_bin());
    isolate_git_env(&mut cmd);
    let child = cmd
        .arg("--json")
        .current_dir(&repo)
        .env("TYPESAFE_API_KEY", "test-key")
        .env("TYPESAFE_BASE_URL", url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let output = wait_for_karu(child);
    let (pending, changed) = server
        .join()
        .unwrap()
        .expect("changed input must arrive while the first answer is withheld");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(pending["state"]["candidates"].as_array().unwrap().len(), 2);
    let changed = changed["state"]["candidates"].as_array().unwrap();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["branch"], "topic");
    assert_eq!(changed[0]["upstream_gone"], true);
    assert_eq!(changed[0]["local_only_commit_count"], 1);

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verdict_for(&report, "topic")["verdict"], "keep");
    assert_eq!(
        verdict_for(&report, "topic")["reason"],
        "commits exist only locally"
    );
    assert_eq!(verdict_for(&report, "other")["verdict"], "ask");
}

#[test]
fn finished_phases_stay_on_screen_above_the_table() {
    let (_tmp, repo) = merged_feature_repo();
    let output = run_sei(&repo, &["--no-fetch"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let collected = stderr
        .find("Collected 2 branches")
        .unwrap_or_else(|| panic!("ERROR: no trace of the collection phase: {stderr}"));
    let table = stderr.find("VERDICT").expect("table");
    assert!(
        collected < table,
        "ERROR: phase line should come first: {stderr}"
    );
    assert!(
        !stderr.contains("Fetched origin"),
        "ERROR: --no-fetch fetched: {stderr}"
    );
}

#[test]
fn runs_as_a_git_subcommand() {
    let (_tmp, repo) = setup_repo();
    let bin_dir = PathBuf::from(env!("CARGO_BIN_EXE_git-karu"))
        .parent()
        .unwrap()
        .to_path_buf();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut git_karu = git_cmd();
    let output = git_karu
        .args(["karu", "--json", "--no-fetch"])
        .current_dir(&repo)
        .env("PATH", &path)
        .env_remove("TYPESAFE_API_KEY")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ERROR: git karu failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(verdict_for(&report, "main")["verdict"], "keep");

    let mut git_help = git_cmd();
    let help = git_help
        .args(["karu", "-h"])
        .current_dir(&repo)
        .env("PATH", &path)
        .output()
        .unwrap();
    let usage = String::from_utf8_lossy(&help.stdout);
    assert!(
        usage.contains("Usage: git karu"),
        "ERROR: usage should name the git form: {usage}"
    );
}

#[test]
fn apply_flag_no_longer_exists() {
    let (_tmp, repo) = setup_repo();
    let output = run_sei(&repo, &["--apply", "--no-fetch"]);
    assert!(!output.status.success());
}

#[test]
fn json_and_yes_are_rejected() {
    let (_tmp, repo) = setup_repo();
    let output = run_sei(&repo, &["--json", "--yes", "--no-fetch"]);
    assert!(!output.status.success());
}

fn merged_feature_repo() -> (TempDir, PathBuf) {
    let (tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "feature"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add a"]);
    run_git(&repo, &["checkout", "main"]);
    run_git(
        &repo,
        &["merge", "--no-ff", "-m", "merge feature", "feature"],
    );
    (tmp, repo)
}

fn branch_names(repo: &Path) -> String {
    let branches = git_cmd()
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .current_dir(repo)
        .output()
        .unwrap();
    String::from_utf8_lossy(&branches.stdout).into_owned()
}

#[test]
fn answering_yes_deletes_in_the_same_run() {
    let (_tmp, repo) = merged_feature_repo();
    let output = run_with_input(&repo, "y\n");
    assert!(
        output.status.success(),
        "ERROR: karu failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let names = branch_names(&repo);
    assert!(
        !names.lines().any(|b| b == "feature"),
        "ERROR: feature branch still present: {names}"
    );
}

#[test]
fn closed_stdin_deletes_nothing() {
    let (_tmp, repo) = merged_feature_repo();
    let output = run_sei(&repo, &["--no-fetch"]);
    assert!(output.status.success());
    let names = branch_names(&repo);
    assert!(
        names.lines().any(|b| b == "feature"),
        "ERROR: closed stdin deleted feature: {names}"
    );
}

#[test]
fn no_plan_file_is_written() {
    let (_tmp, repo) = merged_feature_repo();
    run_sei(&repo, &["--no-fetch"]);
    run_sei(&repo, &["--json", "--no-fetch"]);
    assert!(
        !repo.join(".git/karu-plan.json").exists(),
        "ERROR: plan file was written"
    );
}

#[test]
fn sixteen_unique_commits_go_to_jev_without_api_key() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "long-lived"]);
    for i in 0..16 {
        fs::write(repo.join("n.txt"), format!("{i}\n")).unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", &format!("c{i}")]);
    }
    run_git(&repo, &["checkout", "main"]);

    let report = json_report(&repo);
    let item = verdict_for(&report, "long-lived");
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["source"], "jev");
    assert_eq!(item["reason"], "jev skipped: no API key");
}

#[test]
fn fifteen_unique_commits_go_to_jev_without_api_key() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "almost-long"]);
    for i in 0..15 {
        fs::write(repo.join("n.txt"), format!("{i}\n")).unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-m", &format!("c{i}")]);
    }
    run_git(&repo, &["checkout", "main"]);

    let report = json_report(&repo);
    let item = verdict_for(&report, "almost-long");
    assert_eq!(item["verdict"], "keep");
    assert_eq!(item["source"], "jev");
    assert_eq!(item["reason"], "jev skipped: no API key");
}

#[test]
fn tracking_mismatched_upstream_does_not_delete() {
    let (_tmp, repo) = setup_repo();
    run_git(&repo, &["checkout", "-b", "feature"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add a"]);
    run_git(&repo, &["checkout", "main"]);
    run_git(&repo, &["branch", "-u", "main", "feature"]);

    let report = json_report(&repo);
    let item = verdict_for(&report, "feature");
    assert_eq!(item["verdict"], "keep");
    assert_ne!(item["verdict"], "delete");
}
