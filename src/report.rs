use console::{Style, Term, measure_text_width, strip_ansi_codes, truncate_str};

use crate::candidate::{Judged, Source, VerdictKind};
use crate::reason::Reason;

pub fn print_table(items: &[Judged]) {
    let color = console::colors_enabled_stderr();
    let table = render_table_with(items, color);
    let output = if let Some((_, cols)) = Term::stderr().size_checked() {
        let width = usize::from(cols).saturating_sub(1);
        if table_fits(&table, width) {
            table
        } else {
            render_compact_with(items, width, color)
        }
    } else {
        table
    };
    eprint!("{output}");
}

fn table_fits(table: &str, width: usize) -> bool {
    table
        .lines()
        .all(|line| measure_text_width(&strip_ansi_codes(line)) <= width)
}

#[cfg(test)]
fn render_table(items: &[Judged]) -> String {
    render_table_with(items, false)
}

pub fn render_table_with(items: &[Judged], color: bool) -> String {
    let mut rows: Vec<&Judged> = items.iter().filter(|i| !i.candidate.is_head).collect();
    rows.sort_by_cached_key(|item| sort_key(item));

    let name_width = rows
        .iter()
        .map(|i| display_name(i).chars().count().min(40))
        .max()
        .unwrap_or(6)
        .clamp(6, 40);
    let wt_width = rows
        .iter()
        .map(|i| wt_label(i).chars().count())
        .max()
        .unwrap_or(0)
        .max("WORKTREE".len());

    let mut out = String::new();
    if rows.is_empty() {
        return out;
    }
    let head = Style::new().dim().underlined();
    out.push_str(&render_row(
        [name_width, wt_width],
        [
            "VERDICT", "WHY", "BRANCH", "UNIQUE", "IDLE", "WORKTREE", "REMOTE", "PR",
        ]
        .map(|cell| (cell.to_string(), head.clone())),
        Some(("LAST COMMIT", head.clone())),
        color,
    ));
    for item in &rows {
        let palette = Palette::for_item(item);
        out.push_str(&render_row(
            [name_width, wt_width],
            [
                (
                    verdict_label(item.verdict.kind).to_string(),
                    palette.verdict,
                ),
                (why_label(item), palette.why),
                (truncate(&display_name(item), 40), palette.branch),
                (unique_label(item), palette.unique),
                (idle_label(item), palette.muted.clone()),
                (wt_label(item), palette.worktree),
                (gone_label(item).to_string(), palette.remote),
                (pr_label(item), palette.pr),
            ],
            item.candidate
                .last_subject
                .as_deref()
                .map(|subject| (subject, palette.muted.clone())),
            color,
        ));
    }
    out
}

fn render_compact_with(items: &[Judged], width: usize, color: bool) -> String {
    let mut rows: Vec<&Judged> = items
        .iter()
        .filter(|item| !item.candidate.is_head)
        .collect();
    rows.sort_by_cached_key(|item| sort_key(item));

    let mut out = String::new();
    for item in rows {
        let palette = Palette::for_item(item);
        let verdict = verdict_label(item.verdict.kind);
        let heading_prefix = format!(" {verdict}  ");
        let name_width = width
            .saturating_sub(measure_text_width(&heading_prefix))
            .max(1);
        let display_name = display_name(item);
        let name = truncate_str(&display_name, name_width, "…");
        out.push(' ');
        out.push_str(&paint(verdict, &palette.verdict, color));
        out.push_str("  ");
        out.push_str(&name);
        out.push('\n');

        let c = &item.candidate;
        let mut details = vec![
            format!("why: {}", why_label(item)),
            format!("{} unique", unique_label(item)),
            format!("idle: {}", idle_label(item)),
            format!("worktree: {}", nonempty_or_dash(wt_label(item))),
        ];
        if c.upstream_gone {
            details.push("remote: gone".into());
        }
        let pr = pr_label(item);
        if !pr.is_empty() {
            details.push(format!("PR: {pr}"));
        }
        append_wrapped_line(&mut out, &details.join(" · "), width, "   ");

        if let Some(subject) = c
            .last_subject
            .as_deref()
            .filter(|subject| !subject.is_empty())
        {
            append_wrapped_line(
                &mut out,
                &format!("last commit: {}", truncate(subject, 40)),
                width,
                "   ",
            );
        }
    }
    out
}

