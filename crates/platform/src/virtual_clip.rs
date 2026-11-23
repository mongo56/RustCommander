//! "Virtual" files on the clipboard: files that don't exist on this machine
//! as paths, offered as a list of names ("FileGroupDescriptorW") and their
//! contents one by one ("FileContents"). Files copied in a remote desktop
//! session come this way, as do Outlook attachments, files in zips opened
//! in Explorer and files on phones.
//!
//! The data object is the OLE clipboard's, tied to the thread that opened
//! it: [`VirtualFiles`] is opened, read and dropped on one (worker) thread.

use std::io::{self, Read};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{HGLOBAL, RPC_E_CHANGED_MODE};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows::Win32::System::Com::{
    IDataObject, IStream, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{IsClipboardFormatAvailable, RegisterClipboardFormatW};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{OleGetClipboard, OleInitialize, OleUninitialize, ReleaseStgMedium};
use windows::Win32::UI::Shell::{
    CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, FD_ATTRIBUTES, FD_FILESIZE, FD_WRITESTIME, FILEDESCRIPTORW,
};

/// One item of [`VirtualFiles`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualItem {
    /// Relative path as the source gave it (`dir\sub\file.txt`); not
    /// checked: see `rc_core`'s use for that.
    pub name: String,
    pub is_dir: bool,
    /// `None` when the source doesn't say.
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
}

/// OLE initialised on this thread until dropped.
struct Ole;

impl Drop for Ole {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

/// The virtual files on the clipboard (see the module docs).
pub struct VirtualFiles {
    // Fields drop in order: the data object before OLE goes.
    data: IDataObject,
    contents: u16,
    items: Vec<VirtualItem>,
    _ole: Ole,
}

fn format(name: windows::core::PCWSTR) -> u16 {
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

/// Whether the clipboard holds virtual files. Doesn't open the clipboard.
pub(crate) fn available() -> bool {
    let f = format(CFSTR_FILEDESCRIPTORW);
    f != 0 && unsafe { IsClipboardFormatAvailable(u32::from(f)) }.is_ok()
}

/// A medium handed back by `GetData`, released when dropped.
struct Medium(STGMEDIUM);

impl Drop for Medium {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

fn get(data: &IDataObject, format: u16, index: i32, tymed: i32) -> io::Result<Medium> {
    let etc = FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed: tymed as u32,
    };
    unsafe { data.GetData(&etc) }.map(Medium).map_err(io::Error::other)
}

/// The bytes of a global memory block.
fn global_bytes(mem: HGLOBAL) -> io::Result<Vec<u8>> {
    unsafe {
        let size = GlobalSize(mem);
        let ptr = GlobalLock(mem);
        if ptr.is_null() {
            return Err(io::Error::other("Can't read the clipboard data"));
        }
        let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(mem);
        Ok(bytes)
    }
}

fn filetime(ft: windows::Win32::Foundation::FILETIME) -> Option<SystemTime> {
    // 100 ns ticks since 1601.
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    const TO_UNIX: u64 = 116_444_736_000_000_000;
    let since_unix = ticks.checked_sub(TO_UNIX)?;
    UNIX_EPOCH.checked_add(Duration::from_nanos(since_unix.checked_mul(100)?))
}

/// The items of a FILEGROUPDESCRIPTORW block (a count, then packed
/// descriptors).
fn parse_descriptors(bytes: &[u8]) -> Vec<VirtualItem> {
    let Some(count) = bytes.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize) else {
        return Vec::new();
    };
    let size = size_of::<FILEDESCRIPTORW>();
    let mut items = Vec::new();
    for i in 0..count {
        let Some(chunk) = bytes.get(4 + i * size..4 + (i + 1) * size) else { break };
        // SAFETY: FILEDESCRIPTORW is packed plain data; the chunk holds one.
        let d = unsafe { chunk.as_ptr().cast::<FILEDESCRIPTORW>().read_unaligned() };
        let flags = d.dwFlags as i32;
        let name = d.cFileName;
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let attrs = d.dwFileAttributes;
        let is_dir = flags & FD_ATTRIBUTES.0 != 0 && attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
        let size = (flags & FD_FILESIZE.0 != 0).then(|| (u64::from(d.nFileSizeHigh) << 32) | u64::from(d.nFileSizeLow));
        let modified = if flags & FD_WRITESTIME.0 != 0 { filetime(d.ftLastWriteTime) } else { None };
        items.push(VirtualItem { name: String::from_utf16_lossy(&name[..len]), is_dir, size, modified });
    }
    items
}

impl VirtualFiles {
    /// The virtual files on the clipboard now, or `None` if it holds none.
    /// Initialises OLE on the calling thread (a fresh worker thread: one
    /// already in the multithreaded apartment fails).
    pub fn from_clipboard() -> io::Result<Option<VirtualFiles>> {
        match unsafe { OleInitialize(None) } {
            Ok(()) => {}
            Err(e) if e.code() == RPC_E_CHANGED_MODE => {
                return Err(io::Error::other("Can't read the clipboard on this thread"));
            }
            Err(e) => return Err(io::Error::other(e)),
        }
        let ole = Ole;
        let (descriptors, contents) = (format(CFSTR_FILEDESCRIPTORW), format(CFSTR_FILECONTENTS));
        if descriptors == 0 || contents == 0 {
            return Ok(None);
        }
        let data = unsafe { OleGetClipboard() }.map_err(io::Error::other)?;
        let Ok(medium) = get(&data, descriptors, -1, TYMED_HGLOBAL.0) else { return Ok(None) };
        if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
            return Ok(None);
        }
        let bytes = global_bytes(unsafe { medium.0.u.hGlobal })?;
        drop(medium);
        let items = parse_descriptors(&bytes);
        if items.is_empty() {
            return Ok(None);
        }
        Ok(Some(VirtualFiles { data, contents, items, _ole: ole }))
    }

