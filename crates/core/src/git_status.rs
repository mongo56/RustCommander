//! Git status of a work tree, for the panels' status letters and branch name.
//!
//! `git status --porcelain=v1 -z --branch` is run by [`query`] (on a worker
//! thread; it is killed after a time limit) and read by [`parse`].

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Parsed `git status` output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatus {
    /// Current branch; `HEAD (detached)` when on no branch.
    pub branch: Option<String>,
    /// Changed paths relative to the work tree root (`/` separators; an
    /// untracked folder ends in `/`) and their status letter.
    pub files: Vec<(String, char)>,
}

/// Why [`query`] gave nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// `git` isn't installed / not on PATH.
    NoGit,
    /// It failed or took too long.
    Failed,
}

/// Strength of a status letter when a folder sums up its children:
/// U > M > A > R > D > ?.
pub fn rank(letter: char) -> u8 {
    match letter {
        'U' => 6,
        'M' => 5,
        'A' => 4,
        'R' => 3,
        'D' => 2,
        '?' => 1,
        _ => 0,
    }
}

/// The letter for one side of the two-letter code (index or work tree).
fn letter_of(c: char) -> char {
    match c {
        'M' | 'T' => 'M',
        'A' => 'A',
        'R' | 'C' => 'R',
        'D' => 'D',
        'U' => 'U',
        '?' => '?',
        _ => ' ',
    }
}

/// One letter for an `XY` code.
fn letter_for(x: char, y: char) -> char {
    // Both-deleted / both-added and any `U` are conflicts.
    if x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D') {
        return 'U';
    }
    let (a, b) = (letter_of(x), letter_of(y));
    if rank(a) >= rank(b) { a } else { b }
}

/// Reads the output of `git status --porcelain=v1 -z --branch`.
pub fn parse(out: &[u8]) -> GitStatus {
    let text = String::from_utf8_lossy(out);
    let mut status = GitStatus::default();
    let mut parts = text.split('\0');
    while let Some(part) = parts.next() {
        if let Some(head) = part.strip_prefix("## ") {
            status.branch = branch_name(head);
            continue;
        }
        let mut chars = part.chars();
        let (Some(x), Some(y), Some(' ')) = (chars.next(), chars.next(), chars.next()) else { continue };
        let path = chars.as_str();
        if path.is_empty() {
            continue;
        }
        // The original name of a rename or copy follows as its own item.
        if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            parts.next();
        }
        let letter = letter_for(x, y);
        if letter != ' ' {
            status.files.push((path.to_string(), letter));
        }
    }
    status
}

