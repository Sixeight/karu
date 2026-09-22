use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result};
use console::{Key, Style, Term};

use super::{
    Answer, Cancelled, Decision, View, empty_view_message, item_name, load_view, render_card_with,
    view_label,
};
use crate::candidate::Judged;
use crate::report::paint;

const MIN_ROWS: u16 = 12;
const MIN_COLS: u16 = 40;

pub fn is_available() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
        && Term::stderr()
            .size_checked()
            .is_some_and(|(rows, cols)| rows >= MIN_ROWS && cols >= MIN_COLS)
}

struct Prompt {
    term: Term,
    rows: usize,
    size: (u16, u16),
}

impl Prompt {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("failed to enter terminal input mode")?;
        let term = Term::stderr();
        let prompt = Self {
            size: term.size(),
            term,
            rows: 0,
        };
        prompt.term.hide_cursor()?;
        Ok(prompt)
    }

    fn clear(&mut self) -> std::io::Result<()> {
        let size = self.term.size();
        if size != self.size {
            // Reflow may have moved earlier output into the rows we occupied.
            self.term.write_str("\r\n")?;
        } else {
            self.term.clear_last_lines(self.rows)?;
        }
        self.rows = 0;
        self.size = size;
        Ok(())
    }

    fn draw(&mut self, lines: &[String]) -> Result<()> {
        self.clear()?;
        let (rows, cols) = self.size;
        let frame = lines
            .iter()
            .take(usize::from(rows).saturating_sub(2).max(1))
            .map(|line| console::truncate_str(line, usize::from(cols).saturating_sub(1), "…"))
            .collect::<Vec<_>>();
        self.term.write_str(&frame.join("\r\n"))?;
        self.term.write_str("\r\n")?;
        self.rows = frame.len();
        Ok(())
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        let _ = self.clear();
        let _ = self.term.show_cursor();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

struct Menu {
    actions: Vec<Answer>,
    selected: usize,
}

impl Menu {
    fn new(has_worktree: bool) -> Self {
        let mut actions = vec![
            Answer::Keep,
            Answer::Delete,
            Answer::Show(View::Diff),
            Answer::Show(View::Log),
        ];
        if has_worktree {
            actions.push(Answer::Show(View::Status));
        }
        Self {
            actions,
            selected: 0,
        }
    }

    fn key(&mut self, key: Key) -> Option<Answer> {
        match key {
            Key::ArrowUp | Key::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            Key::ArrowDown | Key::Char('j') => {
                self.selected = (self.selected + 1).min(self.actions.len() - 1);
            }
            Key::Char('n' | 'N') => self.selected = 0,
            Key::Char('y' | 'Y') => self.selected = 1,
            Key::Enter => return Some(self.actions[self.selected]),
            Key::Escape => return Some(Answer::Keep),
            Key::Char('d' | 'l' | 's') => {
                let view = match key {
                    Key::Char('d') => View::Diff,
                    Key::Char('l') => View::Log,
                    _ => View::Status,
                };
                if let Some(index) = self.actions.iter().position(|a| *a == Answer::Show(view)) {
                    self.selected = index;
                    return Some(Answer::Show(view));
                }
            }
            _ => {}
        }
        None
    }

    fn render(&self, item: &Judged, index: usize, total: usize, color: bool) -> Vec<String> {
        let mut lines = vec![format!(
            "  [{index}/{total}] {}",
            clean_line(&item_name(item))
        )];
        lines.push(String::new());
        for (i, action) in self.actions.iter().enumerate() {
            let label = match action {
                Answer::Keep => "[n] Keep",
                Answer::Delete => "[y] Delete",
                Answer::Show(View::Diff) => "[d] Diff",
                Answer::Show(View::Log) => "[l] Log",
                Answer::Show(View::Status) => "[s] Status",
            };
            let line = if i == self.selected {
                let style = if *action == Answer::Delete {
                    Style::new().red().bold()
                } else {
                    Style::new().cyan().bold()
                };
                paint(&format!("  > {label}"), &style, color)
            } else {
                format!("    {label}")
            };
            lines.push(line);
        }
        lines.push(String::new());
        lines.push("  ↑/↓ j/k move · Enter choose · Esc keep".into());
        lines.push("  d/l inspect · Ctrl-C cancel run".into());
        lines
    }
}

struct Detail {
    view: View,
    raw: String,
    lines: Vec<String>,
    width: usize,
    offset: usize,
}

impl Detail {
    fn new(view: View, raw: String) -> Self {
        Self {
            view,
            raw,
            lines: Vec::new(),
            width: 0,
            offset: 0,
        }
    }

    fn resize(&mut self, width: usize, page: usize) {
        if self.width != width {
            self.lines = self
                .raw
                .lines()
                .flat_map(|line| wrap_line(line, width))
                .collect();
            self.width = width;
        }
        self.offset = self.offset.min(self.lines.len().saturating_sub(page));
    }

    fn key(&mut self, key: Key, page: usize) -> bool {
        let end = self.lines.len().saturating_sub(page);
        match key {
            Key::Enter | Key::Escape | Key::Char('q') => return true,
            Key::ArrowUp | Key::Char('k') => self.offset = self.offset.saturating_sub(1),
            Key::ArrowDown | Key::Char('j') => self.offset = (self.offset + 1).min(end),
            Key::PageUp => self.offset = self.offset.saturating_sub(page),
            Key::PageDown | Key::Char(' ') => self.offset = (self.offset + page).min(end),
            Key::Home => self.offset = 0,
            Key::End => self.offset = end,
            _ => {}
        }
        false
    }

    fn render(
        &self,
        item: &Judged,
        index: usize,
        total: usize,
        page: usize,
        color: bool,
    ) -> Vec<String> {
        let mut lines = vec![
            format!(
                "  [{index}/{total}] {} · {}",
                clean_line(&item_name(item)),
                view_label(self.view)
            ),
            String::new(),
        ];
        for line in self.lines.iter().skip(self.offset).take(page) {
            let style = match line.chars().next() {
                Some('+') if self.view == View::Diff => Style::new().green(),
                Some('-') if self.view == View::Diff => Style::new().red(),
                _ => Style::new(),
            };
            lines.push(paint(line, &style, color));
        }
        lines.push(String::new());
        lines.push(format!(
            "  lines {}-{}/{} · ↑/↓ PgUp/PgDn Home/End",
            self.offset + 1,
            (self.offset + page).min(self.lines.len()),
            self.lines.len()
        ));
        lines.push("  Enter/Esc/q back · Ctrl-C cancel run".into());
        lines
    }
}

fn clean_line(line: &str) -> String {
    console::strip_ansi_codes(line)
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\t')
        .collect::<String>()
        .replace('\t', "    ")
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut columns = 0;
    for ch in clean_line(line).chars() {
        let mut buf = [0; 4];
        let size = console::measure_text_width(ch.encode_utf8(&mut buf));
        if columns + size > width.max(1) && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            columns = 0;
        }
        current.push(ch);
        columns += size;
    }
    lines.push(current);
    lines
}

