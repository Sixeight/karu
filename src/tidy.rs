use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use console::style;

use crate::apply::{self, Progress};
use crate::candidate::{Candidate, JevInput, Judged, Nouls, Verdict, VerdictKind};
use crate::collect::{self, CollectResult};
use crate::decide::{self, STALE_AFTER_SECS, jev_failed, jev_unavailable};
use crate::git;
use crate::interact;
use crate::reason::Reason;
use crate::report::{self, count};
use crate::spinner::{self, spinner};
use crate::timing;
use crate::typesafe::{self, Client};

pub struct Options {
    pub path: PathBuf,
    pub yes: bool,
    pub json: bool,
    pub fetch: bool,
    pub force: bool,
    pub api_key: Option<String>,
    /// `--no-jev`: never ask Jev, whatever the key and the config say.
    pub no_jev: bool,
}

pub fn run(opts: Options) -> Result<()> {
    // resolved once; the fetch, the PR lookup and the collection all need it
    let root = git::repo_root(&opts.path)?;
    if !git::has_head_commit(&root)? {
        note("current branch has no commits; nothing to delete");
        if opts.json {
            report::print_json(&[])?;
        }
        return Ok(());
    }
    let (collected, mut judged) = collect_and_judge(&root, &opts)?;

    timing::flush();
    if opts.json {
        report::print_json(&judged)?;
        return Ok(());
    }

    // finish deleting what an interrupted run left behind; never under --json
    apply::sweep_trash(&judged);

    // the table's order is also the order of the questions below
    judged.sort_by_cached_key(report::sort_key);
    report::print_table(&judged);

    let delete_count = judged
        .iter()
        .filter(|i| i.verdict.kind == VerdictKind::Delete)
        .count();
    let ask_count = judged
        .iter()
        .filter(|i| i.verdict.kind == VerdictKind::Ask)
        .count();
    if delete_count == 0 && ask_count == 0 {
        eprintln!();
        note("nothing to delete");
        return Ok(());
    }

    eprintln!();
    if interact::terminal::is_available() {
        if ask_count > 0 || (delete_count > 0 && !opts.yes) {
            interact::terminal::select_candidates(
                &collected.root,
                &collected.default_branch,
                &mut judged,
                !opts.yes,
            )?;
        }
    } else {
        if delete_count > 0 && !opts.yes && !apply::confirm(delete_count)? {
            for item in &mut judged {
                if item.verdict.kind == VerdictKind::Delete {
                    item.verdict.kind = VerdictKind::Keep;
                }
            }
        }
        if ask_count > 0 {
            let mut input = std::io::stdin().lock();
            let mut out = std::io::stderr();
            apply::confirm_asks(&mut judged, |item, index, total| {
                interact::ask_one(item, index, total, &mut input, &mut out, |view| {
                    interact::show(&collected.root, &collected.default_branch, item, view)
                })
            })?;
        }
    }

    let mut sp = None;
    let result = apply::apply_with(&collected.root, &judged, opts.force, |event| {
        let removing = |done: usize, total: usize| format!("Removing ({done}/{total})");
        match event {
            Progress::Started { total } => {
                if total > 0 {
                    sp = Some(spinner(removing(0, total)));
                }
            }
            Progress::Deleted {
                name,
                sha,
                done,
                total,
            } => {
                if let Some(sp) = &sp {
                    let line = match short_sha(sha) {
                        // the tip is what `git branch <name> <sha>` needs to undo this
                        Some(sha) => format!("Deleted {name} ({sha})"),
                        None => format!("Deleted {name}"),
                    };
                    sp.suspend(|| done_line(&line));
                    sp.set_message(removing(done, total));
                }
            }
            Progress::Failed {
                name,
                error,
                done,
                total,
            } => {
                if let Some(sp) = &sp {
                    sp.suspend(|| {
                        eprintln!("  {} Failed {name}", style("✘").for_stderr().red());
                        eprintln!("    {}", style(error).for_stderr().dim());
                    });
                    sp.set_message(removing(done, total));
                }
            }
        }
    });
    if let Some(sp) = sp {
        sp.finish_and_clear();
    }
    if result.deleted.is_empty() && result.errors.is_empty() {
        note("nothing deleted");
    }
    if result.trashed > 0 {
        note(&format!(
            "files of {} are being deleted in the background",
            count(result.trashed, "worktree", "worktrees")
        ));
    }
    if !result.errors.is_empty() {
        bail!(
            "failed to delete {} item(s):\n{}",
            result.errors.len(),
            result.errors.join("\n")
        );
    }
    Ok(())
}

