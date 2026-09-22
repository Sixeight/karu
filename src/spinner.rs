use std::io;
use std::time::Duration;

use console::Term;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle, TermLike};

pub const SPINNER_TEMPLATE: &str = "  {prefix} {msg:.dim}{spinner:.dim}";

/// Columns the template adds around the message: "  ", the prefix, " ", and
/// the three dots.
const LINE_DECORATION: usize = 2 + 1 + 1 + 3;

fn spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .tick_strings(&["   ", ".  ", ".. ", "...", "   "])
        .template(SPINNER_TEMPLATE)
        .unwrap()
}

/// indicatif pads every line to the full terminal width and then clears it
/// assuming the cursor never left that row. A terminal that wraps as soon as
/// the last column is written, or that draws one character wider than
/// expected, breaks that assumption and leaves stale lines behind. Reporting
/// the terminal one column narrower keeps the last column untouched.
#[derive(Debug)]
struct NarrowedTerm(Term);

fn usable_width(real: u16) -> u16 {
    if real > 1 { real - 1 } else { real }
}

impl TermLike for NarrowedTerm {
    fn width(&self) -> u16 {
        usable_width(self.0.size().1)
    }
    fn height(&self) -> u16 {
        self.0.size().0
    }
    fn move_cursor_up(&self, n: usize) -> io::Result<()> {
        self.0.move_cursor_up(n)
    }
    fn move_cursor_down(&self, n: usize) -> io::Result<()> {
        self.0.move_cursor_down(n)
    }
    fn move_cursor_right(&self, n: usize) -> io::Result<()> {
        self.0.move_cursor_right(n)
    }
    fn move_cursor_left(&self, n: usize) -> io::Result<()> {
        self.0.move_cursor_left(n)
    }
    fn write_line(&self, s: &str) -> io::Result<()> {
        self.0.write_line(s)
    }
    fn write_str(&self, s: &str) -> io::Result<()> {
        self.0.write_str(s)
    }
    fn clear_line(&self) -> io::Result<()> {
        self.0.clear_line()
    }
    fn flush(&self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Cuts the message so the whole spinner line fits on one row. A line that
/// wraps needs cursor arithmetic to erase, which is the first thing to go
/// wrong on an unusual terminal.
fn fit_message(msg: &str, term_width: u16) -> String {
    let room = (usable_width(term_width) as usize).saturating_sub(LINE_DECORATION);
    if room == 0 {
        return String::new();
    }
    console::truncate_str(msg, room, "…").into_owned()
}

/// Replaces the spinner text, cut to the terminal the same way as [`spinner`].
pub fn set_message(pb: &ProgressBar, msg: &str) {
    let term = Term::stderr();
    let msg = if term.is_term() {
        fit_message(msg, term.size().1)
    } else {
        msg.to_string()
    };
    pb.set_message(msg);
}

pub fn spinner(msg: String) -> ProgressBar {
    let term = Term::stderr();
    let pb = if term.is_term() {
        let msg = fit_message(&msg, term.size().1);
        let pb = ProgressBar::with_draw_target(
            None,
            ProgressDrawTarget::term_like(Box::new(NarrowedTerm(term))),
        );
        pb.set_style(spinner_style());
        pb.set_prefix("ｶ");
        pb.set_message(msg);
        pb
    } else {
        ProgressBar::hidden()
    };
    pb.enable_steady_tick(Duration::from_millis(400));

    let pb2 = pb.clone();
    std::thread::spawn(move || {
        let mut karu = false;
        loop {
            std::thread::sleep(Duration::from_millis(120));
            if pb2.is_finished() {
                break;
            }
            karu = !karu;
            pb2.set_prefix(if karu { "ﾙ" } else { "ｶ" });
        }
    });

    pb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_is_cut_so_the_line_stays_on_one_row() {
        let long = "Removing sixeight/a-rather-long-branch-name-for-this-feature (1/2)";
        let fitted = fit_message(long, 40);
        assert!(fitted.ends_with('…'), "{fitted}");
        // decoration + message + dots must leave the last column free
        let line = LINE_DECORATION + console::measure_text_width(&fitted);
        assert!(line < 40, "{line} columns: {fitted}");
    }

    #[test]
    fn short_message_is_left_alone() {
        assert_eq!(fit_message("Fetching origin", 80), "Fetching origin");
    }

    #[test]
    fn wide_characters_count_by_columns_not_by_chars() {
        let fitted = fit_message(&"刈".repeat(40), 30);
        assert!(
            LINE_DECORATION + console::measure_text_width(&fitted) <= 29,
            "{fitted}"
        );
    }

    #[test]
    fn absurdly_narrow_terminal_does_not_panic() {
        assert_eq!(fit_message("Fetching origin", 3), "");
    }

    #[test]
    fn terminal_is_reported_one_column_narrower_than_it_is() {
        assert_eq!(usable_width(80), 79);
        assert_eq!(usable_width(1), 1);
        assert_eq!(usable_width(0), 0);
    }
}
