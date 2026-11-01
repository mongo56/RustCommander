//! RAR archives (rar4 and rar5), read-only, through RARLAB's UnRAR library
//! (`unrar_sys` builds its C++ source; freeware licence, see
//! THIRD-PARTY-NOTICES.txt). Multi-volume archives (`x.part1.rar`, ...) are
//! read across their volumes when those lie next to the first one.
//!
//! The index is read in UnRAR's list mode; files are read in its extract
//! mode on a worker thread that hands the unpacked data over a bounded
//! channel, so big files stream instead of being unpacked whole. Entries
//! are matched by their position in the archive (both modes walk the same
//! headers), checked against the name.
//!
//! Passwords (also for encrypted names, `-hp`) are given through UnRAR's
//! callback as UTF-16, so non-ASCII passwords work.

use super::{clean_parts, io_err, password_error, password_for, Archive, Raw};
use chrono::{DateTime, Local, TimeZone};
use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use unrar_sys as sys;

// `unrar_sys` declares these structs `repr(C)`, but UnRAR's dll.hpp packs
// them (`#pragma pack(1)`), so every field after the first pointer is off
// by 4 bytes there (times, and the comment buffer UnRAR may write to).
// Declared again here as UnRAR has them.

#[repr(C, packed)]
struct OpenData {
    arc_name: *const std::os::raw::c_char,
    arc_name_w: *const sys::WCHAR,
    open_mode: u32,
    open_result: u32,
    cmt_buf: *mut std::os::raw::c_char,
    cmt_buf_size: u32,
    cmt_size: u32,
    cmt_state: u32,
    flags: u32,
    callback: Option<sys::Callback>,
    user_data: sys::LPARAM,
    op_flags: u32,
    cmt_buf_w: *mut sys::WCHAR,
    reserved: [u32; 25],
}

#[repr(C, packed)]
struct Header {
    arc_name: [std::os::raw::c_char; 1024],
    arc_name_w: [sys::WCHAR; 1024],
    file_name: [std::os::raw::c_char; 1024],
    file_name_w: [sys::WCHAR; 1024],
    flags: u32,
    pack_size: u32,
    pack_size_high: u32,
    unp_size: u32,
    unp_size_high: u32,
    host_os: u32,
    file_crc: u32,
    file_time: u32,
    unp_ver: u32,
    method: u32,
    file_attr: u32,
    cmt_buf: *mut std::os::raw::c_char,
    cmt_buf_size: u32,
    cmt_size: u32,
    cmt_state: u32,
    dict_size: u32,
    hash_type: u32,
    hash: [std::os::raw::c_char; 32],
    redir_type: u32,
    redir_name: *mut sys::WCHAR,
    redir_name_size: u32,
    dir_target: u32,
    mtime_low: u32,
    mtime_high: u32,
    ctime_low: u32,
    ctime_high: u32,
    atime_low: u32,
    atime_high: u32,
    arc_name_ex: *mut sys::WCHAR,
    arc_name_ex_size: u32,
    file_name_ex: *mut sys::WCHAR,
    file_name_ex_size: u32,
    reserved: [u32; 982],
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Header>() == 6144 + 11 * 4 + 8 + 5 * 4 + 32 + 4 + 8 + 8 * 4 + 8 + 4 + 8 + 4 + 982 * 4);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<OpenData>() == 3 * 8 + 4 * 6 + 8 + 8 + 4 + 8 + 100);

extern "C" {
    // The same functions `unrar_sys` links, with the packed structs.
    fn RAROpenArchiveEx(data: *mut OpenData) -> *const sys::Handle;
    fn RARReadHeaderEx(handle: *const sys::Handle, header: *mut Header) -> std::os::raw::c_int;
}

impl Header {
    /// A zeroed header on the heap (it's about 10 KB).
    fn new() -> Box<Header> {
        // SAFETY: all-zero is valid for every field (null pointers).
        unsafe { Box::new(std::mem::zeroed()) }
    }

    fn name(&self) -> String {
        let name = self.file_name_w;
        from_wide(&name)
    }
}

/// What the UnRAR callback works with (owned by [`Rar`], which outlives
/// the handle using it).
struct State {
    /// NUL-terminated wide password, if one was given.
    password: Option<Vec<sys::WCHAR>>,
    /// Where unpacked data goes while a file is processed.
    sink: Option<SyncSender<Msg>>,
    /// Set when a next volume was asked for but isn't there.
    missing_volume: bool,
}

