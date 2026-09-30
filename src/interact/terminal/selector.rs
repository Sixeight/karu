use std::path::Path;

use anyhow::{Context, Result};
use console::{Key, Style, measure_text_width, truncate_str};

use super::super::{Cancelled, View, empty_view_message, item_name, load_view};
use super::{Detail, Prompt, clean_line};
use crate::candidate::{Judged, VerdictKind};
use crate::report::{format_age, paint, why_label};

const COMPACT_AT: usize = 72;
const WIDE_AT: usize = 100;

#[derive(Clone, Copy)]
enum Column {
    Branch,
    Why,
    Unique,
    Local,
    Idle,
    Worktree,
    Pr,
    Subject,
}

struct ColumnSpec {
    column: Column,
    heading: &'static str,
    width: usize,
    flex: bool,
    right_align: bool,
}

struct Layout {
    columns: Vec<ColumnSpec>,
}

impl Layout {
    fn new(cols: usize) -> Self {
        let mut columns = if cols >= WIDE_AT {
            vec![
                spec(Column::Branch, "BRANCH", 8, true, false),
                spec(Column::Why, "WHY", 12, false, false),
                spec(Column::Unique, "UNIQUE", 6, false, true),
                spec(Column::Local, "UNPUSHED", 8, false, true),
                spec(Column::Idle, "IDLE", 5, false, true),
                spec(Column::Worktree, "WORKTREE", 12, false, false),
                spec(Column::Pr, "PR", 7, false, false),
                spec(Column::Subject, "LAST COMMIT", 11, true, false),
            ]
        } else if cols >= COMPACT_AT {
            vec![
                spec(Column::Branch, "BRANCH", 8, true, false),
                spec(Column::Why, "WHY", 12, false, false),
                spec(Column::Unique, "UNIQUE", 6, false, true),
                spec(Column::Local, "UNPUSHED", 8, false, true),
                spec(Column::Worktree, "WORKTREE", 10, false, false),
                spec(Column::Pr, "PR", 7, false, false),
            ]
        } else {
            vec![
                spec(Column::Branch, "BRANCH", 8, true, false),
                spec(Column::Why, "WHY", 8, false, false),
                spec(Column::Local, "LOCAL", 4, false, true),
                spec(Column::Worktree, "WT", 5, false, false),
            ]
        };

        let gaps = columns.len().saturating_sub(1) * 2;
        let fixed = columns
            .iter()
            .filter(|column| !column.flex)
            .map(|column| column.width)
            .sum::<usize>();
        let fixed_prefix = 8 + gaps;
        let flex_count = columns.iter().filter(|column| column.flex).count();
        let flex_width = cols.saturating_sub(1).saturating_sub(fixed_prefix + fixed);
        let flex_indices: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter_map(|(index, column)| column.flex.then_some(index))
            .collect();
        if flex_count == 1 {
            let index = flex_indices[0];
            columns[index].width = columns[index].width.max(flex_width);
        } else if flex_count == 2 {
            let first = flex_indices[0];
            let second = flex_indices[1];
            let first_width = flex_width / 2;
            columns[first].width = columns[first].width.max(first_width);
            columns[second].width = columns[second]
                .width
                .max(flex_width.saturating_sub(columns[first].width));
        }

        Self { columns }
    }

    fn render_row(&self, item: &Judged, checked: bool, cursor: bool, color: bool) -> String {
        let marker = if cursor {
            paint("▸", &Style::new().cyan().bold(), color)
        } else {
            " ".into()
        };
        let mark_style = if checked {
            Style::new().red().bold()
        } else {
            Style::new().dim()
        };
        let mark = if checked { "[x]" } else { "[ ]" };
        let mut line = format!("  {marker} {}", paint(mark, &mark_style, color));

        for (index, column) in self.columns.iter().enumerate() {
            if index > 0 {
                line.push_str("  ");
            } else {
                line.push(' ');
            }
            let value = column_value(column.column, item);
            let plain = clean_line(&value);
            let clipped = truncate_str(&plain, column.width, "…").into_owned();
            let padding = column.width.saturating_sub(measure_text_width(&clipped));
            let style = column_style(column.column, item, cursor);
            if column.right_align {
                line.push_str(&" ".repeat(padding));
            }
            line.push_str(&paint(&clipped, &style, color));
            if !column.right_align {
                line.push_str(&" ".repeat(padding));
            }
        }
        line
    }

