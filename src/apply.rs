use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};

use crate::candidate::{Judged, VerdictKind};
use crate::git;
use crate::interact::{Decision, is_yes};

pub struct ApplyResult {
    pub deleted: Vec<String>,
    pub errors: Vec<String>,
    /// Worktrees whose files are still being deleted in the background.
    pub trashed: usize,
}

pub fn confirm(count: usize) -> Result<bool> {
    eprint!("{count} item(s) will be deleted. Proceed? [y/N] ");
    io::stderr().flush().ok();
    read_yes()
}

fn read_yes() -> Result<bool> {
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed to read confirmation")?;
    Ok(is_yes(&line))
}

/// Walks the asked items one by one. `Quit` stops early; what was not
/// confirmed by then stays.
pub fn confirm_asks<F>(judged: &mut [Judged], mut ask: F) -> Result<usize>
where
    F: FnMut(&Judged, usize, usize) -> Result<Decision>,
{
    let total = judged
        .iter()
        .filter(|item| item.verdict.kind == VerdictKind::Ask)
        .count();
    let mut promoted = 0;
    let mut index = 0;
    for item in judged.iter_mut() {
        if item.verdict.kind != VerdictKind::Ask {
            continue;
        }
        index += 1;
        match ask(item, index, total)? {
            Decision::Delete => {
                item.verdict.kind = VerdictKind::Delete;
                promoted += 1;
            }
            Decision::Keep => {}
            Decision::Quit => break,
        }
    }
    Ok(promoted)
}

pub enum Progress<'a> {
    Started {
        total: usize,
    },
    Deleted {
        name: &'a str,
        /// The deleted branch's tip, so `git branch <name> <sha>` brings it back.
        sha: Option<&'a str>,
        done: usize,
        total: usize,
    },
    Failed {
        name: &'a str,
        error: &'a str,
        done: usize,
        total: usize,
    },
}

#[cfg(test)]
fn apply(root: &Path, judged: &[Judged], force: bool) -> ApplyResult {
    apply_with(root, judged, force, |_| {})
}