/// Gathers the facts and settles every verdict. Four things wait on something
/// slow (the fetch, the PR lookup, `git status`, Jev), so they overlap:
///
/// - the PR lookup and the fetch start first;
/// - local facts are gathered while both run;
/// - Jev is asked as soon as the PRs are in, with the facts as they are;
/// - after the fetch, the facts it can change are read again, and Jev is asked
///   again only about candidates whose input actually changed.
fn collect_and_judge(root: &Path, opts: &Options) -> Result<(CollectResult, Vec<Judged>)> {
    std::thread::scope(|s| {
        let prs = s.spawn(|| collect::lookup_prs(root));
        let config = git::karu_config(root);
        let stale_after_secs = stale_secs(config.stale_days);
        // Sending branch names off the machine is opt-in per repository: a key
        // that happens to be in the environment is not by itself a choice
        // about karu.
        let enabled = !opts.no_jev && config.jev == Some(true);
        let api_key = opts.api_key.as_deref().filter(|_| enabled);
        // why it was skipped, in the order the user would say it: the flag
        // they typed, then the key, then the setting
        let (skipped, skipped_note) = if opts.no_jev {
            (
                Reason::JevOff,
                "jev skipped: --no-jev; leftover branches are kept",
            )
        } else if opts.api_key.is_none() {
            (
                Reason::JevNoApiKey,
                "jev skipped: TYPESAFE_API_KEY is unset; leftover branches are kept",
            )
        } else {
            (
                Reason::JevOff,
                "jev skipped: not enabled for this repository; leftover branches are kept. \
                 Turn it on with `git config karu.jev true`",
            )
        };

        let recent_fetch = opts
            .fetch
            .then(|| recent_enough(git::secs_since_last_fetch(root), config.fetch_max_age_secs))
            .flatten();
        if let Some(age) = recent_fetch {
            note(&format!(
                "fetch skipped: fetched {} ago (karu.fetchMaxAge)",
                report::format_age(Some(age)).unwrap_or_default()
            ));
        }
        let fetch = (opts.fetch && recent_fetch.is_none()).then(|| {
            s.spawn(|| {
                let started = Instant::now();
                (collect::fetch(root), started)
            })
        });

        // Names everything still running. A long fetch would otherwise be the
        // only thing on the line while collection and Jev are already going.
        let sp = spinner(phase_line(fetch.is_some(), true, None));
        // one expression, so the spinner is cleared on every way out
        let gathered = (|| -> Result<_> {
            let mut collected = collect::local_facts(root, &config)?;
            collected.attach_prs(prs.join().ok().flatten());
            if api_key.is_some() {
                collected.fill_diffstats(|_, c| {
                    decide::without_jev(c, opts.force, stale_after_secs).is_none()
                });
            }

            // `git status` cannot change who goes to Jev, so Jev goes first
            let mut early_n = None;
            let mut early_inputs = Vec::new();
            let early = api_key.and_then(|key| {
                let inputs = open_inputs(&collected.candidates, opts.force, stale_after_secs);
                let n = inputs.len();
                (n > 0).then(|| {
                    early_n = Some(n);
                    early_inputs = inputs.clone();
                    let default_branch = collected.default_branch.clone();
                    s.spawn(move || ask_jev(key, &default_branch, &inputs))
                })
            });
            show_phase(&sp, &fetch, true, &early, early_n);
            collected.mark_dirty_worktrees();
            // Collection is done. Fetch and the early Jev request may still be
            // running, and the line should say so instead of "collecting".
            show_phase(&sp, &fetch, false, &early, early_n);

            if let Some(fetch) = fetch
                && let Ok((fetched, started)) = fetch.join()
            {
                sp.suspend(|| {
                    timing::report("fetch", started);
                    match fetched {
                        Ok(()) => done_line("Fetched origin"),
                        Err(err) => eprintln!("warning: fetch failed: {err}"),
                    }
                });
                let judging = early
                    .as_ref()
                    .filter(|handle| !handle.is_finished())
                    .and(early_n);
                let line = phase_line(false, true, judging);
                if !line.is_empty() {
                    spinner::set_message(&sp, &line);
                }
                collected.reread_remote_facts()?;
            }
            Ok((collected, early_inputs, early))
        })();
        sp.finish_and_clear();
        let (mut collected, early_inputs, early) = gathered?;
        done_line(&format!(
            "Collected {}",
            count(collected.candidates.len(), "branch", "branches")
        ));

        let mut verdicts: Vec<Option<Verdict>> = collected
            .candidates
            .iter()
            .map(|c| decide::without_jev(c, opts.force, stale_after_secs))
            .collect();
        let open: Vec<usize> = (0..verdicts.len())
            .filter(|i| verdicts[*i].is_none())
            .collect();
        if open.is_empty() {
            // the fetch settled everything Jev was asked about early; the
            // scope waits for that request either way, so say what it is
            // waiting for instead of hanging on a cleared spinner
            if let Some(early) = early {
                let sp = spinner("Waiting for the judgment already sent".into());
                let _ = early.join();
                sp.finish_and_clear();
            }
        } else {
            let answers = match api_key {
                None => {
                    note(skipped_note);
                    None
                }
                Some(key) => {
                    collected.fill_diffstats(|_, c| {
                        decide::without_jev(c, opts.force, stale_after_secs).is_none()
                    });
                    let inputs: Vec<JevInput> = open
                        .iter()
                        .map(|i| collected.candidates[*i].jev_input())
                        .collect();
                    let sp = spinner(format!(
                        "Judging {}",
                        count(open.len(), "branch", "branches")
                    ));
                    let started = Instant::now();
                    let answers = settle_answers(
                        early.map(|handle| (early_inputs, handle)),
                        &inputs,
                        |missing| ask_jev(key, &collected.default_branch, missing),
                    );
                    sp.finish_and_clear();
                    timing::report("jev, waited after the fetch", started);
                    match &answers {
                        Ok(_) => done_line(&format!(
                            "Judged {}",
                            count(open.len(), "branch", "branches")
                        )),
                        Err(err) => {
                            eprintln!("warning: Jev failed: {err}; leftover branches will be kept")
                        }
                    }
                    Some(answers)
                }
            };
            for (n, i) in open.iter().enumerate() {
                let candidate = &collected.candidates[*i];
                verdicts[*i] = Some(match &answers {
                    None => jev_unavailable(skipped.clone()),
                    Some(Err(_)) => jev_failed(),
                    Some(Ok(nouls)) => decide::with_jev(candidate, nouls[n], opts.force),
                });
            }
        }

        if !opts.json {
            collected.fill_diffstats(|i, _| {
                verdicts[i]
                    .as_ref()
                    .is_some_and(|v| v.kind == VerdictKind::Ask)
            });
        }
        let judged = std::mem::take(&mut collected.candidates)
            .into_iter()
            .zip(verdicts)
            .map(|(candidate, verdict)| Judged {
                candidate,
                verdict: verdict.unwrap_or_else(jev_failed),
            })
            .collect();
        Ok((collected, judged))
    })
}

