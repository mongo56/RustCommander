//! Finding computers on the local network that may offer SMB shares: Function
//! Discovery (WS-Discovery and NetBIOS, what Explorer's Network view shows:
//! Windows PCs, NAS running wsdd) and mDNS `_smb._tcp` (NAS, Samba, macOS).
//! No subnet or port scan. Every source fails silently: a missing one just
//! finds nothing.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// How long a search runs.
const SEARCH_TIME: Duration = Duration::from_secs(4);

/// A computer found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// The name for `\\name` (no `.local` / domain suffix), or an IPv4
    /// address when the source gave no name.
    pub name: String,
    /// The first address the source reported, if any.
    pub address: Option<IpAddr>,
    pub via: Via,
}

/// Which source found a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// WS-Discovery (Function Discovery).
    Wsd,
    /// NetBIOS (Function Discovery).
    NetBios,
    /// mDNS / DNS-SD `_smb._tcp`.
    Mdns,
}

/// The UNC host name for a name as a source reports it: `WORKGROUP\NAME`,
/// `NAME/Workgroup:WG` (WS-Discovery), `nas.local.` (mDNS) and
/// `pc.example.com` all give the first label. IPv4 addresses stay whole;
/// IPv6 literals and empty names give `None` (`\\fe80::1` is no UNC path).
fn unc_name(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let Some(i) = s.find('/') {
        s = &s[..i];
    }
    if let Some(i) = s.rfind('\\') {
        s = &s[i + 1..];
    }
    let s = s.trim().trim_end_matches('.');
    match s.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => return Some(s.to_string()),
        Ok(IpAddr::V6(_)) => return None,
        Err(_) => {}
    }
    if s.contains(':') {
        return None;
    }
    let label = s.split('.').next().unwrap_or("").trim();
    (!label.is_empty()).then(|| label.to_string())
}

/// The first usable address in a source's address list (`;` or `,`
/// separated, as a property store joins a vector): the first IPv4 one, else
/// the first IPv6 one (scope ids and brackets dropped).
fn first_address(list: &str) -> Option<IpAddr> {
    let parsed = list.split([';', ',']).filter_map(|a| {
        let a = a.trim().trim_start_matches('[');
        let a = a.split(['%', ']']).next().unwrap_or("");
        a.parse::<IpAddr>().ok()
    });
    let all: Vec<IpAddr> = parsed.collect();
    all.iter().find(|a| a.is_ipv4()).or(all.first()).copied()
}

/// Folds hosts as they arrive into one per name (case-insensitive, after
/// [`unc_name`]); the first one seen keeps its address and source.
#[derive(Default)]
struct Merger {
    names: HashSet<String>,
    addresses: HashSet<IpAddr>,
}

impl Merger {
    /// `host` with its name normalised if it is new, `None` if it is a
    /// duplicate (or has no usable name). A host named only by an address
    /// that an earlier host already had is a duplicate too.
    fn add(&mut self, host: Host) -> Option<Host> {
        let name = unc_name(&host.name)?;
        if !self.names.insert(name.to_lowercase()) {
            return None;
        }
        if let Ok(ip) = name.parse::<IpAddr>() {
            if self.addresses.contains(&ip) {
                return None;
            }
        }
        if let Some(a) = host.address {
            self.addresses.insert(a);
        }
        Some(Host { name, ..host })
    }
}

/// The hosts with one entry per name, in the order first seen (see [`Merger`]).
pub fn merge(hosts: impl IntoIterator<Item = Host>) -> Vec<Host> {
    let mut m = Merger::default();
    hosts.into_iter().filter_map(|h| m.add(h)).collect()
}

