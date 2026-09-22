use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use console::Style;

use crate::candidate::Judged;
use crate::report::{count, format_age, paint, why_label};

pub mod terminal;

#[derive(Debug, thiserror::Error)]
#[error("cancelled; no selected deletions were applied")]
pub(crate) struct Cancelled;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Delete,
    Keep,
    /// Stop asking; whatever was not confirmed yet stays.
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Diff,
    Log,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Delete,
    Keep,
    Show(View),
}

/// The one rule for a confirmation, shared by the bulk prompt and the cards.
pub(crate) fn is_yes(line: &str) -> bool {
    matches!(line.trim(), "y" | "Y" | "yes")
}

/// Only a plain yes deletes; anything unclear is asked again.
fn parse_answer(line: &str) -> Option<Answer> {
    match line.trim() {
        answer if is_yes(answer) => Some(Answer::Delete),
        "" | "n" | "N" | "no" => Some(Answer::Keep),
        "d" => Some(Answer::Show(View::Diff)),
        "l" => Some(Answer::Show(View::Log)),
        "s" => Some(Answer::Show(View::Status)),
        _ => None,
    }
}

const SUBJECTS_SHOWN: usize = 4;

#[cfg(test)]
fn render_card(item: &Judged, index: usize, total: usize) -> String {
    render_card_with(item, index, total, false)
}

/// Branch, or the worktree when the checkout has no branch.
fn item_name(item: &Judged) -> String {
    let c = &item.candidate;
    let worktree_name = c.worktree_dir_name();
    match (&c.branch, &worktree_name) {
        (Some(branch), _) => branch.clone(),
        (None, Some(dir)) => format!("(detached) {dir}"),
        (None, None) => c.display_name(),
    }
}

