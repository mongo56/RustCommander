//! What a console process is doing, for the built-in terminal's titles: the
//! current folder of a shell (read from its PEB) and the program it runs
//! (its newest descendant). Windows only (64-bit); elsewhere both are `None`.

use std::path::PathBuf;

/// Processes that belong to the console itself, not to what the user runs.
const CONSOLE_HOSTS: [&str; 2] = ["conhost.exe", "openconsole.exe"];

/// Current folder of process `pid`, as the process itself sees it
/// (`SetCurrentDirectory`). `None` when it can't be read (gone, no access,
/// a 32-bit process).
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(all(windows, target_pointer_width = "64"))]
    {
        imp::cwd(pid)
    }
    #[cfg(not(all(windows, target_pointer_width = "64")))]
    {
        let _ = pid;
        None
    }
}

/// Exe name (without `.exe`) of the program process `pid` runs: following
/// the newest child each time down to the last one, skipping console hosts.
/// `None` when it runs nothing.
pub fn foreground_child(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        let mut procs = imp::processes()?;
        // Creation times only for `pid` and what descends from it (by
        // parent id; the age check comes after).
        let mut tree = vec![pid];
        let mut i = 0;
        while i < tree.len() && tree.len() < 256 {
            let at = tree[i];
            let kids: Vec<u32> =
                procs.iter().filter(|p| p.parent == at && p.pid != at && !tree.contains(&p.pid)).map(|p| p.pid).collect();
            tree.extend(kids);
            i += 1;
        }
        for p in procs.iter_mut().filter(|p| tree.contains(&p.pid)) {
            p.created = imp::created(p.pid);
        }
        newest_descendant(&procs, pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// A process of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Proc {
    pid: u32,
    parent: u32,
    /// Exe file name, e.g. `ping.exe`.
    exe: String,
    /// Creation time (FILETIME ticks), 0 if unknown.
    created: u64,
}

/// The program `pid` runs, from a process list (see `foreground_child`). A
/// child must not be older than its parent: parent ids aren't cleared when
/// a parent exits, so an old process can name a reused id as its parent.
fn newest_descendant(procs: &[Proc], pid: u32) -> Option<String> {
    let created = |pid: u32| procs.iter().find(|p| p.pid == pid).map_or(0, |p| p.created);
    let mut at = pid;
    let mut found = None;
    // (Bounded, in case of a parent-id cycle.)
    for _ in 0..32 {
        let since = created(at);
        let child = procs
            .iter()
            .filter(|p| p.parent == at && p.pid != at && p.created >= since)
            .filter(|p| !CONSOLE_HOSTS.contains(&p.exe.to_ascii_lowercase().as_str()))
            .max_by_key(|p| p.created);
        let Some(child) = child else { break };
        at = child.pid;
        found = Some(child);
    }
    found.map(|p| exe_stem(&p.exe))
}

fn exe_stem(exe: &str) -> String {
    match exe.len().checked_sub(4) {
        Some(n) if exe.is_char_boundary(n) && exe[n..].eq_ignore_ascii_case(".exe") => exe[..n].to_owned(),
        _ => exe.to_owned(),
    }
}

#[cfg(windows)]
mod imp {
    use super::Proc;
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    /// Closes a handle when dropped.
    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: a handle we opened and own.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Every process with its parent (creation times left 0).
    pub(super) fn processes() -> Option<Vec<Proc>> {
        // SAFETY: plain Win32 calls; the entry is sized as the API asks and
        // the snapshot handle is closed by `Owned`.
        unsafe {
            let snap = Owned(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?);
            let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut procs = Vec::new();
            let mut more = Process32FirstW(snap.0, &mut entry).is_ok();
            while more {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                procs.push(Proc {
                    pid: entry.th32ProcessID,
                    parent: entry.th32ParentProcessID,
                    exe: String::from_utf16_lossy(&entry.szExeFile[..len]),
                    created: 0,
                });
                more = Process32NextW(snap.0, &mut entry).is_ok();
            }
            Some(procs)
        }
    }

    /// Creation time of process `pid`, 0 if unknown.
    pub(super) fn created(pid: u32) -> u64 {
        // SAFETY: plain Win32 calls with valid out pointers; the handle is
        // closed by `Owned`.
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return 0 };
            let h = Owned(h);
            let (mut c, mut e, mut k, mut u) =
                (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            if GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u).is_err() {
                return 0;
            }
            (u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime)
        }
    }

    /// The current folder from the process's PEB:
    /// `PEB.ProcessParameters->CurrentDirectory.DosPath`.
    #[cfg(target_pointer_width = "64")]
    pub(super) fn cwd(pid: u32) -> Option<std::path::PathBuf> {
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStringExt;
        use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
        use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        use windows::Win32::System::Threading::{
            IsWow64Process, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
        };
        /// `PEB::ProcessParameters` (x64).
        const PEB_PARAMETERS: usize = 0x20;
        /// `RTL_USER_PROCESS_PARAMETERS::CurrentDirectory` (a `CURDIR`, whose
        /// `DosPath` comes first; x64).
        const PARAMETERS_CURDIR: usize = 0x38;

        /// `size_of::<T>()` bytes at `addr` of process `h`.
        unsafe fn read<T: Default>(h: HANDLE, addr: usize) -> Option<T> {
            let mut value = T::default();
            let mut got = 0usize;
            // SAFETY: `value` is a writable `T`; at most its size is written.
            unsafe {
                ReadProcessMemory(h, addr as *const c_void, &mut value as *mut T as *mut c_void, size_of::<T>(), Some(&mut got))
                    .ok()?;
            }
            (got == size_of::<T>()).then_some(value)
        }

        // SAFETY: plain Win32/NT calls with valid out pointers; remote
        // addresses are only read through `ReadProcessMemory`, which checks
        // them; the handle is closed by `Owned`.
        unsafe {
            let h = Owned(OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).ok()?);
            // A 32-bit process has a 32-bit PEB with other offsets.
            let mut wow = windows::core::BOOL::default();
            if IsWow64Process(h.0, &mut wow).is_err() || wow.as_bool() {
                return None;
            }
            // PROCESS_BASIC_INFORMATION (x64): six pointer-sized fields, the
            // PEB address second.
            let mut info = [0usize; 6];
            let status = NtQueryInformationProcess(
                h.0,
                ProcessBasicInformation,
                info.as_mut_ptr() as *mut c_void,
                size_of_val(&info) as u32,
                std::ptr::null_mut(),
            );
            let peb = info[1];
            if status.is_err() || peb == 0 {
                return None;
            }
            let params: usize = read(h.0, peb + PEB_PARAMETERS)?;
            if params == 0 {
                return None;
            }
            // UNICODE_STRING: Length (bytes), MaximumLength, padding, Buffer.
            let (len, _max, _pad): (u16, u16, u32) = read(h.0, params + PARAMETERS_CURDIR)?;
            let buffer: usize = read(h.0, params + PARAMETERS_CURDIR + 8)?;
            let chars = usize::from(len) / 2;
            if buffer == 0 || chars == 0 {
                return None;
            }
            let mut wide = vec![0u16; chars];
            let mut got = 0usize;
            ReadProcessMemory(h.0, buffer as *const c_void, wide.as_mut_ptr() as *mut c_void, chars * 2, Some(&mut got))
                .ok()?;
            if got != chars * 2 {
                return None;
            }
            let mut path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&wide));
            // Stored with a trailing backslash (`C:\Users\`); keep it only on
            // a drive root.
            let s = path.as_os_str().to_string_lossy();
            if s.len() > 3 && s.ends_with('\\') {
                path = std::path::PathBuf::from(&s[..s.len() - 1]);
            }
            Some(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32, exe: &str, created: u64) -> Proc {
        Proc { pid, parent, exe: exe.to_owned(), created }
    }

    #[test]
    fn program_is_the_newest_descendant() {
        let procs = [
            p(10, 1, "pwsh.exe", 100),
            p(11, 10, "conhost.exe", 101),
            p(12, 10, "git.exe", 110),
            p(13, 10, "ping.exe", 120),
            p(14, 13, "OpenConsole.exe", 121),
            // A stale parent id: older than the shell, so not its child.
            p(15, 10, "old.exe", 50),
        ];
        assert_eq!(newest_descendant(&procs, 10).as_deref(), Some("ping"));
        assert_eq!(newest_descendant(&procs, 13), None);
        let deeper = [p(10, 1, "cmd.exe", 100), p(12, 10, "cmd.exe", 110), p(13, 12, "node.exe", 111)];
        assert_eq!(newest_descendant(&deeper, 10).as_deref(), Some("node"));
        assert_eq!(newest_descendant(&deeper, 99), None);
    }

    #[test]
    fn exe_names() {
        assert_eq!(exe_stem("ping.exe"), "ping");
        assert_eq!(exe_stem("VIM.EXE"), "VIM");
        assert_eq!(exe_stem("exe"), "exe");
        assert_eq!(exe_stem("a.com"), "a.com");
    }

    /// A real `cmd.exe` started in a temp folder: its folder is read, and so
    /// is the `ping` it runs.
    #[cfg(windows)]
    #[test]
    fn real_shell_folder_and_program() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let dir = std::env::temp_dir().join(format!("rc-procinfo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        // canonicalize gives a `\\?\` path; cmd shows the plain one.
        let plain = PathBuf::from(dir.display().to_string().trim_start_matches(r"\\?\"));
        let mut shell = Command::new("cmd.exe")
            .args(["/d", "/c", "ping -n 6 127.0.0.1 >nul"])
            .current_dir(&plain)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = shell.id();
        let end = Instant::now() + Duration::from_secs(5);
        let mut program = None;
        while program.is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
            program = foreground_child(pid);
        }
        let cwd = process_cwd(pid);
        // Stop the ping first, then the shell.
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).output();
        let _ = shell.kill();
        let _ = shell.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(program.as_deref().map(str::to_ascii_lowercase).as_deref(), Some("ping"));
        let cwd = cwd.expect("cwd");
        assert!(cwd.as_os_str().eq_ignore_ascii_case(plain.as_os_str()), "{cwd:?} vs {plain:?}");
    }
}
