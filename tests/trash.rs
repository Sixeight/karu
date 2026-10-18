use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    tmp: tempfile::TempDir,
    repo: PathBuf,
    worktrees: PathBuf,
}

impl Fixture {
    fn new(count: usize) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let worktrees = tmp.path().join("elsewhere/worktrees");
        fs::create_dir(&repo).unwrap();
        fs::create_dir_all(&worktrees).unwrap();
        let fixture = Self {
            tmp,
            repo,
            worktrees,
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.name", "Test"]);
        fixture.git(&["config", "user.email", "test@example.com"]);
        fixture.git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "init",
        ]);
        fixture.git(&["checkout", "-qb", "completed"]);
        fs::write(fixture.repo.join("landed"), "done").unwrap();
        fixture.git(&["add", "landed"]);
        fixture.git(&["-c", "commit.gpgsign=false", "commit", "-qm", "feature"]);
        fixture.git(&["checkout", "-q", "main"]);
        fixture.git(&[
            "-c",
            "commit.gpgsign=false",
            "merge",
            "--no-ff",
            "-qm",
            "merge",
            "completed",
        ]);
        for i in 0..count {
            let branch = format!("topic-{i}");
            let path = fixture.worktrees.join(&branch);
            fixture.git(&[
                "worktree",
                "add",
                "-qb",
                &branch,
                path.to_str().unwrap(),
                "completed",
            ]);
        }
        let bin = fixture.tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(fixture.tmp.path().join("started")).unwrap();
        fs::write(
            bin.join("rm"),
            r#"#!/bin/sh
echo "$PPID" > "$KARU_TEST_DIR/started/$$"
if ! mkdir "$KARU_TEST_DIR/active" 2>/dev/null; then touch "$KARU_TEST_DIR/overlap"; fi
while [ ! -e "$KARU_TEST_DIR/release" ]; do sleep 0.02; done
if [ -e "$KARU_TEST_DIR/fail" ]; then rmdir "$KARU_TEST_DIR/active"; exit 1; fi
/bin/rm "$@"
status=$?
rmdir "$KARU_TEST_DIR/active"
exit "$status"
"#,
        )
        .unwrap();
        fs::set_permissions(bin.join("rm"), fs::Permissions::from_mode(0o755)).unwrap();
        fixture
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_karu"));
        cmd.current_dir(&self.repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("TYPESAFE_API_KEY")
            .env("XDG_STATE_HOME", self.tmp.path().join("state"))
            .env("KARU_TEST_DIR", self.tmp.path())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.tmp.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) {
        let mut child = self.command().args(args).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "karu: {status}");
                return;
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("karu waited for background deletion");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn starts(&self) -> Vec<PathBuf> {
        fs::read_dir(self.tmp.path().join("started"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect()
    }

    fn release(&self) {
        fs::write(self.tmp.path().join("release"), "").unwrap();
    }

    fn wait_empty(&self) {
        wait_for(|| fs::read_dir(&self.worktrees).unwrap().next().is_none());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.release();
        // Give detached children a chance to exit before their fixture disappears.
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn twelve_worktrees_are_deleted_serially_after_karu_exits() {
    let fixture = Fixture::new(12);
    fixture.run(&["--yes", "--no-fetch"]);
    wait_for(|| !fixture.starts().is_empty());
    std::thread::sleep(Duration::from_millis(300));
    let started = fixture.starts().len();
    assert!(fs::read_dir(&fixture.worktrees).unwrap().all(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".karu-trash-")
    }));
    fixture.release();
    fixture.wait_empty();
    assert_eq!(
        started, 1,
        "rm processes started while the first was blocked"
    );
    assert_eq!(fixture.starts().len(), 12);
    assert!(!fixture.tmp.path().join("overlap").exists());
}

#[test]
fn repeated_runs_share_the_limit_and_rm_keeps_the_lock_after_worker_death() {
    let fixture = Fixture::new(12);
    fixture.run(&["--yes", "--no-fetch"]);
    wait_for(|| fixture.starts().len() == 1);
    let worker = fs::read_to_string(&fixture.starts()[0]).unwrap();
    assert!(
        Command::new("kill")
            .args(["-KILL", worker.trim()])
            .status()
            .unwrap()
            .success()
    );
    fixture.run(&["--no-fetch"]);
    fixture.run(&["--no-fetch"]);
    std::thread::sleep(Duration::from_millis(300));
    let started = fixture.starts().len();
    fixture.release();
    fixture.wait_empty();
    assert_eq!(started, 1);
    assert!(!fixture.tmp.path().join("overlap").exists());
}

#[test]
fn next_run_recovers_a_queue_outside_known_worktree_parents_and_json_leaves_it_alone() {
    let fixture = Fixture::new(12);
    fixture.run(&["--yes", "--no-fetch"]);
    wait_for(|| fixture.starts().len() == 1);
    let start = fixture.starts().pop().unwrap();
    let worker = fs::read_to_string(&start).unwrap();
    assert!(
        Command::new("kill")
            .args(["-KILL", worker.trim()])
            .status()
            .unwrap()
            .success()
    );
    let rm_pid = start.file_name().unwrap();
    assert!(
        Command::new("kill")
            .arg("-KILL")
            .arg(rm_pid)
            .status()
            .unwrap()
            .success()
    );
    fs::remove_dir(fixture.tmp.path().join("active")).unwrap();
    fixture.release();
    fixture.run(&["--json", "--no-fetch"]);
    assert_eq!(fixture.starts().len(), 1);
    assert_eq!(fs::read_dir(&fixture.worktrees).unwrap().count(), 12);
    fixture.run(&["--no-fetch"]);
    fixture.wait_empty();
    assert_eq!(fixture.starts().len(), 13);
    assert!(!fixture.tmp.path().join("overlap").exists());
}

#[test]
fn quitting_the_foreground_process_does_not_stop_cleanup() {
    let fixture = Fixture::new(0);
    let leftover = fixture.tmp.path().join(".karu-trash-1-0-interrupted");
    fs::create_dir(&leftover).unwrap();
    let mut child = fixture
        .command()
        .arg("--no-fetch")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for(|| fixture.starts().len() == 1);
    assert!(
        child.try_wait().unwrap().is_none(),
        "foreground must still be waiting for confirmation"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    fixture.release();
    wait_for(|| !leftover.exists());
}

#[test]
fn failed_deletions_remain_queued_for_the_next_run() {
    let fixture = Fixture::new(12);
    fs::write(fixture.tmp.path().join("fail"), "").unwrap();
    fixture.release();
    fixture.run(&["--yes", "--no-fetch"]);
    wait_for(|| fixture.starts().len() == 12);
    wait_for(|| !fixture.tmp.path().join("active").exists());
    assert_eq!(fs::read_dir(&fixture.worktrees).unwrap().count(), 12);
    fs::remove_file(fixture.tmp.path().join("fail")).unwrap();
    fixture.run(&["--no-fetch"]);
    fixture.wait_empty();
    assert_eq!(fixture.starts().len(), 24);
    assert!(!fixture.tmp.path().join("overlap").exists());
}

#[test]
fn reusing_a_worktree_path_does_not_strand_its_previous_trash() {
    let fixture = Fixture::new(3);
    let queue = fixture.tmp.path().join("state/karu/trash");
    fs::create_dir_all(&queue).unwrap();
    let lock = fs::File::create(queue.join("lock")).unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    fixture.run(&["--yes", "--no-fetch"]);
    let replacement = fixture.worktrees.join("topic-0");
    fixture.git(&[
        "worktree",
        "add",
        "-qb",
        "replacement",
        replacement.to_str().unwrap(),
        "main",
    ]);
    drop(lock);
    fixture.release();
    fixture.run(&["--drain-trash"]);
    assert_eq!(fs::read_dir(&fixture.worktrees).unwrap().count(), 1);
    assert!(replacement.join(".git").exists());
    assert_eq!(fixture.starts().len(), 3);
}
