//! SHA-256 checksums of files: streaming hash with progress and cancel, the
//! `sha256sum` line format (`<hex> *<name>`) and its parser.

use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Length of a SHA-256 digest in hex digits.
const HEX_LEN: usize = 64;

/// Lower-case hex SHA-256 of the file. `progress` gets the bytes read so
/// far. `None` when `cancel` was set meanwhile.
pub fn hash_file(path: &Path, cancel: &AtomicBool, mut progress: impl FnMut(u64)) -> io::Result<Option<String>> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done);
    }
    Ok(Some(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()))
}

/// One line of a `.sha256` file (binary mode, as `sha256sum -b`).
pub fn format_line(hash: &str, name: &str) -> String {
    format!("{hash} *{name}")
}

/// A line of a checksum file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Lower-case hex digest.
    pub hash: String,
    /// File name or relative path as written.
    pub name: String,
}

/// Parses the text of a `.sha256` file: `<hex> *<name>` or `<hex>  <name>`
/// per line, LF or CRLF; blank lines and lines starting with `;` or `#` are
/// skipped. A malformed line gives an error naming its (1-based) number.
pub fn parse(text: &str) -> Result<Vec<Item>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut items = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        let bad = || format!("Line {}: not a SHA-256 checksum line", i + 1);
        let hash = line.get(..HEX_LEN).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit())).ok_or_else(bad)?;
        let rest = line[HEX_LEN..].strip_prefix(" *").or_else(|| line[HEX_LEN..].strip_prefix("  ")).ok_or_else(bad)?;
        if rest.is_empty() {
            return Err(bad());
        }
        items.push(Item { hash: hash.to_ascii_lowercase(), name: rest.to_owned() });
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn hash_bytes(bytes: &[u8]) -> String {
        let dir = std::env::temp_dir().join(format!("rc-checksum-{}-{}", std::process::id(), bytes.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.bin");
        std::fs::write(&file, bytes).unwrap();
        let mut last = 0;
        let hash = hash_file(&file, &AtomicBool::new(false), |n| last = n).unwrap().unwrap();
        assert_eq!(last, bytes.len() as u64);
        std::fs::remove_dir_all(&dir).unwrap();
        hash
    }

    #[test]
    fn known_vectors() {
        assert_eq!(hash_bytes(b""), EMPTY);
        assert_eq!(hash_bytes(b"abc"), ABC);
    }

    #[test]
    fn cancel_stops() {
        let file = std::env::temp_dir().join(format!("rc-checksum-cancel-{}.bin", std::process::id()));
        std::fs::write(&file, b"abc").unwrap();
        assert_eq!(hash_file(&file, &AtomicBool::new(true), |_| {}).unwrap(), None);
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn parses_both_modes_and_crlf() {
        let text = format!("# comment\r\n{ABC} *a.bin\r\n\r\n; x\r\n{EMPTY}  sub dir\\b c.txt\r\n");
        let items = parse(&text).unwrap();
        assert_eq!(items, [
            Item { hash: ABC.into(), name: "a.bin".into() },
            Item { hash: EMPTY.into(), name: "sub dir\\b c.txt".into() },
        ]);
        assert_eq!(parse(&format!("{ABC}  x y\n")).unwrap()[0].name, "x y");
        assert_eq!(parse(&format_line(ABC, "n m")).unwrap()[0], Item { hash: ABC.into(), name: "n m".into() });
    }

    #[test]
    fn rejects_malformed_lines() {
        let err = parse(&format!("{ABC} *ok\nnot a hash\n")).unwrap_err();
        assert!(err.starts_with("Line 2"), "{err}");
        assert!(parse(&format!("{ABC}x name")).is_err());
        assert!(parse(&format!("{ABC} *")).is_err());
        assert!(parse(&ABC[1..]).is_err());
    }
}