fn append_wrapped_line(out: &mut String, text: &str, width: usize, indent: &str) {
    let width = width.max(1);
    let continuation = format!("{indent}  ");
    let mut line = indent.to_string();
    let mut columns = measure_text_width(indent);
    let mut has_text = false;

    for word in text.split_whitespace() {
        let space_width = usize::from(has_text);
        let word_width = measure_text_width(word);
        if has_text && columns + space_width + word_width > width {
            out.push_str(&line);
            out.push('\n');
            line = continuation.clone();
            columns = measure_text_width(&line);
            has_text = false;
        }
        if has_text {
            line.push(' ');
            columns += 1;
        }
        for ch in word.chars() {
            let char_width = measure_text_width(&ch.to_string());
            if columns + char_width > width && has_text {
                out.push_str(&line);
                out.push('\n');
                line = continuation.clone();
                columns = measure_text_width(&line);
            }
            line.push(ch);
            columns += char_width;
            has_text = true;
        }
    }

    if !line.trim().is_empty() {
        out.push_str(&line);
        out.push('\n');
    }
}

fn nonempty_or_dash(value: String) -> String {
    if value.is_empty() { "-".into() } else { value }
}

struct Palette {
    verdict: Style,
    why: Style,
    branch: Style,
    unique: Style,
    worktree: Style,
    remote: Style,
    pr: Style,
    muted: Style,
}

impl Palette {
    fn for_item(item: &Judged) -> Self {
        let kept = item.verdict.kind == VerdictKind::Keep;
        let accent = match item.verdict.kind {
            VerdictKind::Delete => Style::new().red(),
            VerdictKind::Ask => Style::new().yellow(),
            VerdictKind::Keep => Style::new().green(),
        };
        let dim_if_kept = |style: Style| if kept { style.dim() } else { style };
        Palette {
            verdict: if kept {
                accent.clone().dim()
            } else {
                accent.clone().bold()
            },
            why: dim_if_kept(accent),
            branch: if kept {
                Style::new().dim()
            } else {
                Style::new().bold()
            },
            unique: if item.candidate.unique_commit_count.unwrap_or(0) > 0 {
                dim_if_kept(Style::new().cyan())
            } else {
                Style::new().dim()
            },
            worktree: dim_if_kept(if item.candidate.worktree_dirty {
                Style::new().yellow()
            } else {
                Style::new().blue()
            }),
            remote: dim_if_kept(Style::new().red()),
            pr: dim_if_kept(match pr_label(item).trim_start_matches('~') {
                "merged" => Style::new().magenta(),
                "open" => Style::new().green(),
                "closed" => Style::new().red(),
                _ => Style::new(),
            }),
            muted: Style::new().dim(),
        }
    }
}

pub(crate) fn paint(text: &str, style: &Style, color: bool) -> String {
    if color && !text.is_empty() {
        style.clone().force_styling(true).apply_to(text).to_string()
    } else {
        text.to_string()
    }
}

fn render_row(
    [name_width, wt_width]: [usize; 2],
    cells: [(String, Style); 8],
    subject: Option<(&str, Style)>,
    color: bool,
) -> String {
    let widths = [7, 12, name_width, 6, 4, wt_width, 6, 7];
    let mut line = String::from(" ");
    for ((cell, style), width) in cells.iter().zip(widths) {
        line.push(' ');
        line.push_str(&paint(cell, style, color));
        line.push_str(&" ".repeat(width.saturating_sub(cell.chars().count())));
        line.push(' ');
    }
    if let Some((subject, style)) = subject
        && !subject.is_empty()
    {
        line.push(' ');
        line.push_str(&paint(&truncate(subject, 40), &style, color));
    }
    let mut line = line.trim_end().to_string();
    line.push('\n');
    line
}