/// `git config karu.staleDays <n>` overrides the default; 0 turns the rule off.
fn stale_secs(configured_days: Option<u64>) -> u64 {
    match configured_days {
        Some(0) => u64::MAX,
        Some(days) => days.saturating_mul(24 * 60 * 60),
        None => STALE_AFTER_SECS,
    }
}

/// One status line for whatever is still in flight, in the order a run
/// starts them. Later clauses stay in the same sentence.
fn phase_line(fetching: bool, collecting: bool, judging: Option<usize>) -> String {
    let mut parts = Vec::new();
    if fetching {
        parts.push("Fetching origin".to_string());
    }
    if collecting {
        push_clause(&mut parts, "Collecting branches");
    }
    if let Some(n) = judging {
        push_clause(
            &mut parts,
            &format!("Judging {}", count(n, "branch", "branches")),
        );
    }
    parts.join(", ")
}

fn push_clause(parts: &mut Vec<String>, text: &str) {
    if parts.is_empty() {
        parts.push(text.to_string());
        return;
    }
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return;
    };
    let mut clause = first.to_lowercase().to_string();
    clause.push_str(chars.as_str());
    parts.push(clause);
}

/// `fetch` and `early` are still running when their handles say so. An empty
/// line means nothing is left to announce; the spinner is about to go away.
fn show_phase<T, U>(
    sp: &indicatif::ProgressBar,
    fetch: &Option<std::thread::ScopedJoinHandle<T>>,
    collecting: bool,
    early: &Option<std::thread::ScopedJoinHandle<U>>,
    early_n: Option<usize>,
) {
    let judging = early
        .as_ref()
        .filter(|handle| !handle.is_finished())
        .and(early_n);
    let line = phase_line(
        fetch.as_ref().is_some_and(|handle| !handle.is_finished()),
        collecting,
        judging,
    );
    if !line.is_empty() {
        spinner::set_message(sp, &line);
    }
}