    fn render_header(&self, color: bool) -> String {
        let mut line = String::from("    DEL ");
        for (index, column) in self.columns.iter().enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            let clipped = truncate_str(column.heading, column.width, "…").into_owned();
            let padding = column.width.saturating_sub(measure_text_width(&clipped));
            if column.right_align {
                line.push_str(&" ".repeat(padding));
            }
            line.push_str(&paint(&clipped, &Style::new().dim().bold(), color));
            if !column.right_align {
                line.push_str(&" ".repeat(padding));
            }
        }
        line
    }
}

fn spec(
    column: Column,
    heading: &'static str,
    width: usize,
    flex: bool,
    right_align: bool,
) -> ColumnSpec {
    ColumnSpec {
        column,
        heading,
        width,
        flex,
        right_align,
    }
}

fn column_value(column: Column, item: &Judged) -> String {
    let candidate = &item.candidate;
    match column {
        Column::Branch => item_name(item),
        Column::Why => why_label(item),
        Column::Unique => candidate
            .unique_commit_count
            .map(|count| count.to_string())
            .unwrap_or_else(|| if candidate.diverged { "-" } else { "0" }.into()),
        Column::Local => candidate
            .local_only_commit_count
            .map(|count| count.to_string())
            .unwrap_or_else(|| "-".into()),
        Column::Idle => format_age(candidate.ref_age_secs).unwrap_or_else(|| "-".into()),
        Column::Worktree => candidate
            .worktree_dir_name()
            .map(|name| {
                let name = if candidate.is_primary_worktree {
                    ".".to_string()
                } else {
                    name
                };
                if candidate.worktree_dirty {
                    format!("{name}*")
                } else {
                    name
                }
            })
            .unwrap_or_else(|| {
                if candidate.worktree_dirty {
                    "dirty".into()
                } else {
                    "-".into()
                }
            }),
        Column::Pr => candidate
            .pr
            .as_ref()
            .map(|pr| {
                let moved = if candidate.has_moved_off_pr_head() {
                    "~"
                } else {
                    ""
                };
                format!("{moved}{}", pr.state_label())
            })
            .unwrap_or_else(|| "-".into()),
        Column::Subject => candidate.last_subject.clone().unwrap_or_else(|| "-".into()),
    }
}

fn column_style(column: Column, item: &Judged, cursor: bool) -> Style {
    match column {
        Column::Branch if cursor => Style::new().cyan().bold(),
        Column::Why => Style::new().yellow(),
        Column::Local
            if item
                .candidate
                .local_only_commit_count
                .is_some_and(|n| n > 0) =>
        {
            Style::new().red().bold()
        }
        Column::Worktree if item.candidate.worktree_dirty => Style::new().yellow().bold(),
        Column::Pr
            if item
                .candidate
                .pr
                .as_ref()
                .is_some_and(|pr| pr.state == crate::candidate::PrState::Open) =>
        {
            Style::new().green()
        }
        Column::Subject => Style::new().dim(),
        _ => Style::new(),
    }
}

struct SelectorDisplay {
    start: usize,
    capacity: usize,
    layout: Layout,
}

impl SelectorDisplay {
    fn new(terminal_size: (u16, u16), item_count: usize, cursor: usize) -> Self {
        let capacity = usize::from(terminal_size.0).saturating_sub(8);
        let start = viewport_start(item_count, cursor, capacity, 0);
        Self {
            start,
            capacity,
            layout: Layout::new(usize::from(terminal_size.1)),
        }
    }

    fn resize(&mut self, terminal_size: (u16, u16), item_count: usize, cursor: usize) {
        let capacity = usize::from(terminal_size.0).saturating_sub(8);
        self.start = viewport_start(item_count, cursor, capacity, self.start);
        self.capacity = capacity;
        self.layout = Layout::new(usize::from(terminal_size.1));
    }

    fn visible_end(&self, item_count: usize) -> usize {
        (self.start + self.capacity).min(item_count)
    }

    fn render(
        &self,
        items: &[&Judged],
        checked: &[bool],
        cursor: usize,
        color: bool,
    ) -> Vec<String> {
        let mut lines = vec![
            paint(
                "  Select branches · * dirty worktree",
                &Style::new().bold(),
                color,
            ),
            self.layout.render_header(color),
        ];
        for index in self.start..self.visible_end(items.len()) {
            lines.push(self.layout.render_row(
                items[index],
                checked[index],
                cursor == index,
                color,
            ));
        }
        let selected = checked.iter().filter(|value| **value).count();
        let position = if cursor == items.len() {
            "run".to_string()
        } else {
            format!("{}/{}", cursor + 1, items.len())
        };
        let footer = format!("{position} · {selected} selected");
        lines.push(if cursor == items.len() {
            paint(&format!("  ▸ {footer}"), &Style::new().bold(), color)
        } else {
            paint(&format!("    {footer}"), &Style::new().dim(), color)
        });
        lines.push("  ↑/↓ j/k move · Space toggle".into());
        lines.push("  Enter run · Esc/q keep · ^C cancel".into());
        lines.push("  d/l inspect · s status if available".into());
        lines
    }
}