/// What needs attention comes first: deletes, then questions, then keeps.
/// Inside a verdict, certain before guessed (or, for keeps, what may become a
/// candidate next before what is protected), then the longest idle first.
pub(crate) fn sort_key(item: &Judged) -> (u8, u8, std::cmp::Reverse<Option<u64>>, String) {
    let verdict = match item.verdict.kind {
        VerdictKind::Delete => 0,
        VerdictKind::Ask => 1,
        VerdictKind::Keep => 2,
    };
    let group = match (item.verdict.kind, &item.verdict.reason) {
        (VerdictKind::Keep, Reason::OpenPr) => 1,
        (
            VerdictKind::Keep,
            Reason::CurrentHead
            | Reason::DefaultBranch
            | Reason::PrimaryWorktree
            | Reason::KeepConfigured,
        ) => 2,
        (VerdictKind::Keep, _) => 0,
        (_, _) if item.verdict.source == Source::Hard => 0,
        (_, _) => 1,
    };
    // `Reverse(None)` sorts after every `Reverse(Some(_))`: unknown age goes last
    (
        verdict,
        group,
        std::cmp::Reverse(item.candidate.ref_age_secs),
        display_name(item),
    )
}

fn verdict_label(kind: VerdictKind) -> &'static str {
    match kind {
        VerdictKind::Delete => "delete",
        VerdictKind::Ask => "ask",
        VerdictKind::Keep => "keep",
    }
}

fn display_name(item: &Judged) -> String {
    item.candidate.display_name()
}

pub(crate) fn why_label(item: &Judged) -> String {
    let label = item.verdict.reason.short().to_string();
    if item.verdict.kind == VerdictKind::Ask
        && let Some(score) = item.verdict.score
    {
        let pct = (score * 100.0).round().clamp(0.0, 100.0) as u32;
        return format!("{label} {pct}%");
    }
    label
}

fn unique_label(item: &Judged) -> String {
    match item.candidate.unique_commit_count {
        Some(n) => n.to_string(),
        None if !item.candidate.diverged => "0".into(),
        None => "-".into(),
    }
}

fn idle_label(item: &Judged) -> String {
    format_age(item.candidate.ref_age_secs).unwrap_or_else(|| "-".into())
}

const WT_LABEL_MAX: usize = 20;

fn wt_label(item: &Judged) -> String {
    let c = &item.candidate;
    let Some(dir) = c.worktree_dir_name() else {
        return String::new();
    };
    let dirty = if c.worktree_dirty { "*" } else { "" };
    let name = if c.is_primary_worktree {
        ".".to_string()
    } else {
        dir
    };
    format!("{}{dirty}", truncate(&name, WT_LABEL_MAX - dirty.len()))
}

fn gone_label(item: &Judged) -> &'static str {
    if item.candidate.upstream_gone {
        "gone"
    } else {
        ""
    }
}

fn pr_label(item: &Judged) -> String {
    let Some(pr) = &item.candidate.pr else {
        return String::new();
    };
    let moved = if item.candidate.has_moved_off_pr_head() {
        "~"
    } else {
        ""
    };
    format!("{moved}{}", pr.state_label())
}

/// "1 branch", "3 branches"
pub(crate) fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

