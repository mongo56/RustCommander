//! Watches the folders shown in the panels and reports changes, debounced,
//! so the panels refresh on their own.

use iced::futures::channel::mpsc;
use iced::Subscription;
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

/// Quiet time after the last change before a folder is reported.
const SETTLE: Duration = Duration::from_millis(150);
/// A folder that keeps changing (e.g. during a long copy) is still reported
/// at least this often.
const MAX_DELAY: Duration = Duration::from_millis(1000);

/// Yields a folder from `dirs` each time its contents changed (or watching it
/// failed, e.g. because it was deleted). Changing `dirs` restarts the watch.
pub fn folders(dirs: Vec<PathBuf>) -> Subscription<PathBuf> {
    Subscription::run_with_id(
        dirs.clone(),
        iced::stream::channel(16, move |output| async move {
            std::thread::spawn(move || watch_loop(dirs, output));
            // The thread ends once this stream is dropped (channel closed).
            std::future::pending::<()>().await
        }),
    )
}

/// Watches `dir` (not its subfolders); `pick` maps a change to the index of
/// the folder to report. Errors (e.g. the folder vanished) report it too.
fn watch(
    dir: &Path,
    tx: &std_mpsc::Sender<usize>,
    pick: impl Fn(&notify::Event) -> Option<usize> + Send + 'static,
) -> Option<notify::RecommendedWatcher> {
    let tx = tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let index = match res {
            Ok(event) if matches!(event.kind, EventKind::Access(_)) => None,
            Ok(event) => pick(&event),
            Err(_) => pick(&notify::Event::default()),
        };
        if let Some(i) = index {
            let _ = tx.send(i);
        }
    })
    .ok()?;
    watcher.watch(dir, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

fn watch_loop(dirs: Vec<PathBuf>, mut output: mpsc::Sender<PathBuf>) {
    let (tx, rx) = std_mpsc::channel::<usize>();
    let mut _watchers = Vec::new();
    for (i, dir) in dirs.iter().enumerate() {
        // One watcher per folder, so an event maps straight to its folder.
        let contents = watch(dir, &tx, move |_| Some(i));
        // Windows doesn't tell a watcher that its own folder was deleted or
        // renamed; the parent's watcher sees it.
        let own_path = dir.clone();
        let itself = dir
            .parent()
            .and_then(|parent| watch(parent, &tx, move |event| event.paths.contains(&own_path).then_some(i)));
        _watchers.extend(contents.into_iter().chain(itself));
    }

    // Folder index -> (first unreported change, latest change).
    let mut pending: HashMap<usize, (Instant, Instant)> = HashMap::new();
    loop {
        let now = Instant::now();
        let wait = pending
            .values()
            .map(|&(first, last)| (last + SETTLE).min(first + MAX_DELAY).saturating_duration_since(now))
            .min()
            .unwrap_or(Duration::from_millis(500));
        match rx.recv_timeout(wait) {
            Ok(i) => {
                let now = Instant::now();
                pending.entry(i).and_modify(|(_, last)| *last = now).or_insert((now, now));
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if output.is_closed() {
            return;
        }
        let now = Instant::now();
        pending.retain(|&i, &mut (first, last)| {
            let due = now >= last + SETTLE || now >= first + MAX_DELAY;
            if due {
                // A full channel means the UI is busy; it will get later events.
                let _ = output.try_send(dirs[i].clone());
            }
            !due
        });
    }
}