/// Searches the network for about 4 s, calling `found` (on this thread) with
/// each host once, as soon as it is found. Returns early when `cancel` is
/// set. Blocks: call it from a worker.
///
/// The sources run on their own threads, which are not waited for: ending a
/// Function Discovery query can take another ~8 s, so they finish cleaning up
/// in the background after this returns (whatever they find then is dropped).
pub fn discover(cancel: &AtomicBool, mut found: impl FnMut(Host)) {
    let deadline = Instant::now() + SEARCH_TIME;
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Host>();
    #[cfg(windows)]
    {
        for source in [win::function_discovery as fn(&AtomicBool, mpsc::Sender<Host>), win::mdns] {
            let (stop, tx) = (stop.clone(), tx.clone());
            let _ = std::thread::Builder::new().name("net-discover".into()).spawn(move || source(&stop, tx));
        }
    }
    drop(tx);
    let mut merger = Merger::default();
    while !cancel.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout((deadline - now).min(Duration::from_millis(50))) {
            Ok(h) => {
                if let Some(h) = merger.add(h) {
                    found(h);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
}

#[cfg(windows)]
mod win {
    use super::{first_address, Host, Via};
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::Mutex;
    use std::time::Duration;
    use windows::core::{implement, Interface, Ref, BSTR, HRESULT, PCWSTR, PWSTR};
    use windows::Win32::Devices::FunctionDiscovery::*;
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::NetworkManagement::Dns::*;
    use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToBSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

    /// How often the source threads look at the stop flag.
    const POLL: Duration = Duration::from_millis(50);

    /// `PKEY_ComputerName` (propkey.h; in windows-rs it sits in the large
    /// `Storage_EnhancedStorage` module, so it is spelled out here).
    const PKEY_COMPUTER_NAME: PROPERTYKEY = PROPERTYKEY {
        fmtid: windows::core::GUID::from_u128(0x28636aa6_953d_11d2_b5d6_00c04fd918d0),
        pid: 5,
    };

    /// `RESOURCEDISPLAYTYPE_SERVER` (WNet), as the property store shows it.
    const RESOURCEDISPLAYTYPE_SERVER: &str = "2";

    fn wait(stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(POLL);
        }
    }

    // ---- Function Discovery ------------------------------------------------

    /// A property as text (vectors come joined with `; `), `None` if missing.
    fn prop(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
        let mut v = unsafe { store.GetValue(key) }.ok()?;
        let s = unsafe { PropVariantToBSTR(&v) }.ok().map(|b: BSTR| b.to_string());
        let _ = unsafe { PropVariantClear(&mut v) };
        s.filter(|s| !s.trim().is_empty())
    }

    /// The host behind a function instance: its computer name, else (for a
    /// computer, not e.g. a WSD printer) its address or WNet server name.
    fn fd_host(inst: &IFunctionInstance, via: Via) -> Option<Host> {
        let store = unsafe { inst.OpenPropertyStore(STGM_READ) }.ok()?;
        let address = prop(&store, &PKEY_PNPX_IpAddress).and_then(|a| first_address(&a));
        if let Some(name) = prop(&store, &PKEY_COMPUTER_NAME) {
            return Some(Host { name, address, via });
        }
        if via == Via::NetBios {
            // The NetBIOS provider wraps WNet enumeration: its instances are
            // networks (seen here: "Microsoft Windows Network", ...), domains
            // and servers (`\\NAME`); keep the servers.
            let server = prop(&store, &PKEY_WNET_DisplayType).is_some_and(|t| t.trim() == RESOURCEDISPLAYTYPE_SERVER);
            return prop(&store, &PKEY_WNET_RemoteName).filter(|_| server).map(|name| Host { name, address, via });
        }
        // WS-Discovery also lists printers, scanners and media devices; only
        // computers (`pub:Computer`, which wsdd on a NAS also announces) can have shares.
        let types = prop(&store, &PKEY_PNPX_Types).unwrap_or_default().to_ascii_lowercase();
        if via == Via::Wsd && !types.contains("computer") {
            return None;
        }
        let a = address.filter(|a| a.is_ipv4())?;
        Some(Host { name: a.to_string(), address, via })
    }

    #[implement(IFunctionDiscoveryNotification)]
    struct Notify {
        tx: Mutex<Sender<Host>>,
        via: Via,
    }

    impl IFunctionDiscoveryNotification_Impl for Notify_Impl {
        fn OnUpdate(&self, action: QueryUpdateAction, _ctx: u64, inst: Ref<IFunctionInstance>) -> windows::core::Result<()> {
            if action == QUA_ADD || action == QUA_CHANGE {
                if let Some(h) = inst.as_ref().and_then(|i| fd_host(i, self.via)) {
                    let _ = self.tx.lock().map(|t| t.send(h));
                }
            }
            Ok(())
        }
        fn OnError(&self, _hr: HRESULT, _ctx: u64, _provider: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn OnEvent(&self, _id: u32, _ctx: u64, _provider: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// Starts an asynchronous instance query on `category`, sending the
    /// instances it already knows at once and the others as they answer
    /// (through [`Notify`]). The query runs while the returned object lives.
    fn fd_query(
        fd: &IFunctionDiscovery,
        category: PCWSTR,
        via: Via,
        tx: &Sender<Host>,
    ) -> Option<IFunctionInstanceCollectionQuery> {
        let notify: IFunctionDiscoveryNotification = Notify { tx: Mutex::new(tx.clone()), via }.into();
        let mut ctx = 0u64;
        let query = unsafe { fd.CreateInstanceCollectionQuery(category, PCWSTR::null(), true, &notify, &mut ctx) }.ok()?;
        // `Execute` says E_PENDING for a network query but may still hand
        // back the instances known so far; the generated wrapper would drop them.
        let mut raw = std::ptr::null_mut();
        let hr = unsafe { (Interface::vtable(&query).Execute)(query.as_raw(), &mut raw) };
        if !raw.is_null() {
            let coll = unsafe { IFunctionInstanceCollection::from_raw(raw) };
            let n = unsafe { coll.GetCount() }.unwrap_or(0);
            for i in 0..n {
                if let Some(h) = unsafe { coll.Item(i) }.ok().and_then(|inst| fd_host(&inst, via)) {
                    let _ = tx.send(h);
                }
            }
        }
        // E_PENDING = 0x8000000A.
        (hr.is_ok() || hr.0 == 0x8000_000Au32 as i32).then_some(query)
    }

    /// WS-Discovery and NetBIOS through Function Discovery, until `stop`.
    pub(super) fn function_discovery(stop: &AtomicBool, tx: Sender<Host>) {
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        {
            let fd: Option<IFunctionDiscovery> = unsafe { CoCreateInstance(&FunctionDiscovery, None, CLSCTX_ALL) }.ok();
            let queries: Vec<_> = fd
                .iter()
                .flat_map(|fd| {
                    [
                        fd_query(fd, FCTN_CATEGORY_WSDISCOVERY, Via::Wsd, &tx),
                        fd_query(fd, FCTN_CATEGORY_NETBIOS, Via::NetBios, &tx),
                    ]
                })
                .flatten()
                .collect();
            if !queries.is_empty() {
                wait(stop);
            }
            // Dropping the queries ends them (and their notifications).
        }
        if inited {
            unsafe { CoUninitialize() };
        }
    }

    // ---- mDNS --------------------------------------------------------------

    /// What the DnsService callbacks pass to the mDNS thread.
    enum Event {
        /// From a browse answer: service instance names (PTR), with the host
        /// name (SRV target) and addresses when the answer carried them.
        Browse(Vec<(String, Option<String>, Option<std::net::IpAddr>)>),
        /// A resolve finished (request `id`), with the host if it worked.
        Resolved(usize, Option<(String, Option<std::net::IpAddr>)>),
    }

    /// The running searches' channels, by context id. The callbacks get only
    /// the id, so one that comes after a search ended finds nothing and does
    /// nothing (no pointer into freed memory).
    static SINKS: Mutex<Option<HashMap<usize, Sender<Event>>>> = Mutex::new(None);
    static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

    fn sink_send(id: usize, e: Event) {
        if let Ok(g) = SINKS.lock() {
            if let Some(tx) = g.as_ref().and_then(|m| m.get(&id)) {
                let _ = tx.send(e);
            }
        }
    }

    fn wstr(p: PWSTR) -> Option<String> {
        (!p.is_null()).then(|| unsafe { p.to_string() }.ok()).flatten()
    }

    fn ipv4(raw: u32) -> std::net::IpAddr {
        // IP4_ADDRESS is in network byte order.
        std::net::IpAddr::V4(std::net::Ipv4Addr::from(raw.to_ne_bytes()))
    }

    /// The (instance, host, address) triples in a browse answer.
    fn browse_entries(list: *const DNS_RECORDW) -> Vec<(String, Option<String>, Option<std::net::IpAddr>)> {
        let mut ptrs = Vec::new();
        let mut srv = HashMap::new();
        let mut a = HashMap::new();
        let mut r = list;
        while let Some(rec) = unsafe { r.as_ref() } {
            let name = wstr(rec.pName).unwrap_or_default().to_lowercase();
            match DNS_TYPE(rec.wType) {
                DNS_TYPE_PTR => ptrs.extend(wstr(unsafe { rec.Data.PTR.pNameHost })),
                DNS_TYPE_SRV => {
                    if let Some(t) = wstr(unsafe { rec.Data.SRV.pNameTarget }) {
                        srv.insert(name, t);
                    }
                }
                DNS_TYPE_A => {
                    a.entry(name).or_insert(ipv4(unsafe { rec.Data.A.IpAddress }));
                }
                _ => {}
            }
            r = rec.pNext;
        }
        ptrs.into_iter()
            .map(|p| {
                let host = srv.get(&p.to_lowercase()).cloned();
                let addr = host.as_ref().and_then(|h| a.get(&h.to_lowercase()).copied());
                (p, host, addr)
            })
            .collect()
    }

    unsafe extern "system" fn on_browse(status: u32, ctx: *const core::ffi::c_void, records: *const DNS_RECORDW) {
        if status == 0 && !records.is_null() {
            sink_send(ctx as usize, Event::Browse(browse_entries(records)));
        }
        if !records.is_null() {
            unsafe { DnsFree(Some(records as *const _), DnsFreeRecordList) };
        }
    }

    /// The resolve callbacks' context: search id and request number, packed
    /// (a pointer-sized integer, never dereferenced).
    fn pack(id: usize, req: usize) -> usize {
        (id << 16) | (req & 0xFFFF)
    }

    unsafe extern "system" fn on_resolve(status: u32, ctx: *const core::ffi::c_void, inst: *const DNS_SERVICE_INSTANCE) {
        let (id, req) = (ctx as usize >> 16, ctx as usize & 0xFFFF);
        let host = match unsafe { inst.as_ref() } {
            Some(i) if status == 0 => wstr(i.pszHostName).map(|h| {
                let addr = (!i.ip4Address.is_null()).then(|| ipv4(unsafe { *i.ip4Address }));
                (h, addr)
            }),
            _ => None,
        };
        sink_send(id, Event::Resolved(req, host));
        if !inst.is_null() {
            unsafe { DnsServiceFreeInstance(inst) };
        }
    }

    /// DNS_REQUEST_PENDING: the asynchronous call started.
    const PENDING: i32 = 9506;

    /// mDNS `_smb._tcp.local` (DnsServiceBrowse, then DnsServiceResolve for
    /// instances whose answer had no SRV record), until `stop`.
    pub(super) fn mdns(stop: &AtomicBool, tx: Sender<Host>) {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) & (usize::MAX >> 16);
        let (etx, erx) = mpsc::channel();
        if let Ok(mut g) = SINKS.lock() {
            g.get_or_insert_with(HashMap::new).insert(id, etx);
        }
        let name: Vec<u16> = "_smb._tcp.local".encode_utf16().chain(Some(0)).collect();
        let req = DNS_SERVICE_BROWSE_REQUEST {
            Version: DNS_QUERY_REQUEST_VERSION1.0,
            InterfaceIndex: 0,
            QueryName: PCWSTR(name.as_ptr()),
            Anonymous: DNS_SERVICE_BROWSE_REQUEST_0 { pBrowseCallback: Some(on_browse) },
            pQueryContext: id as *mut _,
        };
        let mut browse = DNS_SERVICE_CANCEL::default();
        let browsing = unsafe { DnsServiceBrowse(&req, &mut browse) } == PENDING;

        // Resolves in flight: request number -> (cancel handle, query name
        // buffer, which must outlive the request).
        let mut resolving: HashMap<usize, (Box<DNS_SERVICE_CANCEL>, Vec<u16>)> = HashMap::new();
        let mut asked = HashSet::new();
        let mut next_req = 0usize;
        while browsing && !stop.load(Ordering::Relaxed) {
            let Ok(e) = erx.recv_timeout(POLL) else { continue };
            match e {
                Event::Browse(entries) => {
                    for (instance, host, addr) in entries {
                        if let Some(h) = host {
                            let _ = tx.send(Host { name: h, address: addr, via: Via::Mdns });
                            continue;
                        }
                        if !asked.insert(instance.to_lowercase()) || next_req > 0xFFFF {
                            continue;
                        }
                        let mut q: Vec<u16> = instance.encode_utf16().chain(Some(0)).collect();
                        let mut cancel = Box::<DNS_SERVICE_CANCEL>::default();
                        let r = DNS_SERVICE_RESOLVE_REQUEST {
                            Version: DNS_QUERY_REQUEST_VERSION1.0,
                            InterfaceIndex: 0,
                            QueryName: PWSTR(q.as_mut_ptr()),
                            pResolveCompletionCallback: Some(on_resolve),
                            pQueryContext: pack(id, next_req) as *mut _,
                        };
                        if unsafe { DnsServiceResolve(&r, &mut *cancel) } == PENDING {
                            resolving.insert(next_req, (cancel, q));
                        }
                        next_req += 1;
                    }
                }
                Event::Resolved(req, host) => {
                    resolving.remove(&req);
                    if let Some((h, addr)) = host {
                        let _ = tx.send(Host { name: h, address: addr, via: Via::Mdns });
                    }
                }
            }
        }
        // Unhook first, so late callbacks do nothing, then cancel.
        if let Ok(mut g) = SINKS.lock() {
            if let Some(m) = g.as_mut() {
                m.remove(&id);
            }
        }
        if browsing {
            unsafe { DnsServiceBrowseCancel(&browse) };
        }
        // A resolve that finished after the last event read is still in the
        // map; cancelling a finished request just fails.
        for (cancel, _name) in resolving.values() {
            unsafe { DnsServiceResolveCancel(&**cancel) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, address: Option<&str>, via: Via) -> Host {
        Host { name: name.into(), address: address.map(|a| a.parse().unwrap()), via }
    }

    #[test]
    fn unc_names() {
        assert_eq!(unc_name("NAS"), Some("NAS".into()));
        assert_eq!(unc_name("nas.local"), Some("nas".into()));
        assert_eq!(unc_name("nas.local."), Some("nas".into()));
        assert_eq!(unc_name("pc.corp.example.com"), Some("pc".into()));
        assert_eq!(unc_name("DESKTOP-1/Workgroup:WORKGROUP"), Some("DESKTOP-1".into()));
        assert_eq!(unc_name("PC2/Domain:corp.example.com"), Some("PC2".into()));
        assert_eq!(unc_name(r"WORKGROUP\PC3"), Some("PC3".into()));
        assert_eq!(unc_name(r"\\PC4"), Some("PC4".into()));
        assert_eq!(unc_name(" mac-mini.local "), Some("mac-mini".into()));
        assert_eq!(unc_name("192.168.1.20"), Some("192.168.1.20".into()));
        assert_eq!(unc_name("fe80::1"), None);
        assert_eq!(unc_name(""), None);
        assert_eq!(unc_name(".local"), None);
    }

    #[test]
    fn first_addresses() {
        assert_eq!(first_address("192.168.1.5"), Some("192.168.1.5".parse().unwrap()));
        assert_eq!(first_address("fe80::1%12; 192.168.1.5; 10.0.0.1"), Some("192.168.1.5".parse().unwrap()));
        assert_eq!(first_address("[fe80::2]"), Some("fe80::2".parse().unwrap()));
        assert_eq!(first_address("junk, 10.0.0.1"), Some("10.0.0.1".parse().unwrap()));
        assert_eq!(first_address(""), None);
    }

    #[test]
    fn merge_keeps_one_host_per_name() {
        let got = merge([
            host("NAS", Some("192.168.1.5"), Via::Wsd),
            host("nas.local.", Some("192.168.1.6"), Via::Mdns),
            host("desktop-1/Workgroup:WORKGROUP", None, Via::Wsd),
            host("DESKTOP-1", Some("192.168.1.7"), Via::NetBios),
            host("macbook.local", Some("192.168.1.8"), Via::Mdns),
            host("", None, Via::Wsd),
        ]);
        assert_eq!(
            got,
            vec![
                host("NAS", Some("192.168.1.5"), Via::Wsd),
                host("desktop-1", None, Via::Wsd),
                host("macbook", Some("192.168.1.8"), Via::Mdns),
            ]
        );
    }

    #[test]
    fn address_only_host_is_dropped_when_known() {
        let got = merge([
            host("nas.local", Some("192.168.1.5"), Via::Mdns),
            host("192.168.1.5", Some("192.168.1.5"), Via::Wsd),
            host("192.168.1.9", Some("192.168.1.9"), Via::Wsd),
            host("192.168.1.9", None, Via::NetBios),
        ]);
        assert_eq!(
            got,
            vec![host("nas", Some("192.168.1.5"), Via::Mdns), host("192.168.1.9", Some("192.168.1.9"), Via::Wsd)]
        );
    }

    /// Run by hand: `cargo test -p rc-platform -- --ignored discover --nocapture`.
    #[test]
    #[ignore]
    fn discover() {
        let start = Instant::now();
        let cancel = AtomicBool::new(false);
        let mut n = 0;
        super::discover(&cancel, |h| {
            n += 1;
            println!("{:>5} ms  {:<24} {:<40} {:?}", start.elapsed().as_millis(), h.name, format!("{:?}", h.address), h.via);
        });
        println!("{n} host(s) in {} ms", start.elapsed().as_millis());
        assert!(start.elapsed() < Duration::from_secs(6));
    }
}
