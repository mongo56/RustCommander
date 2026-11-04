//! Startup timing, for finding what makes the start slow. Off unless the
//! environment variable `RC_STARTUP_TRACE` is set: to a file path, the lines
//! are appended there (the release exe has no console); to `1`, they go to
//! stderr. Each line is the milliseconds since `main` started and a label.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

struct Trace {
    start: Instant,
    /// None: stderr.
    file: Option<std::path::PathBuf>,
}

static TRACE: OnceLock<Option<Trace>> = OnceLock::new();

/// Call first thing in `main`.
pub fn init() {
    TRACE.get_or_init(|| {
        let value = std::env::var_os("RC_STARTUP_TRACE")?;
        let file = (!value.is_empty() && value != "1").then(|| value.into());
        Some(Trace { start: Instant::now(), file })
    });
}

/// Logs `label` with the time since [`init`], when tracing is on.
pub fn mark(label: &str) {
    let Some(Some(t)) = TRACE.get() else { return };
    let line = format!("{:8.1} ms  {label}\n", t.start.elapsed().as_secs_f64() * 1000.0);
    match &t.file {
        Some(path) => {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = f.write_all(line.as_bytes());
            }
        }
        None => eprint!("{line}"),
    }
}

/// [`mark`], only the first time it's called with this `flag`.
pub fn mark_once(flag: &AtomicBool, label: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        mark(label);
    }
}