pub fn format_age(secs: Option<u64>) -> Option<String> {
    let secs = secs?;
    if secs < 60 {
        Some("<1m".into())
    } else if secs < 3600 {
        Some(format!("{}m", secs / 60))
    } else if secs < 86400 {
        Some(format!("{}h", secs / 3600))
    } else {
        Some(format!("{}d", secs / 86400))
    }
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

pub fn print_json(items: &[Judged]) -> anyhow::Result<()> {
    let payload: Vec<_> = items
        .iter()
        .map(|item| {
            serde_json::json!({
                "branch": item.candidate.branch,
                "worktree": item.candidate.worktree_path,
                "verdict": item.verdict.kind,
                "source": item.verdict.source,
                "reason": item.verdict.reason,
                "score": item.verdict.score,
                "unique": item.candidate.unique_commit_count,
                "local_only": item.candidate.local_only_commit_count,
                "idle": format_age(item.candidate.ref_age_secs),
                "last_subject": item.candidate.last_subject,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Candidate, Judged, Source, Verdict, VerdictKind};
    use crate::reason::Reason;

    fn judged(kind: VerdictKind, reason: Reason, branch: &str) -> Judged {
        Judged {
            candidate: Candidate {
                branch: Some(branch.into()),
                last_commit_at: Some("2026-09-10".into()),
                last_subject: Some("wip: leftover".into()),
                unique_subjects: vec![],
                behind: Some(12),
                unique_commit_count: Some(0),
                diffstat: None,
                merged: true,
                diverged: false,
                sha: Some("abc".into()),
                ref_age_secs: Some(5 * 86400),
                ..crate::candidate::fixtures::candidate()
            },
            verdict: Verdict {
                kind,
                source: Source::Jev,
                reason,
                score: (kind == VerdictKind::Ask).then_some(0.52),
            },
        }
    }

    #[test]
    fn format_age_buckets() {
        assert_eq!(format_age(Some(12)), Some("<1m".into()));
        assert_eq!(format_age(Some(120)), Some("2m".into()));
        assert_eq!(format_age(Some(7200)), Some("2h".into()));
        assert_eq!(format_age(Some(5 * 86400)), Some("5d".into()));
        assert_eq!(format_age(None), None);
    }

    #[test]
    fn pr_state_has_no_prefix() {
        for (state, label) in [("MERGED", "merged"), ("OPEN", "open"), ("CLOSED", "closed")] {
            let mut item = judged(
                VerdictKind::Keep,
                Reason::UniqueWorkWouldBeLost,
                "sixeight/x",
            );
            item.candidate.pr = Some(crate::candidate::PrInfo {
                state: crate::candidate::PrState::parse(state).expect("a PR state"),
                title: "t".into(),
                merged_at: None,
                head_sha: None,
                number: None,
            });
            assert_eq!(pr_label(&item), label);
        }
    }

    #[test]
    fn wt_gone_pr_each_take_one_column() {
        let bare = judged(
            VerdictKind::Keep,
            Reason::UniqueWorkWouldBeLost,
            "sixeight/aaa",
        );
        let mut full = judged(
            VerdictKind::Keep,
            Reason::UniqueWorkWouldBeLost,
            "sixeight/bbb",
        );
        full.candidate.worktree_path = Some("/tmp/wt".into());
        full.candidate.upstream_gone = true;
        full.candidate.pr = Some(crate::candidate::PrInfo {
            state: crate::candidate::PrState::Merged,
            title: "t".into(),
            merged_at: None,
            head_sha: None,
            number: None,
        });
        let mut pr_only = judged(
            VerdictKind::Keep,
            Reason::UniqueWorkWouldBeLost,
            "sixeight/ccc",
        );
        pr_only.candidate.pr = Some(crate::candidate::PrInfo {
            state: crate::candidate::PrState::Open,
            title: "t".into(),
            merged_at: None,
            head_sha: None,
            number: None,
        });

        let table = render_table(&[bare, full, pr_only]);
        let lines: Vec<&str> = table.lines().skip(1).collect();
        assert!(!table.contains("pr:"));
        let (wt, gone, pr) = (
            lines[1].find("wt").unwrap(),
            lines[1].find("gone").unwrap(),
            lines[1].find("merged").unwrap(),
        );
        assert!(wt < gone && gone < pr);
        let subject_col: Vec<usize> = lines.iter().map(|l| l.find("wip:").unwrap()).collect();
        assert!(subject_col.iter().all(|c| *c == subject_col[0]));
        assert_eq!(lines[1].find("merged"), lines[2].find("open"));
    }

    #[test]
    fn header_names_each_column() {
        let mut item = judged(
            VerdictKind::Delete,
            Reason::GoneMergedPr,
            "sixeight/shipped",
        );
        item.candidate.worktree_path = Some("/tmp/wt".into());
        item.candidate.upstream_gone = true;
        item.candidate.pr = Some(crate::candidate::PrInfo {
            state: crate::candidate::PrState::Merged,
            title: "t".into(),
            merged_at: None,
            head_sha: None,
            number: None,
        });
        let table = render_table(&[item]);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 2);
        for (head, value) in [
            ("VERDICT", "delete"),
            ("WHY", "shipped "),
            ("BRANCH", "sixeight/shipped"),
            ("UNIQUE", "0 "),
            ("IDLE", "5d"),
            ("WORKTREE", "wt"),
            ("REMOTE", "gone"),
            ("PR", "merged"),
            ("LAST COMMIT", "wip:"),
        ] {
            assert_eq!(lines[0].find(head), lines[1].find(value), "{head}");
        }
    }

    #[test]
    fn colors_do_not_change_the_layout() {
        let mut shipped = judged(
            VerdictKind::Delete,
            Reason::GoneMergedPr,
            "sixeight/shipped",
        );
        shipped.candidate.worktree_path = Some("/tmp/wt".into());
        shipped.candidate.upstream_gone = true;
        shipped.candidate.pr = Some(crate::candidate::PrInfo {
            state: crate::candidate::PrState::Merged,
            title: "t".into(),
            merged_at: None,
            head_sha: None,
            number: None,
        });
        let items = vec![
            shipped,
            judged(
                VerdictKind::Ask,
                Reason::UncertainLeftoverEmptyRef,
                "sixeight/review",
            ),
            judged(
                VerdictKind::Keep,
                Reason::UniqueWorkWouldBeLost,
                "sixeight/real-work",
            ),
        ];
        let plain = render_table(&items);
        let colored = render_table_with(&items, true);
        assert!(colored.contains("\u{1b}["));
        assert!(!plain.contains("\u{1b}["));
        assert_eq!(console::strip_ansi_codes(&colored), plain);
    }

    #[test]
    fn worktree_column_shows_the_directory() {
        let mut item = judged(VerdictKind::Delete, Reason::Merged, "sixeight/fix-login");
        assert_eq!(wt_label(&item), "");

        item.candidate.worktree_path = Some("/Users/me/src/karu-fix-login".into());
        assert_eq!(wt_label(&item), "karu-fix-login");

        item.candidate.worktree_dirty = true;
        assert_eq!(wt_label(&item), "karu-fix-login*");

        item.candidate.is_primary_worktree = true;
        assert_eq!(wt_label(&item), ".*");
        item.candidate.worktree_dirty = false;
        assert_eq!(wt_label(&item), ".");
    }

    #[test]
    fn long_worktree_names_are_truncated_and_stay_aligned() {
        let mut long = judged(
            VerdictKind::Keep,
            Reason::UniqueWorkWouldBeLost,
            "sixeight/aaa",
        );
        long.candidate.worktree_path = Some("/tmp/a-very-long-worktree-directory-name-here".into());
        long.candidate.worktree_dirty = true;
        let short = judged(
            VerdictKind::Keep,
            Reason::UniqueWorkWouldBeLost,
            "sixeight/bbb",
        );

        assert_eq!(wt_label(&long).chars().count(), 20);
        assert!(wt_label(&long).ends_with("…*"));

        let table = render_table(&[long, short]);
        let cols: Vec<usize> = table
            .lines()
            .map(|l| {
                let at = l.find("LAST COMMIT").or_else(|| l.find("wip:")).unwrap();
                l[..at].chars().count()
            })
            .collect();
        assert!(cols.iter().all(|c| *c == cols[0]), "{table}");
    }

    #[test]
    fn pr_state_is_marked_when_the_branch_is_not_at_the_pr_head() {
        let mut item = judged(VerdictKind::Ask, Reason::UncertainThrowawayPath, "claude/x");
        item.candidate.sha = Some("local".into());
        item.candidate.pr = Some(crate::candidate::PrInfo {
            state: crate::candidate::PrState::Merged,
            title: "t".into(),
            merged_at: None,
            head_sha: Some("local".into()),
            number: None,
        });
        assert_eq!(pr_label(&item), "merged");

        item.candidate.pr.as_mut().unwrap().head_sha = Some("remote".into());
        assert_eq!(pr_label(&item), "~merged");

        item.candidate.pr.as_mut().unwrap().head_sha = None;
        assert_eq!(pr_label(&item), "merged");
    }

    #[test]
    fn closed_pr_delete_is_labelled_closed() {
        let item = judged(VerdictKind::Delete, Reason::ClosedPr, "sixeight/x");
        assert_eq!(why_label(&item), "closed");
    }

    fn aged(
        kind: VerdictKind,
        reason: Reason,
        source: Source,
        branch: &str,
        days: Option<u64>,
    ) -> Judged {
        let mut item = judged(kind, reason, branch);
        item.verdict.source = source;
        item.candidate.ref_age_secs = days.map(|d| d * 86400);
        item
    }

    fn branch_order(items: &[Judged]) -> Vec<String> {
        render_table(items)
            .lines()
            .skip(1)
            .map(|line| {
                line.split_whitespace()
                    .find(|w| w.starts_with("b/"))
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn rows_are_ordered_by_what_needs_attention_first() {
        use Source::{Hard, Jev};
        use VerdictKind::{Ask, Delete, Keep};
        let items = vec![
            aged(Keep, Reason::KeepConfigured, Hard, "b/protected", Some(90)),
            aged(Keep, Reason::OpenPr, Hard, "b/open-new", Some(1)),
            aged(Keep, Reason::OpenPr, Hard, "b/open-old", Some(30)),
            aged(Keep, Reason::UniqueWorkWouldBeLost, Jev, "b/work", Some(2)),
            aged(
                Keep,
                Reason::CommitsOnlyLocal,
                Jev,
                "b/unpushed-old",
                Some(6),
            ),
            aged(
                Ask,
                Reason::UncertainThrowawayPath,
                Jev,
                "b/ask-jev",
                Some(40),
            ),
            aged(Ask, Reason::Stale, Hard, "b/ask-stale", Some(8)),
            aged(
                Delete,
                Reason::WorkAlreadyLanded,
                Jev,
                "b/del-jev",
                Some(50),
            ),
            aged(Delete, Reason::MergedPr, Hard, "b/del-new", Some(1)),
            aged(Delete, Reason::MergedPr, Hard, "b/del-old", Some(20)),
            aged(Delete, Reason::Merged, Hard, "b/del-unknown-age", None),
        ];
        assert_eq!(
            branch_order(&items),
            [
                // delete: certain ones first, oldest first, unknown age last
                "b/del-old",
                "b/del-new",
                "b/del-unknown-age",
                "b/del-jev",
                // ask: same idea
                "b/ask-stale",
                "b/ask-jev",
                // keep: what may become a candidate next, then open PRs, then protected
                "b/unpushed-old",
                "b/work",
                "b/open-old",
                "b/open-new",
                "b/protected",
            ]
        );
    }

    #[test]
    fn order_does_not_depend_on_input_order() {
        use Source::Hard;
        let mut items = vec![
            aged(VerdictKind::Keep, Reason::OpenPr, Hard, "b/b", Some(3)),
            aged(VerdictKind::Keep, Reason::OpenPr, Hard, "b/a", Some(3)),
            aged(VerdictKind::Keep, Reason::OpenPr, Hard, "b/c", None),
        ];
        let forward = branch_order(&items);
        items.reverse();
        assert_eq!(branch_order(&items), forward);
        assert_eq!(forward, ["b/a", "b/b", "b/c"]);
    }

    #[test]
    fn empty_table_has_no_header() {
        assert_eq!(render_table(&[]), "");
    }

    #[test]
    fn table_is_one_aligned_list() {
        let items = vec![
            judged(
                VerdictKind::Keep,
                Reason::UniqueWorkWouldBeLost,
                "sixeight/real-work",
            ),
            judged(
                VerdictKind::Delete,
                Reason::LeftoverEmptyRef,
                "sixeight-pr-123",
            ),
            judged(
                VerdictKind::Ask,
                Reason::UncertainLeftoverEmptyRef,
                "sixeight/review",
            ),
            {
                let mut item = judged(VerdictKind::Keep, Reason::CurrentHead, "main");
                item.candidate.is_head = true;
                item.verdict.source = Source::Hard;
                item
            },
        ];
        let table = render_table(&items);
        assert!(!table.contains("delete  1"));
        assert!(!table.contains("will be removed"));
        let lines: Vec<&str> = table.lines().skip(1).collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("delete"));
        assert!(lines[0].contains("empty"));
        assert!(lines[0].contains("sixeight-pr-123"));
        assert!(lines[1].contains("ask"));
        assert!(lines[1].contains("empty? 52%"));
        assert!(lines[1].contains("sixeight/review"));
        assert!(lines[2].contains("keep"));
        assert!(table.contains("sixeight/real-work"));
        assert!(!table.contains("main"));
        let v_col: Vec<usize> = lines
            .iter()
            .map(|l| {
                l.find("delete")
                    .or_else(|| l.find("ask"))
                    .or_else(|| l.find("keep"))
                    .unwrap()
            })
            .collect();
        assert!(v_col.iter().all(|c| *c == v_col[0]));
        let why0 = lines[0].find("empty").unwrap();
        let why1 = lines[1].find("empty?").unwrap();
        assert_eq!(why0, why1);
    }
}
