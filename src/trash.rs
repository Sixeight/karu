use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub(crate) const PREFIX: &str = ".karu-trash-";

#[derive(Deserialize, Serialize)]
struct Entry {
    path: PathBuf,
    admin: Option<PathBuf>,
}

pub(crate) struct Job {
    path: PathBuf,
    _file: File,
    committed: bool,
}

impl Job {
    pub(crate) fn prepare(path: &Path, admin: Option<&Path>) -> Result<Self> {
        anyhow::ensure!(path.is_absolute(), "trash path must be absolute");
        let queue = queue_dir()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&queue)
            .context("failed to create trash queue")?;
        let mut temp = tempfile::NamedTempFile::new_in(&queue)?;
        // A worker must not see a half-written entry or delete before Git unregisters it.
        fs2::FileExt::lock_exclusive(temp.as_file())?;
        serde_json::to_writer(
            &mut temp,
            &Entry {
                path: path.to_path_buf(),
                admin: admin.map(Path::to_path_buf),
            },
        )?;
        temp.as_file().sync_all()?;
        let path = temp.path().with_extension("pending");
        let file = temp.persist(&path).context("failed to save trash entry")?;
        Ok(Self {
            path,
            _file: file,
            committed: false,
        })
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        self.committed = true;
        fs::rename(&self.path, self.path.with_extension("ready"))
            .context("failed to mark trash ready for cleanup")
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn queue_dir() -> Result<PathBuf> {
    #[cfg(test)]
    {
        static STATE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        Ok(STATE
            .get_or_init(|| tempfile::tempdir().unwrap())
            .path()
            .to_path_buf())
    }
    #[cfg(not(test))]
    queue_dir_from_env()
}

#[cfg(not(test))]
fn queue_dir_from_env() -> Result<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .context("HOME or an absolute XDG_STATE_HOME is required for trash cleanup")?;
    Ok(state.join("karu/trash"))
}

pub(crate) fn is_trash(path: &Path) -> bool {
    path.is_absolute()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(PREFIX))
        && fs::symlink_metadata(path)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false)
}

pub(crate) fn resume() {
    // Unit tests drain explicitly; the integration tests exercise the detached executable.
    if cfg!(test) {
        return;
    }
    let start = || -> Result<()> {
        let queue = queue_dir()?;
        if !queue.is_dir()
            || !fs::read_dir(&queue)?
                .flatten()
                .any(|e| is_queue_entry(&e.path()))
        {
            return Ok(());
        }
        let errors = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(queue.join("cleanup.log"))?;
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--drain-trash")
            .current_dir(&queue)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errors))
            .process_group(0)
            .spawn()
            .context("failed to start trash cleanup")?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    };
    if let Err(err) = start() {
        eprintln!("warning: trash cleanup will retry on the next run: {err:#}");
    }
}

pub(crate) fn drain() -> Result<()> {
    let queue = queue_dir()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(queue.join("lock"))?;
    // Wait in the detached worker so enqueueing during another worker's exit cannot be lost.
    fs2::FileExt::lock_exclusive(&lock)?;
    for entry in fs::read_dir(&queue)? {
        let path = entry?.path();
        if !is_queue_entry(&path) {
            continue;
        }
        if let Err(err) = delete_entry(&path, &lock) {
            eprintln!("warning: trash cleanup will retry on the next run: {err:#}");
        }
    }
    Ok(())
}

fn is_queue_entry(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "pending" || ext == "ready")
}

fn delete_entry(path: &Path, lock: &File) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
        Err(err) => return Err(err.into()),
    }
    let entry: Entry = serde_json::from_reader(&file)?;
    let named = entry.path.is_absolute()
        && entry
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(PREFIX));
    let directory = match fs::symlink_metadata(&entry.path) {
        Ok(meta) => meta.file_type().is_dir(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };
    if named && directory {
        // Once unregistered, a reused Git admin path belongs to another worktree.
        let unregistered =
            path.extension().is_some_and(|ext| ext == "ready") && entry.admin.is_some();
        if !unregistered
            && (entry
                .admin
                .as_ref()
                .map(|admin| admin.try_exists())
                .transpose()?
                .unwrap_or(false)
                || registered_worktree(&entry.path)?)
        {
            return Ok(());
        }
        // rm keeps the lock even if its worker is killed; a restart must still wait for it.
        let status = Command::new("rm")
            .args(["-rf", "--"])
            .arg(&entry.path)
            .stdin(Stdio::from(lock.try_clone()?))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("failed to run rm")?;
        anyhow::ensure!(
            status.success(),
            "rm failed for {}: {status}",
            entry.path.display()
        );
    }
    fs::remove_file(path)?;
    Ok(())
}

fn registered_worktree(trash: &Path) -> Result<bool> {
    match fs::read_to_string(trash.join(".git")) {
        Ok(link) => match link.strip_prefix("gitdir:") {
            Some(admin) => Ok(trash.join(admin.trim()).try_exists()?),
            None => Ok(true),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncommitted_jobs_and_registered_worktrees_are_not_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = tmp.path().join(".karu-trash-1-0-pending");
        let admin = tmp.path().join("admin");
        fs::create_dir(&trash).unwrap();
        fs::create_dir(&admin).unwrap();
        let mut job = Job::prepare(&trash, Some(&admin)).unwrap();
        drain().unwrap();
        assert!(trash.exists(), "the foreground still owns this job");
        // Simulate interruption before the foreground marks the job ready.
        job.committed = true;
        drop(job);
        drain().unwrap();
        assert!(trash.exists(), "Git has not unregistered this worktree");
        fs::remove_dir(&admin).unwrap();
        drain().unwrap();
        assert!(!trash.exists());
    }

    #[test]
    fn sweep_jobs_respect_git_registration_and_leave_symlinks_and_other_names_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = tmp.path().join(".karu-trash-1-0-registered");
        let admin = tmp.path().join("admin");
        let kept = tmp.path().join("karu-kept-1-0-rescued");
        let link = tmp.path().join(".karu-trash-1-0-link");
        fs::create_dir(&trash).unwrap();
        fs::create_dir(&admin).unwrap();
        fs::create_dir(&kept).unwrap();
        fs::write(trash.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::os::unix::fs::symlink(&kept, &link).unwrap();
        for path in [&trash, &kept, &link] {
            Job::prepare(path, None).unwrap().commit().unwrap();
        }
        drain().unwrap();
        assert!(trash.exists());
        assert!(kept.exists());
        assert!(link.is_symlink());
        fs::remove_dir(&admin).unwrap();
        drain().unwrap();
        assert!(!trash.exists());
    }

    #[test]
    fn cancelled_jobs_and_missing_targets_leave_no_queue_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let trash = tmp.path().join(".karu-trash-1-0-missing");
        let job = Job::prepare(&trash, None).unwrap();
        let entry = job.path.clone();
        drop(job);
        assert!(!entry.exists());
        let job = Job::prepare(&trash, None).unwrap();
        let entry = job.path.with_extension("ready");
        job.commit().unwrap();
        drain().unwrap();
        assert!(!entry.exists());
    }
}