pub fn apply_with<F>(root: &Path, judged: &[Judged], force: bool, report: F) -> ApplyResult
where
    F: FnMut(Progress),
{
    let targets: Vec<&Judged> = judged
        .iter()
        .filter(|item| item.verdict.kind == VerdictKind::Delete)
        .collect();
    let trashed = AtomicUsize::new(0);
    let mut result = run_deletions(
        &targets,
        |item| {
            let moved = remove_worktree(root, item, force)?;
            if moved {
                trashed.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        },
        |item| delete_branch(root, item),
        report,
    );
    result.trashed = trashed.into_inner();
    result
}

/// Removing a worktree means deleting its files, which is nearly all of the
/// time a deletion takes, so several go at once.
const WORKTREE_REMOVALS_AT_ONCE: usize = 4;

/// Worktrees are removed side by side. Each branch is deleted on this thread
/// as soon as its own worktree is gone: `git branch -D` is quick, and running
/// several at once would have them fight over the packed-refs lock.
fn run_deletions<R, B, F>(
    targets: &[&Judged],
    remove_worktree: R,
    mut delete_branch: B,
    mut report: F,
) -> ApplyResult
where
    R: Fn(&Judged) -> Result<()> + Sync,
    B: FnMut(&Judged) -> Result<()>,
    F: FnMut(Progress),
{
    let total = targets.len();
    let mut result = ApplyResult {
        deleted: Vec::new(),
        errors: Vec::new(),
        trashed: 0,
    };
    report(Progress::Started { total });

    let mut done = 0;
    let mut finish = |item: &Judged, removed: Result<()>, result: &mut ApplyResult| {
        let name = item.candidate.display_name();
        done += 1;
        match removed.and_then(|()| delete_branch(item)) {
            Ok(()) => {
                report(Progress::Deleted {
                    name: &name,
                    sha: item.candidate.sha.as_deref(),
                    done,
                    total,
                });
                result.deleted.push(name);
            }
            Err(err) => {
                let error = format!("{err:#}");
                report(Progress::Failed {
                    name: &name,
                    error: &error,
                    done,
                    total,
                });
                result.errors.push(error);
            }
        }
    };

    let (with_worktree, without): (Vec<&Judged>, Vec<&Judged>) = targets
        .iter()
        .copied()
        .partition(|item| has_removable_worktree(item));
    for item in without {
        finish(item, Ok(()), &mut result);
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let (sender, receiver) = std::sync::mpsc::channel::<(usize, Result<()>)>();
    std::thread::scope(|s| {
        for _ in 0..WORKTREE_REMOVALS_AT_ONCE.min(with_worktree.len()) {
            let sender = sender.clone();
            let (next, with_worktree, remove_worktree) = (&next, &with_worktree, &remove_worktree);
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = with_worktree.get(i) else {
                        break;
                    };
                    if sender.send((i, remove_worktree(item))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        for (i, removed) in receiver {
            finish(with_worktree[i], removed, &mut result);
        }
    });
    result
}

fn has_removable_worktree(item: &Judged) -> bool {
    item.candidate.worktree_path.is_some() && !item.candidate.is_primary_worktree
}

/// Removes a linked worktree and reports whether its files were handed to a
/// background deletion. Deleting a big worktree's files is nearly all of the
/// time a deletion takes, so the directory is renamed out of the way (the
/// path is free again at once), its admin entry is dropped, and the files are
/// deleted by a detached process. When any of that is not possible, it falls
/// back to `git worktree remove`.
fn remove_worktree(root: &Path, item: &Judged, force: bool) -> Result<bool> {
    let Some(path) = &item.candidate.worktree_path else {
        return Ok(false);
    };
    if item.candidate.is_primary_worktree {
        return Ok(false);
    }
    // a dirty worktree the user already confirmed (or --force) may go
    let force = force || item.candidate.worktree_dirty;
    let wt = Path::new(path);
    let missing = !wt.exists();
    let Some(admin) = worktree_admin_dir(root, wt) else {
        return git_remove(root, path, force || missing).map(|()| false);
    };

    // what `git worktree remove` would refuse, checked before anything moves
    if admin.join("locked").exists() {
        anyhow::bail!("failed to remove worktree {path}: it is locked");
    }
    if missing {
        std::fs::remove_dir_all(&admin)
            .with_context(|| format!("failed to unregister vanished worktree {path}"))?;
        return Ok(false);
    }
    if !force {
        let status = git::git_output(wt, &["status", "--porcelain"])
            .with_context(|| format!("failed to check worktree {path}"))?;
        if !status.is_empty() {
            anyhow::bail!("failed to remove worktree {path}: it has uncommitted changes");
        }
    }

    let Some(trash) = trash_path(wt) else {
        return git_remove(root, path, force).map(|()| false);
    };
    if std::fs::rename(wt, &trash).is_err() {
        // another filesystem, a busy directory, a read-only parent
        return git_remove(root, path, force).map(|()| false);
    }
    if let Err(err) = std::fs::remove_dir_all(&admin) {
        // put it back exactly as it was; if even that fails the files are
        // still there under another name, and only the error says where
        if let Err(stranded) = std::fs::rename(&trash, wt) {
            // Left under the trash name it would be swept, and deleted without
            // a prompt on a later run. Out of that name it is only a directory.
            let kept = rescue_path(&trash);
            let at = match kept.as_ref().filter(|k| std::fs::rename(&trash, k).is_ok()) {
                Some(kept) => kept.clone(),
                None => trash.clone(),
            };
            return Err(err).context(format!(
                "failed to unregister worktree {path}, and it could not be put back \
                 ({stranded}); its files are in {}",
                at.display()
            ));
        }
        return Err(err).with_context(|| format!("failed to unregister worktree {path}"));
    }
    delete_in_background(&trash);
    Ok(true)
}

fn git_remove(root: &Path, path: &str, force: bool) -> Result<()> {
    git::worktree_remove(root, path, force)
        .with_context(|| format!("failed to remove worktree {path}"))
}

/// `<repo>/.git/worktrees/<id>`, read from the worktree's `.git` file. `None`
/// when it is anything else, so nothing outside that directory is ever
/// removed: the entry has to sit under the worktrees directory of the repo
/// karu was pointed at, and to name this worktree back. Anything unfamiliar
/// (a different repo, a layout karu does not recognise) falls back to
/// `git worktree remove`, which is slower but always right.
fn worktree_admin_dir(root: &Path, wt: &Path) -> Option<PathBuf> {
    admin_from_git_file(root, wt).or_else(|| admin_from_worktrees_dir(root, wt))
}

fn admin_from_git_file(root: &Path, wt: &Path) -> Option<PathBuf> {
    let link = std::fs::read_to_string(wt.join(".git")).ok()?;
    let target = Path::new(link.strip_prefix("gitdir:")?.trim());
    let admin = if target.is_absolute() {
        target.to_path_buf()
    } else {
        wt.join(target)
    };
    let admin = std::fs::canonicalize(admin).ok()?;
    let worktrees = admin.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    // the entry must belong to this repo, not to some other one it links into
    let ours = git::git_common_dir(root)
        .ok()
        .and_then(|dir| std::fs::canonicalize(dir).ok())?;
    if worktrees.parent()? != ours {
        return None;
    }
    // git's own back-pointer: the entry has to name the worktree being removed
    let back = std::fs::read_to_string(admin.join("gitdir")).ok()?;
    let back = Path::new(back.trim()).parent()?;
    let back = std::fs::canonicalize(back).ok()?;
    let wt = std::fs::canonicalize(wt).ok()?;
    (back == wt && admin.join("HEAD").is_file()).then_some(admin)
}

/// When the worktree directory is already gone, `.git` is gone with it, so the
/// admin entry is found from this repo's `worktrees/` directory instead.
fn admin_from_worktrees_dir(root: &Path, wt: &Path) -> Option<PathBuf> {
    let ours = git::git_common_dir(root)
        .ok()
        .and_then(|dir| std::fs::canonicalize(dir).ok())?;
    let mut found = None;
    for entry in std::fs::read_dir(ours.join("worktrees")).ok()?.flatten() {
        let admin = entry.path();
        if !admin.join("HEAD").is_file() {
            continue;
        }
        // an entry karu cannot read says nothing about the one being looked for
        let Ok(gitdir) = std::fs::read_to_string(admin.join("gitdir")) else {
            continue;
        };
        let Some(linked) = Path::new(gitdir.trim()).parent().map(Path::to_path_buf) else {
            continue;
        };
        if !same_worktree_path(&linked, wt) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(admin);
    }
    found
}

/// macOS exposes `/var` as a symlink to `/private/var`. After the worktree
/// directory is gone, canonicalize cannot resolve that, so both spellings
/// have to compare equal.
fn same_worktree_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    path_key(a) == path_key(b)
}

fn path_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let stripped = raw.strip_prefix("/private").unwrap_or(raw.as_ref());
    stripped.trim_end_matches('/').to_string()
}

/// Prefix of the directories karu renames worktrees to before deleting them.
const TRASH_PREFIX: &str = ".karu-trash-";

fn trash_path(wt: &Path) -> Option<PathBuf> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let wt = std::fs::canonicalize(wt).ok()?;
    let name = wt.file_name()?.to_string_lossy().into_owned();
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    Some(wt.with_file_name(format!("{TRASH_PREFIX}{}-{n}-{name}", std::process::id())))
}

/// A name outside the trash prefix, so a sweep leaves the directory alone.
fn rescue_path(trash: &Path) -> Option<PathBuf> {
    let name = trash.file_name()?.to_str()?.strip_prefix(TRASH_PREFIX)?;
    Some(trash.with_file_name(format!("karu-kept-{name}")))
}

/// A real directory (not a symlink) that karu itself named as trash.
fn is_trash(path: &Path) -> bool {
    let named = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(TRASH_PREFIX));
    named
        && std::fs::symlink_metadata(path)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false)
}

