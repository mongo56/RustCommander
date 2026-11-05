//! The Network list (`net:`, `vfs::network_path`): the computers found on the
//! network, as folders that open their share list (`\\host`). Opening it
//! (button after the drive buttons, `Network` command) lists the computers
//! known from the last search at once and searches again in the background
//! (`rc_platform::net_discover::discover`, ~4 s), adding each one as it is
//! found. The hosts are kept in `App::net` and handed to `vfs` (which lists
//! them) whenever they change.

use super::*;
use iced::widget::image;
use rc_platform::net_discover::{self, Host};

#[derive(Debug, Clone)]
pub enum NetMsg {
    /// The button of `side` (after the drive buttons) was clicked.
    Open(usize),
    /// Search `gen` found a computer.
    Found(u64, Host),
    /// Search `gen` ended (or was cancelled).
    Done(u64),
}

/// The computers known and the search running, if any.
#[derive(Default)]
pub(super) struct NetSearch {
    /// From the last searches, one per name, in the order found.
    hosts: Vec<Host>,
    /// Names found by the running search (a complete search drops the
    /// computers it didn't find).
    found: Vec<String>,
    /// Cancel flag of the running search.
    cancel: Option<Arc<AtomicBool>>,
    /// Number of the latest search; messages of older ones are dropped.
    gen: u64,
}

impl NetSearch {
    pub(super) fn searching(&self) -> bool {
        self.cancel.is_some()
    }

    /// Stops the running search (a new one, quitting).
    pub(super) fn cancel(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Adds `host` (true if it is new).
    fn add(&mut self, host: Host) -> bool {
        self.found.push(host.name.clone());
        let before = self.hosts.len();
        self.hosts = net_discover::merge(std::mem::take(&mut self.hosts).into_iter().chain([host]));
        self.hosts.len() != before
    }

    /// The search ended: keeps only the computers it found.
    fn finish(&mut self) {
        let found = std::mem::take(&mut self.found);
        self.hosts.retain(|h| found.iter().any(|f| f.eq_ignore_ascii_case(&h.name)));
    }

    fn names(&self) -> Vec<String> {
        self.hosts.iter().map(|h| h.name.clone()).collect()
    }
}

/// The line shown when a search found nothing.
pub(super) const NONE_FOUND: &str = r"No computers found; type \\name in the path bar";

impl App {
    /// The `Network` command or button: shows the Network list in `side`
    /// and searches the network again.
    pub(super) fn open_network(&mut self, side: usize) -> Task<Message> {
        self.leave_edits();
        self.focus_panel(side);
        if vfs::is_network(&self.panel(side).path) {
            return self.start_net_search();
        }
        let nav = self.start_nav(side, Nav::to(vfs::network_path()));
        // Arriving there starts a search (`net_arrived`) unless one runs.
        let search = if self.net.searching() { Task::none() } else { self.start_net_search() };
        Task::batch([nav, search])
    }

    /// A panel arrived at the Network list from elsewhere (Backspace from a
    /// server, history, typed `net:`): search unless a search runs.
    pub(super) fn net_arrived(&mut self) -> Task<Message> {
        if self.net.searching() { Task::none() } else { self.start_net_search() }
    }

    /// Starts a new search (cancelling the running one) on a worker; each
    /// computer found is a `NetMsg::Found`, the end a `NetMsg::Done`.
    pub(super) fn start_net_search(&mut self) -> Task<Message> {
        self.net.cancel();
        self.net.found.clear();
        self.net.gen += 1;
        let gen = self.net.gen;
        let cancel = Arc::new(AtomicBool::new(false));
        self.net.cancel = Some(cancel.clone());
        let (tx, rx) = iced::futures::channel::mpsc::unbounded();
        let spawned = std::thread::Builder::new().name("net-search".into()).spawn(move || {
            net_discover::discover(&cancel, |h| {
                let _ = tx.unbounded_send(Some(h));
            });
            let _ = tx.unbounded_send(None);
        });
        if spawned.is_err() {
            self.net.cancel = None;
            self.error("Can't start the network search");
            return Task::none();
        }
        Task::run(rx, move |found| match found {
            Some(h) => Message::Net(NetMsg::Found(gen, h)),
            None => Message::Net(NetMsg::Done(gen)),
        })
    }

