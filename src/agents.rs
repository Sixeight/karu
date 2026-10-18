use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

#[derive(Default)]
pub(crate) struct ActiveAgents {
    directories: Vec<PathBuf>,
    available: bool,
}

impl ActiveAgents {
    pub(crate) fn inspect() -> Self {
        match Self::read() {
            Ok(agents) => agents,
            Err(err) => {
                eprintln!("warning: AI agent session check unavailable: {err:#}");
                Self::default()
            }
        }
    }

    fn read() -> Result<Self> {
        let output = Command::new("ps")
            .args(["-axo", "pid=,args="])
            .output()
            .context("failed to list processes")?;
        anyhow::ensure!(output.status.success(), "ps failed ({})", output.status);
        let pids: Vec<u32> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(agent_pid)
            .collect();
        Ok(Self {
            directories: process_directories(&pids)?,
            available: true,
        })
    }

    pub(crate) fn is_available(&self) -> bool {
        self.available
    }

    pub(crate) fn uses(&self, worktree: &Path) -> bool {
        let worktree = normalized(worktree);
        self.directories
            .iter()
            .any(|cwd| normalized(cwd).starts_with(&worktree))
    }

    pub(crate) fn uses_branch(&self, branch: &str, worktrees: &[&str]) -> bool {
        worktrees.iter().any(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| name == branch)
                && self.uses(Path::new(path))
        })
    }

    pub(crate) fn ensure_branch_unused(&self, branch: &str, worktrees: &[&str]) -> Result<()> {
        for path in worktrees.iter().filter(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| name == branch)
        }) {
            self.ensure_unused(Some(path))?;
        }
        Ok(())
    }

    pub(crate) fn ensure_unused(&self, worktree: Option<&str>) -> Result<()> {
        if let Some(path) = worktree {
            anyhow::ensure!(
                self.available,
                "refusing to delete {path}: AI agent session check unavailable"
            );
            anyhow::ensure!(
                !self.uses(Path::new(path)),
                "refusing to delete {path}: active AI agent session"
            );
        }
        Ok(())
    }
}

fn normalized(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn process(line: &str) -> Option<(u32, &str)> {
    let line = line.trim();
    let (pid, command) = line.split_once(char::is_whitespace)?;
    Some((pid.parse().ok()?, command.trim()))
}

fn agent_name(name: &str) -> bool {
    matches!(
        name,
        "claude" | "codex" | "opencode" | "gemini" | "aider" | "grok"
    )
}

fn agent_pid(line: &str) -> Option<u32> {
    let (pid, command) = process(line)?;
    if Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(agent_name)
    {
        return Some(pid);
    }
    let executable = command.split_whitespace().next()?;
    let path = Path::new(executable);
    let name = path.file_name()?.to_str()?;
    let versioned_claude = path.parent().is_some_and(|parent| {
        parent.file_name().is_some_and(|name| name == "versions")
            && parent
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "claude")
    });
    (agent_name(name) || versioned_claude || script_agent(command)).then_some(pid)
}

fn script_agent(args: &str) -> bool {
    let mut words = args.split_whitespace();
    let Some(runtime) = words
        .next()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
    else {
        return false;
    };
    if !matches!(runtime, "node" | "python" | "python3") {
        return false;
    }
    let Some(script) = words.next() else {
        return false;
    };
    let name = Path::new(script)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    agent_name(name)
        || matches!(name, "codex.js" | "aider-chat")
        || script.contains("/@anthropic-ai/claude-code/")
        || script.contains("/@google/gemini-cli/")
        || (script == "-m" && words.next() == Some("aider"))
}

#[cfg(target_os = "linux")]
fn process_directories(pids: &[u32]) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for pid in pids {
        match std::fs::read_link(format!("/proc/{pid}/cwd")) {
            Ok(path) => directories.push(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read AI agent cwd for PID {pid}"));
            }
        }
    }
    Ok(directories)
}