/// Deletes the directory in a process that outlives karu. It gets no
/// terminal (so nothing lands on the next prompt) and its own process group
/// (so Ctrl-C on karu does not stop it halfway).
fn delete_in_background(trash: &Path) {
    use std::os::unix::process::CommandExt;
    if !trash.is_absolute() || !is_trash(trash) {
        return;
    }
    let _ = Command::new("rm")
        .arg("-rf")
        .arg("--")
        .arg(trash)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
}

/// Finishes what an interrupted run left behind: trash next to any worktree
/// karu knows about. Never fails the run.
pub fn sweep_trash(judged: &[Judged]) {
    let mut parents: Vec<PathBuf> = judged
        .iter()
        .filter_map(|item| item.candidate.worktree_path.as_deref())
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect();
    parents.sort();
    parents.dedup();
    for parent in parents {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_trash(&path) {
                delete_in_background(&path);
            }
        }
    }
}

fn delete_branch(root: &Path, item: &Judged) -> Result<()> {
    if let Some(branch) = &item.candidate.branch {
        git::branch_delete(root, branch)
            .with_context(|| format!("failed to delete branch {branch}"))
    } else if item.candidate.worktree_path.is_none() {
        anyhow::bail!("nothing to delete for {}", item.candidate.display_name())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Candidate, Judged, Source, Verdict, VerdictKind};
    use std::path::PathBuf;

    fn judged(kind: VerdictKind) -> Judged {
        Judged {
            candidate: Candidate {
                branch: Some("feature".into()),
                last_commit_at: None,
                last_subject: None,
                unique_subjects: vec![],
                behind: None,
                unique_commit_count: None,
                diffstat: None,
                ..crate::candidate::fixtures::candidate()
            },
            verdict: Verdict {
                kind,
                source: Source::Hard,
                reason: crate::reason::Reason::Merged,
                score: None,
            },
        }
    }

    #[test]
    fn apply_keeps_the_underlying_git_error() {
        let result = apply(Path::new("/tmp"), &[judged(VerdictKind::Delete)], false);
        assert_eq!(result.errors.len(), 1);
        let error = &result.errors[0];
        assert!(error.contains("failed to delete branch feature"), "{error}");
        assert!(error.contains("git branch"), "{error}");
    }

    #[test]
    fn apply_skips_keep_and_ask() {
        let items = vec![judged(VerdictKind::Keep), judged(VerdictKind::Ask)];
        let result = apply(Path::new("/tmp"), &items, false);
        assert!(result.deleted.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn apply_reports_progress_for_each_deletion() {
        let mut first = judged(VerdictKind::Delete);
        first.candidate.branch = None;
        first.candidate.worktree_path = None;
        let kept = judged(VerdictKind::Keep);
        let mut second = judged(VerdictKind::Delete);
        second.candidate.branch = None;
        second.candidate.worktree_path = None;

        let mut events = Vec::new();
        apply_with(Path::new("/tmp"), &[first, kept, second], false, |event| {
            events.push(match event {
                Progress::Started { total } => format!("start {total}"),
                Progress::Deleted { .. } => "deleted".to_string(),
                Progress::Failed {
                    error, done, total, ..
                } => {
                    assert!(error.contains("nothing to delete"));
                    format!("failed {done}/{total}")
                }
            });
        });
        assert_eq!(events, ["start 2", "failed 1/2", "failed 2/2"]);
    }

    fn with_worktree(name: &str) -> Judged {
        let mut item = judged(VerdictKind::Delete);
        item.candidate.branch = Some(name.into());
        item.candidate.worktree_path = Some(format!("/work/{name}"));
        item
    }

    #[test]
    fn worktrees_are_removed_side_by_side() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let items: Vec<Judged> = (0..4).map(|i| with_worktree(&format!("b{i}"))).collect();
        let targets: Vec<&Judged> = items.iter().collect();
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let result = run_deletions(
            &targets,
            |_| {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let started = std::time::Instant::now();
                while peak.load(Ordering::SeqCst) < 4
                    && started.elapsed() < std::time::Duration::from_secs(2)
                {
                    std::thread::yield_now();
                }
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            },
            |_| Ok(()),
            |_| {},
        );
        assert_eq!(result.deleted.len(), 4);
        assert_eq!(
            peak.load(Ordering::SeqCst),
            4,
            "four removals should overlap"
        );
    }

    #[test]
    fn a_branch_goes_only_after_its_own_worktree_is_gone() {
        use std::sync::Mutex;
        let mut plain = judged(VerdictKind::Delete);
        plain.candidate.branch = Some("plain".into());
        let items = [with_worktree("ok"), with_worktree("stuck"), plain];
        let targets: Vec<&Judged> = items.iter().collect();
        let removed = Mutex::new(Vec::new());
        let mut finished = Vec::new();
        let result = run_deletions(
            &targets,
            |item| {
                let name = item.candidate.display_name();
                if name == "stuck" {
                    anyhow::bail!("worktree is locked");
                }
                removed.lock().unwrap().push(name);
                Ok(())
            },
            |item| {
                let name = item.candidate.display_name();
                // the worktree step is skipped for a branch without one
                assert!(name == "plain" || removed.lock().unwrap().contains(&name));
                finished.push(name);
                Ok(())
            },
            |_| {},
        );
        finished.sort();
        assert_eq!(finished, ["ok", "plain"], "`stuck` keeps its branch");
        assert_eq!(result.deleted.len(), 2);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("worktree is locked"));
    }

    #[test]
    fn apply_records_failure_and_continues() {
        let mut first = judged(VerdictKind::Delete);
        first.candidate.branch = None;
        first.candidate.worktree_path = None;
        let mut second = judged(VerdictKind::Delete);
        second.candidate.branch = None;
        second.candidate.worktree_path = None;
        let result = apply(Path::new("/tmp"), &[first, second], false);
        assert!(result.deleted.is_empty());
        assert_eq!(result.errors.len(), 2);
        assert!(result.errors[0].contains("nothing to delete"));
    }

    #[test]
    fn apply_empty_list_deletes_nothing() {
        let result = apply(&PathBuf::from("."), &[], true);
        assert!(result.deleted.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn confirm_asks_promotes_only_what_was_confirmed() {
        let mut items = vec![
            judged(VerdictKind::Ask),
            judged(VerdictKind::Keep),
            judged(VerdictKind::Ask),
            judged(VerdictKind::Delete),
        ];
        let mut seen = Vec::new();
        let mut answers = vec![Decision::Delete, Decision::Keep].into_iter();
        let promoted = confirm_asks(&mut items, |_, index, total| {
            seen.push((index, total));
            Ok(answers.next().unwrap())
        })
        .unwrap();
        assert_eq!(promoted, 1);
        assert_eq!(seen, [(1, 2), (2, 2)]);
        let kinds: Vec<_> = items.iter().map(|i| i.verdict.kind).collect();
        assert_eq!(
            kinds,
            [
                VerdictKind::Delete,
                VerdictKind::Keep,
                VerdictKind::Ask,
                VerdictKind::Delete
            ]
        );
    }

    #[test]
    fn quitting_keeps_everything_not_yet_confirmed() {
        let mut items = vec![
            judged(VerdictKind::Ask),
            judged(VerdictKind::Ask),
            judged(VerdictKind::Ask),
        ];
        let mut answers = vec![Decision::Delete, Decision::Quit].into_iter();
        let mut asked = 0;
        let promoted = confirm_asks(&mut items, |_, _, _| {
            asked += 1;
            Ok(answers.next().unwrap())
        })
        .unwrap();
        assert_eq!((promoted, asked), (1, 2));
        assert_eq!(items[1].verdict.kind, VerdictKind::Ask);
        assert_eq!(items[2].verdict.kind, VerdictKind::Ask);
    }

    fn git_cmd() -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        isolate_git_env(&mut cmd);
        cmd
    }

    fn isolate_git_env(cmd: &mut std::process::Command) {
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

    /// A real repository with one linked worktree on `feature`.
    fn repo_with_worktree() -> (tempfile::TempDir, PathBuf, PathBuf, Judged) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let wt = tmp.path().join("feature-wt");
        std::fs::create_dir(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = git_cmd().args(args).current_dir(dir).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );
        let mut item = judged(VerdictKind::Delete);
        item.candidate.worktree_path = Some(wt.to_string_lossy().into_owned());
        (tmp, repo, wt, item)
    }

    fn worktree_is_registered(repo: &Path, wt: &Path) -> bool {
        let out = git_cmd()
            .args(["worktree", "list", "--porcelain"])
            .current_dir(repo)
            .output()
            .unwrap();
        let wt = std::fs::canonicalize(wt).unwrap_or_else(|_| wt.to_path_buf());
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .any(|p| Path::new(p) == wt || p.ends_with("feature-wt"))
    }

    fn trash_dirs(parent: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(parent)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_trash(p))
            .collect()
    }

    /// The detached `rm` competes with everything else on the machine, so the
    /// budget is generous; a passing run still returns as soon as it is done.
    fn wait_until_gone(parent: &Path) {
        for _ in 0..600 {
            if trash_dirs(parent).is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("trash was never deleted: {:?}", trash_dirs(parent));
    }

    #[test]
    fn worktree_is_gone_at_once_and_its_files_follow() {
        let (tmp, repo, wt, item) = repo_with_worktree();
        std::fs::create_dir(wt.join("node_modules")).unwrap();
        std::fs::write(wt.join("node_modules/big.js"), "x").unwrap();
        // ignored files must not count as changes
        std::fs::write(repo.join(".git/info/exclude"), "node_modules/\n").unwrap();

        remove_worktree(&repo, &item, false).unwrap();
        assert!(!wt.exists(), "the path is free again right away");
        assert!(!worktree_is_registered(&repo, &wt));
        wait_until_gone(tmp.path());
    }

    /// A half-written entry belonging to another worktree says nothing about
    /// the one being removed, so it must not stop the scan.
    #[test]
    fn a_broken_neighbour_entry_does_not_hide_the_target() {
        let (_tmp, repo, wt, item) = repo_with_worktree();
        // sorts before the real entry, so a scan that gives up meets it first
        let broken = repo.join(".git/worktrees/aaa-half-written");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("HEAD"), "ref: refs/heads/other\n").unwrap();
        std::fs::remove_dir_all(&wt).unwrap();

        assert!(!remove_worktree(&repo, &item, false).unwrap());
        assert!(!worktree_is_registered(&repo, &wt));
        assert!(broken.exists(), "someone else's entry was removed");
    }

    /// Two entries claiming the same path are not something karu can pick
    /// between, so it must hand the removal back to git.
    #[test]
    fn an_ambiguous_entry_is_left_to_git() {
        let (_tmp, repo, wt, _item) = repo_with_worktree();
        let real = std::fs::read_to_string(wt.join(".git")).unwrap();
        let real = PathBuf::from(real.strip_prefix("gitdir:").unwrap().trim());
        let copy = repo.join(".git/worktrees/copy-of-it");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join("HEAD"), std::fs::read(real.join("HEAD")).unwrap()).unwrap();
        std::fs::write(
            copy.join("gitdir"),
            std::fs::read(real.join("gitdir")).unwrap(),
        )
        .unwrap();
        std::fs::remove_dir_all(&wt).unwrap();

        assert_eq!(admin_from_worktrees_dir(&repo, &wt), None);
    }

    /// Files that could not be put back must not carry the trash name: a
    /// later run sweeps that name without asking anyone.
    #[test]
    fn rescued_files_are_out_of_the_sweeps_reach() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = trash_path(tmp.path()).unwrap();
        let rescued = rescue_path(&trash).expect("a trash name can always be rescued");
        std::fs::create_dir(&rescued).unwrap();

        assert!(is_trash(&trash) || !trash.exists());
        assert!(!is_trash(&rescued), "{}", rescued.display());
        // a name that was never karu's trash has nothing to rescue
        assert_eq!(rescue_path(Path::new("/tmp/some-worktree")), None);
    }

    #[test]
    fn worktree_edited_after_the_verdict_is_not_removed() {
        let (tmp, repo, wt, item) = repo_with_worktree();
        std::fs::write(wt.join("new-work.txt"), "just typed this\n").unwrap();

        let err = remove_worktree(&repo, &item, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("uncommitted changes"),
            "{err:#}"
        );
        assert!(wt.join("new-work.txt").exists());
        assert!(worktree_is_registered(&repo, &wt));
        assert!(trash_dirs(tmp.path()).is_empty());

        // confirmed by the user, or --force: then it goes
        remove_worktree(&repo, &item, true).unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn locked_worktree_is_refused_before_anything_moves() {
        let (tmp, repo, wt, item) = repo_with_worktree();
        let out = git_cmd()
            .args(["worktree", "lock", wt.to_str().unwrap()])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success());

        assert!(remove_worktree(&repo, &item, false).is_err());
        assert!(wt.join("a.txt").exists(), "files are back");
        assert!(
            wt.join(".git").is_file(),
            "and so is the link to the repository"
        );
        assert!(worktree_is_registered(&repo, &wt));
        assert!(trash_dirs(tmp.path()).is_empty());
    }

    #[test]
    fn leftovers_of_an_interrupted_run_are_swept() {
        let (tmp, _repo, wt, item) = repo_with_worktree();
        let leftover = tmp.path().join(".karu-trash-999-0-old-wt");
        std::fs::create_dir_all(leftover.join("deep/dir")).unwrap();
        let unrelated = tmp.path().join(".karu-notes");
        std::fs::create_dir(&unrelated).unwrap();

        sweep_trash(&[item]);
        wait_until_gone(tmp.path());
        assert!(unrelated.exists(), "only karu's own trash is touched");
        assert!(wt.exists());
    }

    #[test]
    fn only_the_registration_of_a_vanished_worktree_is_removed() {
        let (tmp, repo, wt, item) = repo_with_worktree();
        // a second worktree whose directory is also gone must stay registered
        let other = tmp.path().join("other-wt");
        let out = git_cmd()
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "other",
                other.to_str().unwrap(),
            ])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success());
        std::fs::remove_dir_all(&wt).unwrap();
        std::fs::remove_dir_all(&other).unwrap();

        assert!(!remove_worktree(&repo, &item, false).unwrap());
        let list = git_cmd()
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let list = String::from_utf8_lossy(&list.stdout);
        assert!(!list.contains("feature-wt"), "{list}");
        assert!(
            list.contains("other-wt"),
            "someone else's entry was removed: {list}"
        );
    }

    #[test]
    fn vanished_worktree_paths_match_with_or_without_private() {
        assert!(same_worktree_path(
            Path::new("/var/folders/x/feature-wt"),
            Path::new("/private/var/folders/x/feature-wt"),
        ));
        assert!(!same_worktree_path(
            Path::new("/var/folders/x/feature-wt"),
            Path::new("/var/folders/x/other-wt"),
        ));
    }
}