/// What a finished phase leaves behind once its spinner is cleared, so the
/// steps stay readable above the table.
fn done_line(message: &str) {
    eprintln!("  {} {message}", style("✔").for_stderr().green());
}

/// The abbreviation git itself prints for a deleted branch's tip.
fn short_sha(sha: Option<&str>) -> Option<&str> {
    sha.filter(|sha| sha.len() >= 7).map(|sha| &sha[..7])
}

/// A quiet status line.
fn note(message: &str) {
    eprintln!("  {}", style(message).for_stderr().dim());
}

/// The age of the last fetch when it is recent enough to reuse. Off unless a
/// positive max age is configured; an unknown age is never recent.
fn recent_enough(age_secs: Option<u64>, max_age_secs: Option<u64>) -> Option<u64> {
    let (age, max) = (age_secs?, max_age_secs?);
    (max > 0 && age <= max).then_some(age)
}

/// What Jev would be shown for every candidate no rule settles.
fn open_inputs(candidates: &[Candidate], force: bool, stale_after_secs: u64) -> Vec<JevInput> {
    candidates
        .iter()
        .filter(|c| decide::without_jev(c, force, stale_after_secs).is_none())
        .map(Candidate::jev_input)
        .collect()
}

fn ask_jev(api_key: &str, default_branch: &str, inputs: &[JevInput]) -> Result<Vec<Nouls>> {
    Client::new(api_key).and_then(|client| typesafe::judge_all(&client, default_branch, inputs))
}

type PendingJudgment<'scope> = (
    Vec<JevInput>,
    std::thread::ScopedJoinHandle<'scope, Result<Vec<Nouls>>>,
);