/// Header, then the one thing that matters (what deleting would lose), then
/// just enough of the content to recognise the branch.
pub fn render_card_with(item: &Judged, index: usize, total: usize, color: bool) -> String {
    let c = &item.candidate;
    let paint = |text: String, style: Style| paint(&text, &style, color);
    let name = item_name(item);

    let mut tags = vec![why_label(item)];
    tags.extend(format_age(c.ref_age_secs));
    if let (Some(_), Some(dir)) = (&c.branch, &c.worktree_dir_name()) {
        tags.push(format!("wt {dir}"));
    }
    if c.upstream_gone {
        tags.push("remote gone".into());
    }
    let mut lines = vec![
        String::new(),
        format!(
            "  {}  {}",
            paint(format!("[{index}/{total}] {name}"), Style::new().bold()),
            paint(tags.join(" · "), Style::new().dim()),
        ),
    ];

    let mut lost = Vec::new();
    if let Some(n) = c.local_only_commit_count.filter(|n| *n > 0) {
        lost.push(count(n, "unpushed commit", "unpushed commits"));
    }
    if c.worktree_dirty {
        lost.push("uncommitted changes".into());
    }
    if !lost.is_empty() {
        lines.push(paint(
            format!("    would lose: {}", lost.join(", ")),
            Style::new().yellow(),
        ));
    } else if c.unique_commit_count == Some(0) {
        lines.push(paint(
            "    safe: no commits of its own".into(),
            Style::new().green(),
        ));
    } else if c.local_only_commit_count == Some(0) {
        lines.push(paint(
            "    safe: every commit is on a remote".into(),
            Style::new().green(),
        ));
    }

    if let Some(pr) = &c.pr {
        let number = pr.number.map(|n| format!("#{n} ")).unwrap_or_default();
        let moved = if c.has_moved_off_pr_head() {
            " (branch moved on)"
        } else {
            ""
        };
        lines.push(format!(
            "    PR {number}{}{moved} — {}",
            pr.state_label(),
            pr.title
        ));
    }

    let unique = c.unique_commit_count.unwrap_or(0);
    if unique > 0 {
        let diff = c
            .diffstat
            .as_deref()
            .and_then(compact_diffstat)
            .map(|d| format!(" · {d}"))
            .unwrap_or_default();
        lines.push(format!("    {}{diff}", count(unique, "commit", "commits")));

        let (merges, work): (Vec<&String>, Vec<&String>) = c
            .unique_subjects
            .iter()
            .partition(|s| s.starts_with("Merge "));
        let shown = work.len().min(SUBJECTS_SHOWN);
        for subject in &work[..shown] {
            lines.push(paint(format!("      {subject}"), Style::new().dim()));
        }
        let hidden = unique.saturating_sub(shown);
        if hidden > 0 {
            let merges = match merges.len() {
                0 => String::new(),
                n => format!(", incl. {}", count(n, "merge", "merges")),
            };
            lines.push(paint(
                format!("      (+{hidden} more{merges})"),
                Style::new().dim(),
            ));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

/// "3 files changed, 20 insertions(+), 4 deletions(-)" -> "3 files +20 −4"
fn compact_diffstat(diffstat: &str) -> Option<String> {
    let number_before = |word: &str| {
        let head = &diffstat[..diffstat.find(word)?];
        head.split_whitespace().next_back()?.parse::<u64>().ok()
    };
    let files = number_before(" file")?;
    let mut out = format!("{files} file{}", if files == 1 { "" } else { "s" });
    if let Some(added) = number_before(" insertion") {
        out.push_str(&format!(" +{added}"));
    }
    if let Some(removed) = number_before(" deletion") {
        out.push_str(&format!(" −{removed}"));
    }
    Some(out)
}

/// Shows the card once, then keeps asking until the answer is a decision.
/// End of input stops the questions instead of guessing.
pub fn ask_one(
    item: &Judged,
    index: usize,
    total: usize,
    input: &mut impl BufRead,
    out: &mut impl Write,
    show: impl FnMut(View),
) -> Result<Decision> {
    ask_one_with(
        MenuStyle {
            color: console::colors_enabled_stderr(),
            term_width: interactive_width(),
        },
        item,
        index,
        total,
        input,
        out,
        show,
    )
}

struct MenuStyle {
    color: bool,
    /// Columns when the menu and the typed answer share a terminal, so the
    /// answered menu can be cleared. `None` leaves the transcript as it is.
    term_width: Option<usize>,
}

/// Columns of the terminal the menu is drawn on, when the answer is typed
/// there too. A pipe keeps the transcript: clearing a menu the output is
/// not sharing would erase the card and leave the answer behind.
fn interactive_width() -> Option<usize> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin())
        || !std::io::IsTerminal::is_terminal(&std::io::stderr())
    {
        return None;
    }
    console::Term::stderr()
        .size_checked()
        .map(|(_, cols)| cols as usize)
        .filter(|cols| *cols > 0)
}

struct Choice {
    key: char,
    /// What the key does, in the words shown next to it.
    action: &'static str,
}

/// Keys that only display. Each word starts with its key, so the line can
/// read `(d)iff` rather than a key and a label. The answer is `[y/N]`, the
/// same token as the bulk confirmation: capital N, because Enter keeps.
fn choices(has_worktree: bool) -> Vec<Choice> {
    let mut choices = vec![
        Choice {
            key: 'd',
            action: "diff",
        },
        Choice {
            key: 'l',
            action: "log",
        },
    ];
    if has_worktree {
        choices.push(Choice {
            key: 's',
            action: "status",
        });
    }
    choices
}

/// The tools, then `[y/N]`, then the branch on the cursor line. The name
/// sits on the line being answered because a view above it can push the
/// card off the screen.
fn render_menu(
    item: &Judged,
    index: usize,
    total: usize,
    has_worktree: bool,
    color: bool,
    with_hint: bool,
) -> String {
    let cursor = paint(
        &format!("[{index}/{total}] {}", item_name(item)),
        &Style::new().bold(),
        color,
    );
    let mut menu = String::new();
    if with_hint {
        menu.push_str("  no such key\n");
    }
    let looks = choices(has_worktree)
        .iter()
        .map(|choice| keyed_word(choice, color))
        .collect::<Vec<_>>()
        .join("  ");
    menu.push_str(&format!("  {looks}    [y/N]\n"));
    menu.push_str(&format!("  {cursor} > "));
    menu
}

/// `(d)iff`: the key is the first letter, and it is the only bold part.
fn keyed_word(choice: &Choice, color: bool) -> String {
    let mut buf = [0; 4];
    let key: &str = choice.key.encode_utf8(&mut buf);
    let tail = choice.action.strip_prefix(key).unwrap_or(choice.action);
    format!("({}){}", paint(key, &Style::new().bold(), color), tail)
}

fn ask_one_with(
    style: MenuStyle,
    item: &Judged,
    index: usize,
    total: usize,
    input: &mut impl BufRead,
    out: &mut impl Write,
    mut show: impl FnMut(View),
) -> Result<Decision> {
    let has_worktree = item.candidate.worktree_path.is_some();
    let name = item_name(item);
    // Drawn into the next menu and then reset, so a bad key's hint leaves
    // with that menu instead of stacking under it.
    let mut with_hint = false;

    write!(out, "{}", render_card_with(item, index, total, style.color))?;
    loop {
        let menu = render_menu(item, index, total, has_worktree, style.color, with_hint);
        with_hint = false;
        write!(out, "{menu}")?;
        out.flush().ok();
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .context("failed to read the answer")?
            == 0
        {
            writeln!(out)?;
            writeln!(out, "  stop")?;
            return Ok(Decision::Quit);
        }
        let typed = line.trim_end_matches(['\n', '\r']);
        // The cursor line is the branch this answer is about. Once it has
        // been answered, a second copy still sitting above the view reads
        // as another question.
        if let Some(width) = style.term_width {
            let _ = replace_menu(menu_rows(&menu, typed, width));
        }
        match parse_answer(&line) {
            Some(Answer::Delete) => {
                writeln!(out, "  delete {name}")?;
                return Ok(Decision::Delete);
            }
            Some(Answer::Keep) => {
                writeln!(out, "  keep {name}")?;
                return Ok(Decision::Keep);
            }
            Some(Answer::Show(View::Status)) if !has_worktree => with_hint = true,
            Some(Answer::Show(view)) => show(view),
            None => with_hint = true,
        }
    }
}

/// Cursor sits on the line after the menu, which Enter just finished.
fn replace_menu(rows: usize) -> std::io::Result<()> {
    if rows == 0 {
        return Ok(());
    }
    let term = console::Term::stderr();
    term.move_cursor_up(rows)?;
    term.clear_to_end_of_screen()
}

/// Rows the menu occupies once `typed` has been echoed on its last line.
fn menu_rows(menu: &str, typed: &str, width: usize) -> usize {
    let mut rows = 0;
    let mut lines = menu.lines().peekable();
    while let Some(line) = lines.next() {
        let mut cols = console::measure_text_width(line);
        if lines.peek().is_none() {
            cols += console::measure_text_width(typed);
        }
        rows += if width == 0 || cols == 0 {
            1
        } else {
            cols.div_ceil(width)
        };
    }
    rows
}

const VIEW_MAX_LINES: usize = 60;

/// Prints the view right under the prompt. No pager: one that draws on the
/// alternate screen and exits at once leaves nothing to read, and an empty
/// result must not look like a dead key.
pub fn show(root: &Path, default_branch: &str, item: &Judged, view: View) {
    match load_view(
        root,
        default_branch,
        item,
        view,
        console::colors_enabled_stderr(),
    ) {
        Ok(raw) => eprint!(
            "{}",
            render_view(
                view,
                &raw,
                default_branch,
                &item_name(item),
                item.candidate.branch.as_deref().unwrap_or("HEAD"),
            )
        ),
        Err(err) => eprintln!("  warning: {err:#}"),
    }
}

pub(super) fn load_view(
    root: &Path,
    default_branch: &str,
    item: &Judged,
    view: View,
    color: bool,
) -> Result<String> {
    let c = &item.candidate;
    let (dir, rev) = match (&c.branch, &c.worktree_path) {
        (Some(branch), _) => (root.to_path_buf(), branch.clone()),
        (None, Some(path)) => (Path::new(path).to_path_buf(), "HEAD".to_string()),
        (None, None) => anyhow::bail!("the candidate has no branch or worktree"),
    };
    let color = if color {
        "--color=always"
    } else {
        "--color=never"
    };
    let mut git = Command::new("git");
    git.arg("--no-pager");
    match view {
        View::Log => git.current_dir(&dir).args([
            "log",
            color,
            "--oneline",
            "--no-decorate",
            &format!("{default_branch}..{rev}"),
        ]),
        View::Diff => git.current_dir(&dir).args([
            "diff",
            color,
            "--no-ext-diff",
            "--no-textconv",
            "--stat",
            "--patch",
            &format!("{default_branch}...{rev}"),
        ]),
        View::Status => {
            let path = c
                .worktree_path
                .as_deref()
                .context("the candidate has no worktree to inspect")?;
            git.current_dir(path).args(["status", "--short"])
        }
    };
    let context = || {
        format!(
            "failed to load {} for {}",
            view_label(view),
            item_name(item)
        )
    };
    let output = git.output().with_context(context)?;
    anyhow::ensure!(
        output.status.success(),
        "{}: git failed: {}",
        context(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) fn view_label(view: View) -> &'static str {
    match view {
        View::Diff => "diff",
        View::Log => "log",
        View::Status => "status",
    }
}

pub(super) fn empty_view_message(view: View, default_branch: &str) -> String {
    match view {
        View::Log => "(no commits of its own)".into(),
        View::Diff => format!("(no difference from {default_branch})"),
        View::Status => "(worktree is clean)".into(),
    }
}

fn render_view(view: View, raw: &str, default_branch: &str, name: &str, rev: &str) -> String {
    let title = format!("{} · {name}", view_label(view));
    let lines: Vec<&str> = raw.lines().collect();
    if lines.is_empty() {
        let note = empty_view_message(view, default_branch);
        return format!("\n  {title}\n    {note}\n\n");
    }

    let mut out = format!("\n  {title}\n");
    for line in lines.iter().take(VIEW_MAX_LINES) {
        out.push_str(&format!("    {line}\n"));
    }
    if lines.len() > VIEW_MAX_LINES {
        let rest = lines.len() - VIEW_MAX_LINES;
        let command = match view {
            View::Log => format!("git log {default_branch}..{rev}"),
            View::Diff => format!("git diff {default_branch}...{rev}"),
            View::Status => "git status".to_string(),
        };
        out.push_str(&format!("    ({rest} more lines: {command})\n"));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Candidate, Judged, PrInfo, Source, Verdict, VerdictKind};
    use crate::reason::Reason;

    fn asked() -> Judged {
        Judged {
            candidate: Candidate {
                branch: Some("sixeight/forgotten".into()),
                worktree_path: Some("/work/karu-forgotten".into()),
                last_commit_at: Some("2026-09-01".into()),
                last_subject: Some("feat: half done".into()),
                unique_subjects: vec!["feat: half done".into(), "wip".into()],
                behind: Some(40),
                unique_commit_count: Some(2),
                local_only_commit_count: Some(1),
                non_merge_unique_count: Some(2),
                diffstat: Some("3 files changed, 20 insertions(+), 4 deletions(-)".into()),
                worktree_dirty: true,
                upstream_gone: true,
                pr: Some(PrInfo {
                    state: crate::candidate::PrState::Closed,
                    title: "Add the forgotten thing".into(),
                    merged_at: None,
                    head_sha: Some("other".into()),
                    number: Some(42),
                }),
                sha: Some("abc".into()),
                ref_age_secs: Some(9 * 86400),
                ..crate::candidate::fixtures::candidate()
            },
            verdict: Verdict {
                kind: VerdictKind::Ask,
                source: Source::Hard,
                reason: Reason::Stale,
                score: None,
            },
        }
    }

    #[test]
    fn card_leads_with_what_would_be_lost() {
        let card = render_card(&asked(), 1, 3);
        let lines: Vec<&str> = card.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines,
            [
                "  [1/3] sixeight/forgotten  stale · 9d · wt karu-forgotten · remote gone",
                "    would lose: 1 unpushed commit, uncommitted changes",
                "    PR #42 closed (branch moved on) — Add the forgotten thing",
                "    2 commits · 3 files +20 −4",
                "      feat: half done",
                "      wip",
            ]
        );
    }

    #[test]
    fn card_says_so_when_nothing_would_be_lost() {
        let mut item = asked();
        item.candidate.local_only_commit_count = Some(0);
        item.candidate.worktree_dirty = false;
        let card = render_card(&item, 1, 1);
        assert!(
            card.contains("    safe: every commit is on a remote"),
            "{card}"
        );
        assert!(!card.contains("would lose"), "{card}");
    }

    #[test]
    fn card_stays_short_without_optional_facts() {
        let mut item = asked();
        item.candidate.pr = None;
        item.candidate.worktree_path = None;
        item.candidate.worktree_dirty = false;
        item.candidate.upstream_gone = false;
        item.candidate.diffstat = None;
        item.candidate.local_only_commit_count = None;
        let card = render_card(&item, 2, 2);
        let lines: Vec<&str> = card.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines,
            [
                "  [2/2] sixeight/forgotten  stale · 9d",
                "    2 commits",
                "      feat: half done",
                "      wip",
            ]
        );
    }

    #[test]
    fn long_histories_are_summarised() {
        let mut item = asked();
        item.candidate.unique_commit_count = Some(12);
        item.candidate.unique_subjects = vec![
            "Merge branch 'main' into x".into(),
            "one".into(),
            "two".into(),
            "Merge remote-tracking branch 'origin/main'".into(),
            "three".into(),
            "four".into(),
            "five".into(),
            "six".into(),
        ];
        let card = render_card(&item, 1, 1);
        for shown in ["one", "two", "three", "four"] {
            assert!(card.contains(&format!("      {shown}\n")), "{card}");
        }
        assert!(!card.contains("five"), "{card}");
        assert!(!card.contains("Merge branch"), "{card}");
        assert!(card.contains("      (+8 more, incl. 2 merges)"), "{card}");
    }

    #[test]
    fn jev_score_is_part_of_the_reason() {
        let mut item = asked();
        item.verdict.reason = Reason::UncertainLeftoverEmptyRef;
        item.verdict.score = Some(0.49);
        assert!(render_card(&item, 1, 1).contains("  empty? 49% · 9d"));
    }

    #[test]
    fn answers_are_parsed_leniently_but_yes_is_strict() {
        assert_eq!(parse_answer("y\n"), Some(Answer::Delete));
        assert_eq!(parse_answer("yes"), Some(Answer::Delete));
        assert_eq!(parse_answer("Y"), Some(Answer::Delete));
        assert_eq!(parse_answer("n"), Some(Answer::Keep));
        assert_eq!(parse_answer("\n"), Some(Answer::Keep));
        assert_eq!(parse_answer("d"), Some(Answer::Show(View::Diff)));
        assert_eq!(parse_answer("l"), Some(Answer::Show(View::Log)));
        assert_eq!(parse_answer("s"), Some(Answer::Show(View::Status)));
        for unclear in ["YES", "Y!", "delete", "q", "x"] {
            assert_eq!(parse_answer(unclear), None, "{unclear}");
        }
    }

    #[test]
    fn one_rule_decides_what_counts_as_yes() {
        for yes in ["y", "Y", "yes", "y\n", "  yes  "] {
            assert!(is_yes(yes), "{yes:?}");
            assert_eq!(parse_answer(yes), Some(Answer::Delete), "{yes:?}");
        }
        for no in ["YES", "Yes", "Y!", "n", "", "delete"] {
            assert!(!is_yes(no), "{no:?}");
        }
    }

    #[test]
    fn an_empty_view_says_so_instead_of_looking_dead() {
        for (view, expected) in [
            (View::Log, "(no commits of its own)"),
            (View::Diff, "(no difference from main)"),
            (View::Status, "(worktree is clean)"),
        ] {
            let shown = render_view(view, "", "main", "sixeight/x", "sixeight/x");
            assert!(shown.contains(expected), "{view:?}: {shown:?}");
            let title = match view {
                View::Log => "log · sixeight/x",
                View::Diff => "diff · sixeight/x",
                View::Status => "status · sixeight/x",
            };
            assert!(shown.contains(title), "{view:?}: {shown:?}");
        }
    }

    #[test]
    fn a_short_view_is_shown_as_is() {
        let shown = render_view(
            View::Log,
            "abc123 feat: one\n",
            "main",
            "sixeight/x",
            "sixeight/x",
        );
        assert_eq!(shown, "\n  log · sixeight/x\n    abc123 feat: one\n\n");
    }

    #[test]
    fn a_long_view_is_cut_with_a_way_to_see_the_rest() {
        let raw: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let shown = render_view(View::Diff, &raw, "main", "sixeight/x", "sixeight/x");
        assert!(shown.contains("line 0\n"));
        assert!(shown.contains(&format!("line {}\n", VIEW_MAX_LINES - 1)));
        assert!(!shown.contains(&format!("line {VIEW_MAX_LINES}\n")));
        assert!(
            shown.contains(&format!(
                "({} more lines: git diff main...sixeight/x)",
                500 - VIEW_MAX_LINES
            )),
            "{shown}"
        );
    }

    fn run(script: &str, item: &Judged) -> (Decision, Vec<View>, String) {
        let mut shown = Vec::new();
        let mut out = Vec::new();
        let decision = ask_one_with(
            MenuStyle {
                color: false,
                term_width: None,
            },
            item,
            1,
            1,
            &mut script.as_bytes(),
            &mut out,
            |view| shown.push(view),
        )
        .unwrap();
        (decision, shown, String::from_utf8(out).unwrap())
    }

    #[test]
    fn looking_around_does_not_decide_anything() {
        let (decision, shown, out) = run("l\nd\ns\ny\n", &asked());
        assert_eq!(decision, Decision::Delete);
        assert_eq!(shown, [View::Log, View::Diff, View::Status]);
        // The card once, then the cursor line of each menu that was shown.
        assert_eq!(out.matches("[1/1] sixeight/forgotten").count(), 5, "{out}");
        assert!(out.contains("delete sixeight/forgotten"), "{out}");
    }

    #[test]
    fn a_detached_worktree_is_named_on_the_cursor_line() {
        let mut item = asked();
        item.candidate.branch = None;
        item.candidate.worktree_path = Some("/work/newmo-app".into());
        let menu = render_menu(&item, 2, 2, true, false, false);
        assert!(menu.contains("[2/2] (detached) newmo-app > "), "{menu}");
        let card = render_card(&item, 2, 2);
        assert!(card.contains("[2/2] (detached) newmo-app  "), "{card}");
    }

    #[test]
    fn the_cursor_line_names_the_branch_being_answered() {
        let (_, _, out) = run("d\nn\n", &asked());
        let cursor = "[1/1] sixeight/forgotten > ";
        assert_eq!(out.matches(cursor).count(), 2, "{out}");
        assert!(out.contains("keep sixeight/forgotten\n"), "{out}");
    }

    #[test]
    fn an_unclear_answer_keeps_the_hint_on_the_menu_it_belongs_to() {
        let (_, _, out) = run("nope\nn\n", &asked());
        let hint_at = out.find("no such key").expect("hint");
        let second_menu = out[hint_at..]
            .find("[1/1] sixeight/forgotten > ")
            .expect("cursor");
        assert!(second_menu > 0, "{out}");
    }

    #[test]
    fn menu_rows_count_the_echoed_answer_and_ignore_color() {
        let menu = "  one\n  two\n  name > ";
        assert_eq!(menu_rows(menu, "", 80), 3);
        assert_eq!(menu_rows(menu, "d", 80), 3);
        // "  name > " is 9 columns; 12 more wrap it onto a second row at width 20.
        assert_eq!(menu_rows(menu, "abcdefghijkl", 20), 4);
        assert_eq!(
            menu_rows(
                "1234567890123456789012345678901234567890\n  name > ",
                "",
                20
            ),
            3
        );

        let plain = render_menu(&asked(), 1, 1, true, false, false);
        let colored = render_menu(&asked(), 1, 1, true, true, false);
        assert_ne!(plain, colored);
        assert_eq!(menu_rows(&plain, "d", 40), menu_rows(&colored, "d", 40));
    }

    #[test]
    fn unclear_input_asks_again_instead_of_guessing() {
        let (decision, _, out) = run("YES\nn\n", &asked());
        assert_eq!(decision, Decision::Keep);
        assert!(out.contains("no such key"), "{out}");
        assert!(out.contains("(d)iff  (l)og  (s)tatus    [y/N]"), "{out}");
        let menu = render_menu(&asked(), 1, 1, true, false, false);
        assert_eq!(
            menu.lines().count(),
            2,
            "the choices are one line above the cursor: {menu}"
        );
    }

    #[test]
    fn end_of_input_stops_without_deleting() {
        for script in ["", "l\n"] {
            assert_eq!(run(script, &asked()).0, Decision::Quit, "{script:?}");
        }
    }

    #[test]
    fn status_is_not_offered_without_a_worktree() {
        let mut item = asked();
        item.candidate.worktree_path = None;
        item.candidate.worktree_dirty = false;
        let (decision, shown, out) = run("s\nn\n", &item);
        assert_eq!(decision, Decision::Keep);
        assert!(shown.is_empty());
        assert!(!out.contains("(s)tatus"), "{out}");
        assert!(out.contains("no such key"), "{out}");
        assert!(out.contains("(d)iff  (l)og    [y/N]"), "{out}");
    }
}