fn viewport_start(
    item_count: usize,
    cursor: usize,
    capacity: usize,
    previous_start: usize,
) -> usize {
    if capacity == 0 {
        return item_count;
    }
    let last_start = item_count.saturating_sub(capacity);
    let previous_start = previous_start.min(last_start);
    if cursor >= item_count {
        last_start
    } else if cursor < previous_start {
        cursor
    } else if cursor >= previous_start + capacity {
        (cursor + 1).saturating_sub(capacity).min(last_start)
    } else {
        previous_start
    }
}

fn view_for_key(key: Key, has_worktree: bool) -> Option<View> {
    match key {
        Key::Char('d' | 'D') => Some(View::Diff),
        Key::Char('l' | 'L') => Some(View::Log),
        Key::Char('s' | 'S') if has_worktree => Some(View::Status),
        _ => None,
    }
}

pub(super) fn select_candidates(
    root: &Path,
    default_branch: &str,
    judged: &mut [Judged],
    include_recommendations: bool,
) -> Result<()> {
    let indices: Vec<usize> = judged
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item.verdict.kind == VerdictKind::Ask
                || (include_recommendations && item.verdict.kind == VerdictKind::Delete))
                .then_some(index)
        })
        .collect();
    if indices.is_empty() {
        return Ok(());
    }

    let items: Vec<&Judged> = indices.iter().map(|&index| &judged[index]).collect();
    let mut checked: Vec<bool> = items
        .iter()
        .map(|item| item.verdict.kind == VerdictKind::Delete)
        .collect();
    let mut cursor = items.len();
    let color = console::colors_enabled_stderr();
    let mut prompt = Prompt::enter()?;
    let mut display = SelectorDisplay::new(prompt.term.size(), items.len(), cursor);
    let mut detail: Option<Detail> = None;

    let selected = loop {
        let size = prompt.term.size();
        display.resize(size, items.len(), cursor);
        let (rows, cols) = size;
        let fits = rows >= super::MIN_ROWS && cols >= super::MIN_COLS;
        let page = usize::from(rows).saturating_sub(7).clamp(1, 8);
        let lines = if !fits {
            vec!["Resize to 40x12, or press Esc to keep all.".into()]
        } else if let Some(detail) = &mut detail {
            detail.resize(usize::from(cols).saturating_sub(1), page);
            detail.render(items[cursor], cursor + 1, items.len(), page, color)
        } else {
            display.render(&items, &checked, cursor, color)
        };
        prompt.draw(&lines)?;
        let key = prompt
            .term
            .read_key_raw()
            .context("failed to read the selection")?;
        if matches!(key, Key::CtrlC | Key::Char('\u{4}')) {
            return Err(Cancelled.into());
        }
        if prompt.term.size() != size {
            continue;
        }

        if let Some(current) = &mut detail {
            if current.key(key, page) {
                detail = None;
            }
            continue;
        }
        if !fits {
            if matches!(key, Key::Escape | Key::Char('q')) {
                break vec![false; checked.len()];
            }
            continue;
        }

        match key {
            Key::ArrowUp | Key::Char('k') => cursor = cursor.saturating_sub(1),
            Key::ArrowDown | Key::Char('j') => cursor = (cursor + 1).min(items.len()),
            Key::Char(' ') if cursor < items.len() => checked[cursor] = !checked[cursor],
            Key::Enter if cursor == items.len() => break checked,
            Key::Escape | Key::Char('q') => break vec![false; checked.len()],
            _ if cursor < items.len() => {
                if let Some(view) =
                    view_for_key(key, items[cursor].candidate.worktree_path.is_some())
                {
                    let raw = match load_view(root, default_branch, items[cursor], view, false) {
                        Ok(raw) if raw.trim().is_empty() => {
                            empty_view_message(view, default_branch)
                        }
                        Ok(raw) => raw,
                        Err(error) => format!("warning: {error:#}"),
                    };
                    detail = Some(Detail::new(view, raw));
                }
            }
            _ => {}
        }
    };

    drop(prompt);
    for (&index, selected) in indices.iter().zip(selected) {
        judged[index].verdict.kind = if selected {
            VerdictKind::Delete
        } else {
            VerdictKind::Keep
        };
    }
    Ok(())
}