/// Only identical inputs can reuse a judgment. Missing responses fail closed:
/// an early request has already spent its retries.
fn settle_answers(
    early: Option<PendingJudgment<'_>>,
    inputs: &[JevInput],
    ask: impl FnOnce(&[JevInput]) -> Result<Vec<Nouls>>,
) -> Result<Vec<Nouls>> {
    let pending: HashSet<&JevInput> = early
        .as_ref()
        .into_iter()
        .flat_map(|(asked, _)| asked)
        .collect();
    let missing: Vec<JevInput> = inputs
        .iter()
        .filter(|input| !pending.contains(*input))
        .cloned()
        .collect();
    let additional = if missing.is_empty() {
        Ok(Vec::new())
    } else {
        ask(&missing)
    };
    let mut known: HashMap<JevInput, Nouls> = match early {
        Some((asked, handle)) => {
            let nouls = handle
                .join()
                .map_err(|_| anyhow::anyhow!("judging thread panicked"))??;
            if nouls.len() < asked.len() {
                bail!("Jev returned fewer answers than it was asked");
            }
            asked.into_iter().zip(nouls).collect()
        }
        None => HashMap::new(),
    };
    let nouls = additional?;
    if nouls.len() < missing.len() {
        bail!("Jev returned fewer answers than it was asked");
    }
    known.extend(missing.into_iter().zip(nouls));
    inputs
        .iter()
        .map(|input| {
            known
                .get(input)
                .copied()
                .context("Jev returned fewer answers than it was asked")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(branch: &str) -> JevInput {
        Candidate {
            branch: Some(branch.into()),
            ..crate::candidate::fixtures::candidate()
        }
        .jev_input()
    }

    fn nouls(landed: f64) -> Nouls {
        Nouls {
            work_already_landed: landed,
            name_is_throwaway: 0.0,
            looks_abandoned: 0.0,
            unique_commits_are_noise: 0.0,
            empty_ref_is_leftover: 0.0,
        }
    }

    #[test]
    fn judgments_without_an_early_request_keep_input_order() {
        let inputs = [input("a"), input("b")];
        let answers = settle_answers(None, &inputs, |asked| {
            assert_eq!(asked, &inputs);
            Ok(vec![nouls(0.1), nouls(0.2)])
        })
        .unwrap();
        assert_eq!(answers, [nouls(0.1), nouls(0.2)]);
    }

    #[test]
    fn identical_inputs_reuse_answers_even_when_reordered() {
        let inputs = [input("b"), input("a")];
        let answers = std::thread::scope(|s| {
            let early = s.spawn(|| Ok(vec![nouls(0.1), nouls(0.2)]));
            settle_answers(Some((vec![input("a"), input("b")], early)), &inputs, |_| {
                panic!("identical inputs must not be sent twice")
            })
        })
        .unwrap();
        assert_eq!(answers, [nouls(0.2), nouls(0.1)]);
    }

    #[test]
    fn changed_inputs_start_before_the_early_request_finishes() {
        let mut changed = input("b");
        changed.local_only_commit_count = Some(0);
        let inputs = [input("c"), input("a"), changed.clone()];
        let (started, wait_for_additional) = std::sync::mpsc::channel();
        let answers = std::thread::scope(|s| {
            let early = s.spawn(move || {
                wait_for_additional
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("the changed inputs waited for the early response");
                Ok(vec![nouls(0.1), nouls(0.2), nouls(0.3)])
            });
            settle_answers(
                Some((vec![input("a"), input("b"), input("removed")], early)),
                &inputs,
                |asked| {
                    assert_eq!(asked, &[input("c"), changed]);
                    started.send(()).unwrap();
                    Ok(vec![nouls(0.8), nouls(0.9)])
                },
            )
        })
        .unwrap();
        assert_eq!(answers, [nouls(0.8), nouls(0.1), nouls(0.9)]);
    }

    #[test]
    fn the_early_response_can_finish_before_the_changed_response() {
        let (finished, wait_for_early) = std::sync::mpsc::channel();
        let answers = std::thread::scope(|s| {
            let early = s.spawn(move || {
                finished.send(()).unwrap();
                Ok(vec![nouls(0.1)])
            });
            settle_answers(
                Some((vec![input("a")], early)),
                &[input("b"), input("a")],
                |asked| {
                    assert_eq!(asked, &[input("b")]);
                    wait_for_early
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    Ok(vec![nouls(0.2)])
                },
            )
        })
        .unwrap();
        assert_eq!(answers, [nouls(0.2), nouls(0.1)]);
    }

    #[test]
    fn a_failed_early_request_is_not_retried() {
        let result = std::thread::scope(|s| {
            let early = s.spawn(|| anyhow::bail!("early request failed"));
            settle_answers(Some((vec![input("a")], early)), &[input("a")], |_| {
                panic!("a failed request must not be retried")
            })
        });
        assert_eq!(result.unwrap_err().to_string(), "early request failed");
    }

    #[test]
    fn a_failed_early_request_rejects_successful_changed_answers() {
        let result = std::thread::scope(|s| {
            let early = s.spawn(|| anyhow::bail!("early request failed"));
            settle_answers(Some((vec![input("a")], early)), &[input("b")], |asked| {
                assert_eq!(asked, &[input("b")]);
                Ok(vec![nouls(1.0)])
            })
        });
        assert_eq!(result.unwrap_err().to_string(), "early request failed");
    }

    #[test]
    fn a_failed_changed_request_rejects_successful_early_answers() {
        let result = std::thread::scope(|s| {
            let early = s.spawn(|| Ok(vec![nouls(1.0)]));
            settle_answers(
                Some((vec![input("a")], early)),
                &[input("a"), input("b")],
                |asked| {
                    assert_eq!(asked, &[input("b")]);
                    anyhow::bail!("changed request failed")
                },
            )
        });
        assert_eq!(result.unwrap_err().to_string(), "changed request failed");
    }

    #[test]
    fn a_panicked_early_request_fails_without_resending_inputs() {
        let result = std::thread::scope(|s| {
            let early = s.spawn(|| panic!("judgment panicked"));
            settle_answers(Some((vec![input("a")], early)), &[input("a")], |_| {
                panic!("a panicked request must not be retried")
            })
        });
        assert_eq!(result.unwrap_err().to_string(), "judging thread panicked");
    }

    #[test]
    fn missing_early_answers_fail_without_resending_inputs() {
        let result = std::thread::scope(|s| {
            let early = s.spawn(|| Ok(vec![nouls(1.0)]));
            settle_answers(
                Some((vec![input("a"), input("b")], early)),
                &[input("a"), input("b")],
                |_| panic!("a partial request must not be retried"),
            )
        });
        assert_eq!(
            result.unwrap_err().to_string(),
            "Jev returned fewer answers than it was asked"
        );
    }

    #[test]
    fn missing_changed_answers_reject_successful_early_answers() {
        let result = std::thread::scope(|s| {
            let early = s.spawn(|| Ok(vec![nouls(1.0)]));
            settle_answers(
                Some((vec![input("a")], early)),
                &[input("a"), input("b")],
                |_| Ok(Vec::new()),
            )
        });
        assert_eq!(
            result.unwrap_err().to_string(),
            "Jev returned fewer answers than it was asked"
        );
    }

    /// A detached worktree has no branch, and its tip is the only way back.
    #[test]
    fn a_tip_is_abbreviated_when_there_is_one() {
        assert_eq!(short_sha(Some("a1b2c3d4e5f6")), Some("a1b2c3d"));
        assert_eq!(short_sha(None), None);
        assert_eq!(short_sha(Some("abc")), None);
    }

    #[test]
    fn the_status_line_names_every_phase_still_running() {
        assert_eq!(
            phase_line(true, true, None),
            "Fetching origin, collecting branches"
        );
        assert_eq!(phase_line(false, true, None), "Collecting branches");
        assert_eq!(phase_line(true, false, None), "Fetching origin");
        assert_eq!(
            phase_line(true, true, Some(2)),
            "Fetching origin, collecting branches, judging 2 branches"
        );
        assert_eq!(
            phase_line(true, false, Some(1)),
            "Fetching origin, judging 1 branch"
        );
        assert_eq!(phase_line(false, false, Some(1)), "Judging 1 branch");
        assert_eq!(phase_line(false, false, None), "");
    }

    #[test]
    fn fetch_is_skipped_only_when_recent_enough_and_asked_for() {
        assert_eq!(recent_enough(Some(60), Some(300)), Some(60));
        assert_eq!(recent_enough(Some(300), Some(300)), Some(300));
        assert_eq!(recent_enough(Some(301), Some(300)), None);
        // off by default, and an unknown age never counts as recent
        assert_eq!(recent_enough(Some(1), None), None);
        assert_eq!(recent_enough(None, Some(300)), None);
        assert_eq!(recent_enough(Some(0), Some(0)), None);
    }
}