/// The branch from a `## ...` header line (without the `## `).
fn branch_name(head: &str) -> Option<String> {
    if head.starts_with("HEAD (no branch)") {
        return Some("HEAD (detached)".into());
    }
    let head = head
        .strip_prefix("No commits yet on ")
        .or_else(|| head.strip_prefix("Initial commit on "))
        .unwrap_or(head);
    // `main...origin/main [ahead 1]`: the local branch comes first.
    let name = head.split("...").next().unwrap_or(head);
    let name = name.split(" [").next().unwrap_or(name);
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

impl GitStatus {
    /// Status letter per name inside the folder `dir` (path relative to the
    /// work tree root, `/` separators, empty = the root itself): a file's own
    /// letter, a folder's strongest letter of anything below it.
    pub fn marks_in(&self, dir: &str) -> HashMap<String, char> {
        let dir = dir.trim_matches('/');
        let mut marks: HashMap<String, char> = HashMap::new();
        for (path, letter) in &self.files {
            let rest = if dir.is_empty() {
                path.as_str()
            } else {
                match path.strip_prefix(dir).and_then(|r| r.strip_prefix('/')) {
                    Some(rest) => rest,
                    None => continue,
                }
            };
            let name = rest.split('/').next().unwrap_or(rest);
            if name.is_empty() {
                continue;
            }
            match marks.get_mut(name) {
                Some(old) if rank(*old) >= rank(*letter) => {}
                Some(old) => *old = *letter,
                None => {
                    marks.insert(name.to_string(), *letter);
                }
            }
        }
        marks
    }
}

/// The work tree root above `dir` (the folder holding `.git`, a folder or,
/// for worktrees and submodules, a file).
pub fn find_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// Runs git for the work tree at `root`; gives up (killing it) after `limit`.
pub fn query(root: &Path, limit: Duration) -> Result<GitStatus, QueryError> {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--branch", "--untracked-files=normal", "--ignored=no"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { QueryError::NoGit } else { QueryError::Failed }
    })?;
    let mut stdout = child.stdout.take().ok_or(QueryError::Failed)?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(QueryError::Failed);
            }
        }
    };
    let out = reader.join().map_err(|_| QueryError::Failed)?;
    if !status.success() {
        return Err(QueryError::Failed);
    }
    Ok(parse(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(s: &GitStatus) -> Vec<(&str, char)> {
        s.files.iter().map(|(p, c)| (p.as_str(), *c)).collect()
    }

    #[test]
    fn branch_headers() {
        assert_eq!(parse(b"## main...origin/main [ahead 1]\0").branch.as_deref(), Some("main"));
        assert_eq!(parse(b"## main...origin/main [ahead 1, behind 2]\0").branch.as_deref(), Some("main"));
        assert_eq!(parse(b"## feature/x\0").branch.as_deref(), Some("feature/x"));
        assert_eq!(parse(b"## HEAD (no branch)\0").branch.as_deref(), Some("HEAD (detached)"));
        assert_eq!(parse(b"## No commits yet on main\0").branch.as_deref(), Some("main"));
        assert_eq!(parse(b"## Initial commit on master\0").branch.as_deref(), Some("master"));
        assert_eq!(parse(b"").branch, None);
    }

    #[test]
    fn simple_codes() {
        let s = parse(b"## main\0 M src/a.rs\0M  b.rs\0A  new.txt\0 D gone.txt\0D  gone2.txt\0?? untracked.txt\0MM both.rs\0");
        assert_eq!(
            files(&s),
            [
                ("src/a.rs", 'M'),
                ("b.rs", 'M'),
                ("new.txt", 'A'),
                ("gone.txt", 'D'),
                ("gone2.txt", 'D'),
                ("untracked.txt", '?'),
                ("both.rs", 'M'),
            ]
        );
    }

    #[test]
    fn rename_skips_the_old_name() {
        let s = parse(b"## main\0R  new name.txt\0old name.txt\0 M after.txt\0RM x.rs\0y.rs\0");
        assert_eq!(files(&s), [("new name.txt", 'R'), ("after.txt", 'M'), ("x.rs", 'M')]);
    }

    #[test]
    fn conflicts() {
        let s = parse(b"UU a\0AA b\0DD c\0AU d\0UD e\0DU f\0UA g\0");
        assert!(s.files.iter().all(|(_, c)| *c == 'U'), "{s:?}");
        assert_eq!(s.files.len(), 7);
    }

    #[test]
    fn spaces_and_unicode() {
        let s = parse("## main\0?? sub dir/my file.txt\0 M \u{010d}esk\u{00fd}/\u{65e5}\u{672c}.txt\0".as_bytes());
        assert_eq!(files(&s), [("sub dir/my file.txt", '?'), ("\u{010d}esk\u{00fd}/\u{65e5}\u{672c}.txt", 'M')]);
    }

    #[test]
    fn ignores_junk() {
        let s = parse(b"\0x\0 \0?? \0!! ignored\0");
        assert!(s.files.is_empty(), "{s:?}");
    }

    #[test]
    fn marks_aggregate_by_priority() {
        let s = parse(b"## main\0?? src/a/new.rs\0 D src/a/old.rs\0 M src/b.rs\0A  top.txt\0?? build/\0UU src/c/x\0 M other/z\0");
        let root = s.marks_in("");
        assert_eq!(root.get("src"), Some(&'U'));
        assert_eq!(root.get("top.txt"), Some(&'A'));
        assert_eq!(root.get("build"), Some(&'?'));
        assert_eq!(root.get("other"), Some(&'M'));
        let src = s.marks_in("src");
        assert_eq!(src.get("a"), Some(&'D'), "deleted beats untracked");
        assert_eq!(src.get("b.rs"), Some(&'M'));
        assert_eq!(src.get("c"), Some(&'U'));
        assert_eq!(src.len(), 3);
        // Not a prefix match on partial names.
        assert!(s.marks_in("sr").is_empty());
        assert_eq!(s.marks_in("src/a/").len(), 2);
        let mixed = parse(b"A  d/a\0 M d/b\0R  d/c\0old\0");
        assert_eq!(mixed.marks_in("").get("d"), Some(&'M'));
        let ar = parse(b"R  d/c\0old\0A  d/e\0");
        assert_eq!(ar.marks_in("").get("d"), Some(&'A'));
    }

    #[test]
    fn finds_root_by_dot_git() {
        let base = std::env::temp_dir().join(format!("rc_git_root_{}", std::process::id()));
        let deep = base.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_root(&deep).filter(|r| r.starts_with(&base)), None);
        // A `.git` file (worktree / submodule) counts too.
        std::fs::write(base.join("a").join(".git"), "gitdir: elsewhere").unwrap();
        assert_eq!(find_root(&deep), Some(base.join("a")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn real_repo_when_git_is_installed() {
        let git = |dir: &Path, args: &[&str]| {
            Command::new("git").arg("-C").arg(dir).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status()
        };
        let base = std::env::temp_dir().join(format!("rc_git_repo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        if !git(&base, &["init", "-q"]).is_ok_and(|s| s.success()) {
            let _ = std::fs::remove_dir_all(&base);
            return; // no git here
        }
        std::fs::write(base.join("sub").join("tracked.txt"), "one").unwrap();
        let _ = git(&base, &["add", "."]);
        let _ = git(
            &base,
            &["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "x"],
        );
        std::fs::write(base.join("sub").join("tracked.txt"), "two").unwrap();
        std::fs::write(base.join("new file.txt"), "n").unwrap();
        let status = query(&base, Duration::from_secs(20)).unwrap();
        assert!(status.branch.is_some());
        let marks = status.marks_in("");
        assert_eq!(marks.get("sub"), Some(&'M'));
        assert_eq!(marks.get("new file.txt"), Some(&'?'));
        assert_eq!(find_root(&base.join("sub")), Some(base.clone()));
        let _ = std::fs::remove_dir_all(&base);
    }
}