#[cfg(not(target_os = "linux"))]
fn process_directories(pids: &[u32]) -> Result<Vec<PathBuf>> {
    if pids.is_empty() {
        return Ok(Vec::new());
    }
    let pids = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let output = Command::new("lsof")
        .args(["-a", "-p", &pids, "-d", "cwd", "-Fn"])
        .output()
        .context("failed to read agent working directories with lsof")?;
    anyhow::ensure!(output.status.success(), "lsof failed ({})", output.status);
    anyhow::ensure!(
        !output.stdout.is_empty(),
        "lsof returned no agent working directories"
    );
    let directories = lsof_directories(&String::from_utf8_lossy(&output.stdout));
    anyhow::ensure!(
        directories.len() == pids.split(',').count(),
        "lsof omitted an AI agent working directory"
    );
    Ok(directories)
}

#[cfg(any(not(target_os = "linux"), test))]
fn lsof_directories(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("n/"))
        .map(|path| PathBuf::from(format!("/{path}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_process_waits() {
        if std::env::var_os("KARU_AGENT_TEST_PROCESS").is_some() {
            std::fs::write("agent-ready", "").unwrap();
            loop {
                std::thread::park();
            }
        }
    }

    #[test]
    #[ignore = "requires permission to inspect real processes"]
    fn live_process_protects_collection_and_deletion() {
        use crate::candidate::{Judged, VerdictKind};
        use crate::git::{self, KaruConfig};

        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                self.0.kill().ok();
                self.0.wait().ok();
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let wt = dir.path().join("worktree");
        std::fs::create_dir(&root).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
            vec!["worktree", "add", "-b", "topic", wt.to_str().unwrap()],
        ] {
            git::git_output(&root, &args).unwrap();
        }
        git::git_output(&root, &["branch", "worktree"]).unwrap();
        let collected = crate::collect::local_facts(&root, &KaruConfig::default()).unwrap();
        let candidate = collected
            .candidates
            .into_iter()
            .find(|c| c.branch.as_deref() == Some("topic"))
            .unwrap();
        assert!(!candidate.active_agent);
        let item = Judged {
            candidate,
            verdict: crate::candidate::Verdict {
                kind: VerdictKind::Delete,
                source: crate::candidate::Source::Hard,
                reason: crate::reason::Reason::Merged,
                score: None,
            },
        };
        let executable = dir.path().join("codex");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let _child = Child(
            Command::new(&executable)
                .args(["--exact", "agents::tests::agent_process_waits"])
                .env("KARU_AGENT_TEST_PROCESS", "1")
                .current_dir(&wt)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        for _ in 0..100 {
            if wt.join("agent-ready").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(wt.join("agent-ready").exists());
        assert!(ActiveAgents::read().unwrap().uses(&wt));
        let collected = crate::collect::local_facts(&root, &KaruConfig::default()).unwrap();
        let active = collected
            .candidates
            .iter()
            .find(|c| c.branch.as_deref() == Some("topic"))
            .unwrap();
        assert!(active.active_agent);
        let alias = collected
            .candidates
            .iter()
            .find(|candidate| candidate.branch.as_deref() == Some("worktree"))
            .unwrap();
        assert!(alias.active_agent);
        assert_eq!(
            crate::decide::without_jev(active, true, 0).unwrap().kind,
            VerdictKind::Keep
        );
        let result = crate::apply::apply_with(&root, &[item], true, |_| {});
        assert!(result.deleted.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("active AI agent session"));
        assert!(wt.exists());
        assert!(git::git_output(&root, &["rev-parse", "refs/heads/topic"]).is_ok());
        let alias_item = Judged {
            candidate: alias.clone(),
            verdict: crate::candidate::Verdict {
                kind: VerdictKind::Delete,
                source: crate::candidate::Source::Hard,
                reason: crate::reason::Reason::Merged,
                score: None,
            },
        };
        let result = crate::apply::apply_with(&root, &[alias_item], true, |_| {});
        assert!(result.deleted.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("active AI agent session"));
        assert!(git::git_output(&root, &["rev-parse", "refs/heads/worktree"]).is_ok());
    }

    #[test]
    fn script_launch_matches_the_entrypoint_only() {
        for args in [
            "node /opt/bin/codex --resume",
            "node /opt/node_modules/@anthropic-ai/claude-code/cli.js",
            "node /opt/node_modules/@google/gemini-cli/dist/index.js",
            "python3 -m aider --message test",
        ] {
            assert!(script_agent(args), "{args}");
        }
        for args in [
            "vim /tmp/codex",
            "node /tmp/service.js codex",
            "python3 -m service aider",
        ] {
            assert!(!script_agent(args), "{args}");
        }
    }

    #[test]
    fn lsof_fields_preserve_spaces_and_ignore_non_paths() {
        assert_eq!(
            lsof_directories("p123\nn/repo/a b\np124\nn/repo/other\nnpermission denied\n"),
            vec![PathBuf::from("/repo/a b"), PathBuf::from("/repo/other")]
        );
    }

    #[test]
    fn deletion_guard_blocks_only_a_matching_worktree() {
        let agents = ActiveAgents {
            directories: vec![PathBuf::from("/repo/wt/src")],
            available: true,
        };
        assert!(agents.ensure_unused(Some("/repo/wt")).is_err());
        assert!(agents.ensure_unused(Some("/repo/other")).is_ok());
        assert!(agents.ensure_unused(None).is_ok());
    }

    #[test]
    fn unavailable_inspection_blocks_worktree_deletion() {
        let agents = ActiveAgents::default();
        assert!(agents.ensure_unused(Some("/repo/wt")).is_err());
        assert!(agents.ensure_unused(None).is_ok());
    }

    #[test]
    fn recognizes_cli_processes_without_matching_arguments_or_apps() {
        for line in [
            "123 /usr/local/bin/codex",
            "123 /Users/me/Library/Application Support/Claude/claude.app/Contents/MacOS/claude",
            "123 claude bg-pty-host",
            "123 /usr/bin/opencode",
            "123 gemini",
            "123 aider",
            "123 grok",
            "123 /Users/me/.local/share/claude/versions/2.1.302 --resume /tmp/session.jsonl",
        ] {
            assert_eq!(agent_pid(line), Some(123), "{line}");
        }
        for line in [
            "123 /Applications/Codex.app/Contents/MacOS/Codex",
            "123 /usr/bin/vim",
            "123 codex-code-mode-host",
            "123 bash",
            "bad claude",
        ] {
            assert_eq!(agent_pid(line), None, "{line}");
        }
    }

    #[test]
    fn cwd_protects_its_worktree_and_subdirectories_only() {
        let agents = ActiveAgents {
            directories: vec![PathBuf::from("/repo/wt/src")],
            available: true,
        };
        assert!(agents.uses(Path::new("/repo/wt")));
        assert!(agents.uses(Path::new("/repo/wt/src")));
        assert!(!agents.uses(Path::new("/repo/wt-other")));
        assert!(!agents.uses(Path::new("/repo/other")));
    }

    #[test]
    fn active_worktree_directory_protects_its_matching_branch_name() {
        let agents = ActiveAgents {
            directories: vec![PathBuf::from("/repo/worktrees/sixeight-tda-2902/src")],
            available: true,
        };
        let worktrees = ["/repo/worktrees/sixeight-tda-2902", "/repo/worktrees/other"];
        assert!(agents.uses_branch("sixeight-tda-2902", &worktrees));
        assert!(!agents.uses_branch("sixeight-tda-2962", &worktrees));
        assert!(
            agents
                .ensure_branch_unused("sixeight-tda-2902", &worktrees)
                .is_err()
        );
        assert!(
            agents
                .ensure_branch_unused("sixeight-tda-2962", &worktrees)
                .is_ok()
        );
    }

    #[test]
    fn cwd_matches_worktree_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        #[cfg(unix)]
        {
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&wt, &alias).unwrap();
            let agents = ActiveAgents {
                directories: vec![wt.join("src")],
                available: true,
            };
            assert!(agents.uses(&alias));
        }
    }
}