    pub(super) fn net_update(&mut self, msg: NetMsg) -> Task<Message> {
        match msg {
            NetMsg::Open(side) => return self.open_network(side),
            NetMsg::Found(gen, host) => {
                if gen == self.net.gen && self.net.add(host) {
                    self.net_relist();
                }
            }
            NetMsg::Done(gen) => {
                if gen != self.net.gen {
                    return Task::none();
                }
                // Only a search that ran to its end says who is gone.
                if self.net.cancel.take().is_some_and(|c| !c.load(Ordering::Relaxed)) {
                    self.net.finish();
                }
                self.net_relist();
                let shown = (0..2).any(|side| vfs::is_network(&self.panel(side).path));
                if shown && self.net.hosts.is_empty() {
                    self.info(NONE_FOUND);
                }
            }
        }
        Task::none()
    }

    /// Hands the hosts to `vfs` and re-lists every tab on the Network list
    /// (keeping cursor and selection).
    fn net_relist(&mut self) {
        vfs::set_network_hosts(self.net.names());
        let entries = vfs::list(&vfs::network_path(), true).unwrap_or_default();
        for tab in self.sides.iter_mut().flat_map(|s| s.tabs.iter_mut()) {
            if vfs::is_network(&tab.path) && tab.results().is_none() {
                tab.apply_listing(entries.clone());
            }
        }
        self.after_change();
    }
}

/// The Network button's picture: a globe (outline, equator, two latitudes,
/// an ellipse of meridians), drawn once at 32 px with smooth edges.
pub(super) fn globe_icon() -> image::Handle {
    static ICON: std::sync::OnceLock<image::Handle> = std::sync::OnceLock::new();
    ICON.get_or_init(|| {
        const N: usize = 32;
        let (rgb, c, r) = ([0x3a, 0x8f, 0xd9], 15.5f32, 13.5f32);
        let mut px = vec![0u8; N * N * 4];
        for y in 0..N {
            for x in 0..N {
                let (dx, dy) = (x as f32 - c, y as f32 - c);
                let d = (dx * dx + dy * dy).sqrt();
                // Distance to the nearest line of the drawing.
                let mut dist = (d - r).abs();
                if d < r {
                    let meridian = ((dx / (r * 0.45)).powi(2) + (dy / r).powi(2)).sqrt();
                    dist = dist
                        .min(dy.abs())
                        .min((dy.abs() - r * 0.5).abs())
                        .min((meridian - 1.0).abs() * r * 0.45);
                }
                let alpha = (1.6 - dist).clamp(0.0, 1.0);
                let i = (y * N + x) * 4;
                px[i..i + 3].copy_from_slice(&rgb);
                px[i + 3] = (alpha * 255.0) as u8;
            }
        }
        image::Handle::from_rgba(N as u32, N as u32, px)
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_platform::net_discover::Via;

    fn host(name: &str) -> Host {
        Host { name: name.into(), address: None, via: Via::Mdns }
    }

    #[test]
    fn a_complete_search_drops_the_computers_gone() {
        let mut net = NetSearch::default();
        assert!(net.add(host("nas")) && net.add(host("pc")));
        assert!(!net.add(host("NAS")), "same name in another case");
        net.finish();
        assert_eq!(net.names(), ["nas", "pc"]);
        // The next search only finds the PC.
        assert!(!net.add(host("PC")));
        net.finish();
        assert_eq!(net.names(), ["pc"]);
    }

    #[test]
    fn globe_is_drawn() {
        let _ = globe_icon();
    }
}