/// An open archive. Fields drop in order: the handle closes before its
/// callback state goes.
struct Rar {
    handle: *const sys::Handle,
    state: Box<State>,
}

impl Drop for Rar {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful RAROpenArchiveEx and is
        // closed once.
        unsafe { sys::RARCloseArchive(self.handle) };
    }
}

enum Msg {
    /// A wanted file starts (its position in the archive).
    Start(usize),
    Data(Vec<u8>),
    /// The current file is done.
    End(io::Result<()>),
    /// The worker is done (or failed between files).
    Done(io::Result<()>),
}

#[cfg(windows)]
fn wide(text: &std::ffi::OsStr) -> Vec<sys::WCHAR> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain(Some(0)).collect()
}

#[cfg(not(windows))]
fn wide(text: &std::ffi::OsStr) -> Vec<sys::WCHAR> {
    text.to_string_lossy().chars().map(|c| c as sys::WCHAR).chain(Some(0)).collect()
}

/// A NUL-terminated wide string.
fn from_wide(text: &[sys::WCHAR]) -> String {
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    #[cfg(windows)]
    return String::from_utf16_lossy(&text[..end]);
    #[cfg(not(windows))]
    return text[..end].iter().map(|&c| char::from_u32(c as u32).unwrap_or('\u{fffd}')).collect();
}

extern "C" fn callback(msg: sys::UINT, user: sys::LPARAM, p1: sys::LPARAM, p2: sys::LPARAM) -> std::os::raw::c_int {
    if user == 0 {
        return -1;
    }
    // SAFETY: `user` is the `State` boxed in the `Rar` whose handle calls
    // this, alive for as long as the handle.
    let state = unsafe { &mut *(user as *mut State) };
    match msg {
        sys::UCM_CHANGEVOLUMEW | sys::UCM_CHANGEVOLUME => {
            if p2 == sys::RAR_VOL_ASK {
                state.missing_volume = true;
                -1
            } else {
                0
            }
        }
        sys::UCM_PROCESSDATA => {
            let Some(sink) = &state.sink else { return 0 };
            // SAFETY: UnRAR passes a buffer and its length.
            let data = unsafe { std::slice::from_raw_parts(p1 as *const u8, p2 as usize) };
            match sink.send(Msg::Data(data.to_vec())) {
                Ok(()) => 1,
                // The reader is gone: stop unpacking.
                Err(_) => -1,
            }
        }
        sys::UCM_NEEDPASSWORDW => {
            let Some(password) = &state.password else { return -1 };
            let size = p2 as usize;
            if size == 0 || password.len() > size {
                return -1;
            }
            // SAFETY: UnRAR passes a buffer of `size` wide chars; the
            // password (with its NUL) fits.
            unsafe { std::ptr::copy_nonoverlapping(password.as_ptr(), p1 as *mut sys::WCHAR, password.len()) };
            1
        }
        // The narrow password is only asked for when the wide one is
        // empty: there's none.
        _ => -1,
    }
}

/// Why UnRAR failed with `code`, as an error about the archive `file`.
fn error(file: &Path, code: i32, missing_volume: bool) -> io::Error {
    let text = match code {
        sys::ERAR_MISSING_PASSWORD | sys::ERAR_BAD_PASSWORD => return password_error(file),
        sys::ERAR_EOPEN if missing_volume => "The next volume of this multi-volume archive is missing",
        sys::ERAR_EOPEN => "Can't open the archive",
        sys::ERAR_NO_MEMORY => "Not enough memory",
        sys::ERAR_BAD_DATA => "The archive is damaged (CRC error)",
        sys::ERAR_BAD_ARCHIVE => "Not a RAR archive",
        sys::ERAR_UNKNOWN_FORMAT => "Unknown archive format or encryption",
        sys::ERAR_EREAD => "Read error",
        sys::ERAR_EREFERENCE => "A file reference in the archive can't be resolved",
        _ => "Unknown error",
    };
    io::Error::other(format!("{text} (RAR error {code}): {}", file.display()))
}

