use std::sync::Mutex;
use std::time::Instant;

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `KARU_TIMING=1 karu` shows where the time went. Lines are held until
/// `flush`, because a spinner may own the terminal line right now.
pub fn report(phase: &str, started: Instant) {
    if std::env::var_os("KARU_TIMING").is_some()
        && let Ok(mut lines) = LINES.lock()
    {
        lines.push(format!(
            "  timing: {:>6.2}s  {phase}",
            started.elapsed().as_secs_f64()
        ));
    }
}

pub fn flush() {
    if let Ok(mut lines) = LINES.lock() {
        for line in lines.drain(..) {
            eprintln!("{line}");
        }
    }
}