    pub fn items(&self) -> &[VirtualItem] {
        &self.items
    }

    /// The contents of file `index` (of [`VirtualFiles::items`]). From a
    /// remote desktop they come over the connection as they are read.
    pub fn open(&self, index: usize) -> io::Result<Box<dyn Read + '_>> {
        let medium = get(&self.data, self.contents, index as i32, TYMED_ISTREAM.0 | TYMED_HGLOBAL.0)?;
        if medium.0.tymed == TYMED_ISTREAM.0 as u32 {
            // AddRef'd copy: the medium releases its own.
            let stream = unsafe { (*medium.0.u.pstm).clone() }
                .ok_or_else(|| io::Error::other("The clipboard gave no data"))?;
            // Some sources hand the stream over positioned at its end.
            let _ = unsafe { stream.Seek(0, STREAM_SEEK_SET, None) };
            return Ok(Box::new(StreamReader(stream)));
        }
        if medium.0.tymed == TYMED_HGLOBAL.0 as u32 {
            let mut bytes = global_bytes(unsafe { medium.0.u.hGlobal })?;
            // A block may be larger than asked for: the size says how much is the file.
            if let Some(size) = self.items[index].size {
                bytes.truncate(usize::try_from(size).unwrap_or(usize::MAX));
            }
            return Ok(Box::new(io::Cursor::new(bytes)));
        }
        Err(io::Error::other("This kind of clipboard data can't be pasted as a file"))
    }
}

struct StreamReader(IStream);

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut got = 0u32;
        let want = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let hr = unsafe { self.0.Read(buf.as_mut_ptr().cast(), want, Some(&mut got)) };
        // S_FALSE (end reached) is a success too.
        hr.ok().map_err(io::Error::other)?;
        Ok(got as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_descriptors() {
        let mut bytes = 2u32.to_le_bytes().to_vec();
        for (name, flags, attrs, size) in [("dir", FD_ATTRIBUTES.0, FILE_ATTRIBUTE_DIRECTORY.0, 0u64), ("dir\\a.txt", FD_FILESIZE.0, 0, 5)] {
            let mut d: FILEDESCRIPTORW = unsafe { std::mem::zeroed() };
            d.dwFlags = flags as u32;
            d.dwFileAttributes = attrs;
            d.nFileSizeLow = size as u32;
            let mut file_name = [0u16; 260];
            for (slot, unit) in file_name.iter_mut().zip(name.encode_utf16()) {
                *slot = unit;
            }
            d.cFileName = file_name;
            bytes.extend_from_slice(unsafe {
                std::slice::from_raw_parts((&d as *const FILEDESCRIPTORW).cast::<u8>(), size_of::<FILEDESCRIPTORW>())
            });
        }
        let items = parse_descriptors(&bytes);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].name.as_str(), items[0].is_dir, items[0].size), ("dir", true, None));
        assert_eq!((items[1].name.as_str(), items[1].is_dir, items[1].size), ("dir\\a.txt", false, Some(5)));
        // Truncated block: only whole descriptors.
        assert_eq!(parse_descriptors(&bytes[..bytes.len() - 1]).len(), 1);
        assert!(parse_descriptors(&[]).is_empty());
    }
}