pub fn ask_one(
    root: &Path,
    default_branch: &str,
    item: &Judged,
    index: usize,
    total: usize,
) -> Result<Decision> {
    let color = console::colors_enabled_stderr();
    eprint!("{}", render_card_with(item, index, total, color));
    let mut prompt = Prompt::enter()?;
    let mut menu = Menu::new(item.candidate.worktree_path.is_some());
    let mut detail: Option<Detail> = None;
    let decision = loop {
        let (rows, cols) = prompt.term.size();
        let fits = rows >= MIN_ROWS && cols >= MIN_COLS;
        let page = usize::from(rows).saturating_sub(7).clamp(1, 8);
        let lines = if !fits {
            vec!["Resize to 40x12. Esc keeps this branch.".into()]
        } else if let Some(detail) = &mut detail {
            detail.resize(usize::from(cols) - 1, page);
            detail.render(item, index, total, page, color)
        } else {
            menu.render(item, index, total, color)
        };
        prompt.draw(&lines)?;
        let key = prompt
            .term
            .read_key_raw()
            .context("failed to read the selection")?;
        if matches!(key, Key::CtrlC | Key::Char('\u{4}')) {
            return Err(Cancelled.into());
        }
        if prompt.term.size() != (rows, cols) {
            continue;
        }
        if !fits {
            if key == Key::Escape {
                if detail.is_some() {
                    detail = None;
                } else {
                    break Decision::Keep;
                }
            }
            continue;
        }
        if let Some(current) = &mut detail {
            if current.key(key, page) {
                detail = None;
            }
            continue;
        }
        match menu.key(key) {
            Some(Answer::Keep) => break Decision::Keep,
            Some(Answer::Delete) => break Decision::Delete,
            Some(Answer::Show(view)) => {
                let raw = match load_view(root, default_branch, item, view, false) {
                    Ok(raw) if raw.trim().is_empty() => empty_view_message(view, default_branch),
                    Ok(raw) => raw,
                    Err(error) => format!("warning: {error:#}"),
                };
                detail = Some(Detail::new(view, raw));
            }
            None => {}
        }
    };
    drop(prompt);
    let label = match decision {
        Decision::Delete => "delete",
        Decision::Keep => "keep",
        Decision::Quit => "stop",
    };
    eprintln!("  {label} {}", item_name(item));
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_needs_a_selection_and_enter() {
        let mut menu = Menu::new(false);
        assert_eq!(menu.key(Key::Enter), Some(Answer::Keep));
        assert_eq!(menu.key(Key::Char('y')), None);
        assert_eq!(menu.key(Key::Char('x')), None);
        assert_eq!(menu.key(Key::Enter), Some(Answer::Delete));
        assert_eq!(menu.key(Key::Escape), Some(Answer::Keep));
        menu.key(Key::Char('n'));
        assert_eq!(menu.key(Key::Enter), Some(Answer::Keep));
    }

    #[test]
    fn arrow_selection_can_inspect_without_deciding() {
        let mut menu = Menu::new(false);
        menu.key(Key::ArrowUp);
        assert_eq!(menu.key(Key::Enter), Some(Answer::Keep));
        menu.key(Key::ArrowDown);
        assert_eq!(menu.key(Key::Enter), Some(Answer::Delete));
        menu.key(Key::ArrowDown);
        assert_eq!(menu.key(Key::Enter), Some(Answer::Show(View::Diff)));
        assert_eq!(menu.key(Key::Char('l')), Some(Answer::Show(View::Log)));
        assert_eq!(menu.key(Key::Char('s')), None);
        assert_eq!(
            Menu::new(true).key(Key::Char('s')),
            Some(Answer::Show(View::Status))
        );
    }

    #[test]
    fn returning_from_a_view_does_not_confirm_a_delete() {
        let mut menu = Menu::new(false);
        menu.key(Key::Char('y'));
        assert_eq!(menu.key(Key::Char('d')), Some(Answer::Show(View::Diff)));
        let mut detail = Detail::new(View::Diff, "patch".into());
        assert!(!detail.key(Key::Char('y'), 10));
        assert!(detail.key(Key::Enter, 10));
        assert_eq!(menu.key(Key::Enter), Some(Answer::Show(View::Diff)));
    }

    #[test]
    fn every_detail_line_is_reachable_and_scrolling_is_bounded() {
        let mut detail = Detail::new(View::Log, (0..120).map(|n| format!("line {n}\n")).collect());
        detail.resize(79, 10);
        detail.key(Key::PageDown, 10);
        assert_eq!(detail.offset, 10);
        detail.key(Key::End, 10);
        assert_eq!(detail.lines[detail.offset + 9], "line 119");
        detail.key(Key::ArrowDown, 10);
        assert_eq!(detail.offset, 110);
        detail.key(Key::PageUp, 10);
        assert_eq!(detail.offset, 100);
        detail.resize(79, 200);
        assert_eq!(detail.offset, 0);
        detail.key(Key::Home, 10);
        detail.key(Key::ArrowUp, 10);
        assert_eq!(detail.offset, 0);
    }

    #[test]
    fn long_unicode_details_wrap_without_losing_content() {
        let raw = "日本語の変更 abcdef";
        let lines = wrap_line(raw, 8);
        assert_eq!(lines.concat(), raw);
        assert!(
            lines
                .iter()
                .all(|line| console::measure_text_width(line) <= 8)
        );
        assert_eq!(wrap_line("", 8), [""]);
        assert_eq!(clean_line("\x1b[31mred\x1b[0m\t\u{7}"), "red    ");
    }
}