impl Rar {
    /// Opens `disk` in list or extract mode, with the password given for
    /// the logical archive `file`.
    fn open(file: &Path, disk: &Path, mode: u32) -> io::Result<Rar> {
        let state = Box::new(State {
            password: password_for(file).map(|p| wide(std::ffi::OsStr::new(&p))),
            sink: None,
            missing_volume: false,
        });
        let name = wide(disk.as_os_str());
        let mut data = OpenData {
            arc_name: std::ptr::null(),
            arc_name_w: name.as_ptr(),
            open_mode: mode,
            open_result: 0,
            cmt_buf: std::ptr::null_mut(),
            cmt_buf_size: 0,
            cmt_size: 0,
            cmt_state: 0,
            flags: 0,
            callback: Some(callback),
            user_data: &*state as *const State as sys::LPARAM,
            op_flags: 0,
            cmt_buf_w: std::ptr::null_mut(),
            reserved: [0; 25],
        };
        // SAFETY: `data` and the name it points to outlive the call.
        let handle = unsafe { RAROpenArchiveEx(&mut data) };
        if handle.is_null() {
            return Err(error(file, data.open_result as i32, false));
        }
        let rar = Rar { handle, state };
        match data.open_result as i32 {
            sys::ERAR_SUCCESS => Ok(rar),
            code => Err(error(file, code, false)),
        }
    }

    /// The next header, `None` at the end.
    fn next(&mut self, file: &Path) -> io::Result<Option<Box<Header>>> {
        let mut header = Header::new();
        // SAFETY: valid handle and header struct.
        match unsafe { RARReadHeaderEx(self.handle, &mut *header) } {
            sys::ERAR_SUCCESS => Ok(Some(header)),
            sys::ERAR_END_ARCHIVE => Ok(None),
            code => Err(error(file, code, self.state.missing_volume)),
        }
    }

    /// Skips or tests (unpacks into the callback) the current file.
    fn process(&mut self, file: &Path, operation: i32) -> io::Result<()> {
        // SAFETY: valid handle; no destination for skip/test.
        match unsafe { sys::RARProcessFileW(self.handle, operation, std::ptr::null(), std::ptr::null()) } {
            sys::ERAR_SUCCESS => Ok(()),
            code => Err(error(file, code, self.state.missing_volume)),
        }
    }
}

/// A header's 64-bit unpacked size.
fn size_of(h: &Header) -> u64 {
    (u64::from(h.unp_size_high) << 32) | u64::from(h.unp_size)
}

/// A header's modification time: its FILETIME, else the DOS one.
fn modified_of(h: &Header) -> Option<DateTime<Local>> {
    let ticks = (u64::from(h.mtime_high) << 32) | u64::from(h.mtime_low);
    if ticks != 0 {
        // 100 ns ticks since 1601-01-01.
        let secs = (ticks / 10_000_000) as i64 - 11_644_473_600;
        return Local.timestamp_opt(secs, (ticks % 10_000_000) as u32 * 100).single();
    }
    let (date, time) = (h.file_time >> 16, h.file_time & 0xffff);
    Local
        .with_ymd_and_hms(
            (1980 + (date >> 9)) as i32,
            (date >> 5) & 0xf,
            date & 0x1f,
            time >> 11,
            (time >> 5) & 0x3f,
            (time & 0x1f) * 2,
        )
        .earliest()
}

/// The archive's entries. A missing next volume ends the list early (what
/// the volumes at hand hold is shown; reading a file running into the
/// missing one fails).
pub(super) fn read_index(file: &Path, disk: &Path) -> io::Result<Vec<Raw>> {
    let mut rar = Rar::open(file, disk, sys::RAR_OM_LIST)?;
    let mut raw = Vec::new();
    loop {
        let header = match rar.next(file) {
            Ok(Some(h)) => h,
            Ok(None) => break,
            Err(_) if rar.state.missing_volume && !raw.is_empty() => break,
            Err(e) => return Err(e),
        };
        let attr = header.file_attr;
        // DOS, OS/2 and Windows attributes: hidden or system.
        let dos = header.host_os <= 2;
        raw.push(Raw {
            name: header.name(),
            is_dir: header.flags & sys::RHDF_DIRECTORY != 0,
            size: size_of(&header),
            modified: modified_of(&header),
            hidden: dos && attr & 0x6 != 0,
            source: raw.len(),
            encrypted: header.flags & sys::RHDF_ENCRYPTED != 0,
        });
        match rar.process(file, sys::RAR_SKIP) {
            Ok(()) => {}
            Err(_) if rar.state.missing_volume => break,
            Err(e) => return Err(e),
        }
    }
    Ok(raw)
}

/// The cleaned, lowercased form of an entry name (as `Archive` keys it).
fn key_of(name: &str) -> Option<String> {
    clean_parts(name).map(|parts| parts.join("/").to_lowercase())
}

/// Hands the unpacked data of one file over from the worker.
struct Stream<'a> {
    rx: &'a Receiver<Msg>,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl Read for Stream<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.buf.len() {
            if self.done {
                return Ok(0);
            }
            match self.rx.recv() {
                Ok(Msg::Data(data)) => (self.buf, self.pos) = (data, 0),
                Ok(Msg::End(result)) => {
                    self.done = true;
                    result?;
                }
                _ => {
                    self.done = true;
                    return Err(io::Error::other("RAR reader stopped unexpectedly"));
                }
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// The worker: walks the archive, unpacking the wanted files (position →
/// expected key) into `tx`.
fn unpack(file: &Path, disk: &Path, mut want: HashMap<usize, String>, tx: &SyncSender<Msg>) -> io::Result<()> {
    let mut rar = Rar::open(file, disk, sys::RAR_OM_EXTRACT)?;
    let mut n = 0;
    while !want.is_empty() {
        let Some(header) = rar.next(file)? else { break };
        let here = n;
        n += 1;
        let Some(key) = want.remove(&here) else {
            rar.process(file, sys::RAR_SKIP)?;
            continue;
        };
        if key_of(&header.name()).as_deref() != Some(key.as_str()) {
            return Err(io::Error::other(format!("The archive changed while reading it: {}", file.display())));
        }
        if tx.send(Msg::Start(here)).is_err() {
            return Ok(());
        }
        rar.state.sink = Some(tx.clone());
        let mut result = rar.process(file, sys::RAR_TEST);
        rar.state.sink = None;
        // A wrong rar4 password shows as a CRC error.
        let garbled = result.as_ref().is_err_and(|e| e.to_string().contains("CRC error"));
        if garbled && header.flags & sys::RHDF_ENCRYPTED != 0 {
            result = Err(password_error(file));
        }
        let failed = result.is_err();
        if tx.send(Msg::End(result)).is_err() || failed {
            return Ok(());
        }
    }
    Ok(())
}

impl Archive {
    /// RAR: unpacks the wanted files on a worker thread, in archive order.
    pub(super) fn read_rar(&self, files: &[PathBuf], each: &mut dyn FnMut(usize, &mut dyn Read) -> io::Result<bool>)
        -> io::Result<()> {
        let items: HashMap<usize, usize> =
            self.items.iter().enumerate().filter_map(|(k, item)| item.source.map(|src| (src, k))).collect();
        let mut want = HashMap::new();
        let mut index = HashMap::new();
        for (i, src) in self.sources(files)? {
            let Some(&item) = src.and_then(|src| items.get(&src)) else { continue };
            let src = self.items[item].source.unwrap_or_default();
            want.insert(src, self.items[item].inner.to_lowercase());
            index.insert(src, i);
        }
        if want.is_empty() {
            return Ok(());
        }
        let (tx, rx) = sync_channel(8);
        let (file, disk) = (self.file.clone(), self.disk.clone());
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let result = unpack(&file, &disk, want, &tx);
                let _ = tx.send(Msg::Done(result));
            });
            let result = (|| loop {
                match rx.recv() {
                    Ok(Msg::Start(src)) => {
                        let Some(&i) = index.get(&src) else { continue };
                        let mut stream = Stream { rx: &rx, buf: Vec::new(), pos: 0, done: false };
                        if !each(i, &mut stream)? {
                            return Ok(());
                        }
                        // Whatever the reader left (and its error).
                        io::copy(&mut stream, &mut io::sink())?;
                    }
                    Ok(Msg::Done(result)) => return result,
                    Ok(Msg::Data(_) | Msg::End(_)) => {}
                    Err(_) => return Err(io_err("RAR reader stopped unexpectedly")),
                }
            })();
            // Lets a worker blocked on sending see the reader gone.
            drop(rx);
            result
        })
    }
}
