//! Splitting a file into parts and joining them again (Total Commander
//! style): parts are `name.001`, `name.002`, ..., and `name.crc` holds
//! `filename=`, `size=` and `crc32=` lines to check the joined file. This is
//! the pure part (names, the crc file, sizes with units, finding the parts);
//! the copying is done by the `Split` / `Combine` jobs in `ops.rs`.

use std::fs;
use std::path::{Path, PathBuf};

/// Digits of the part number at least (`.001`).
pub const MIN_DIGITS: usize = 3;

const MB: u64 = 1024 * 1024;

/// Part sizes offered in the dialog: label and bytes.
pub const PRESETS: [(&str, u64); 4] = [
    ("100 MB", 100 * MB),
    ("700 MB", 700 * MB),
    ("1 GB", 1024 * MB),
    ("4095 MB (FAT32)", 4095 * MB),
];

/// Name of part `n` (1-based) of `name`: `name.001`; wider when there are
/// more than 999 parts, or `digits` is wider (a joined set keeps its width).
pub fn part_name(name: &str, n: u64, digits: usize) -> String {
    format!("{name}.{n:0width$}", width = digits.max(MIN_DIGITS))
}

/// Number of parts of a file of `size` bytes cut every `part` bytes.
pub fn part_count(size: u64, part: u64) -> u64 {
    if part == 0 {
        return 0;
    }
    size.div_ceil(part)
}

/// For a file name ending in `.<digits>`: (the name before it, the number, the number of digits).
pub fn parse_part(file_name: &str) -> Option<(&str, u64, usize)> {
    let (base, ext) = file_name.rsplit_once('.')?;
    if base.is_empty() || ext.is_empty() || ext.len() > 18 || !ext.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((base, ext.parse().ok()?, ext.len()))
}

/// Name of the crc file of `name`.
pub fn crc_name(name: &str) -> String {
    format!("{name}.crc")
}

/// Contents of a `.crc` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrcFile {
    pub filename: String,
    pub size: u64,
    pub crc32: u32,
}

impl CrcFile {
    pub fn format(&self) -> String {
        format!("filename={}\r\nsize={}\r\ncrc32={:08X}\r\n", self.filename, self.size, self.crc32)
    }

    pub fn parse(text: &str) -> Result<CrcFile, String> {
        let (mut filename, mut size, mut crc32) = (None, None, None);
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "filename" => filename = Some(value.to_string()),
                "size" => size = Some(value.parse::<u64>().map_err(|_| format!("Bad size line: {line}"))?),
                "crc32" => {
                    crc32 = Some(u32::from_str_radix(value, 16).map_err(|_| format!("Bad crc32 line: {line}"))?)
                }
                _ => {}
            }
        }
        match (filename, size, crc32) {
            (Some(filename), Some(size), Some(crc32)) => Ok(CrcFile { filename, size, crc32 }),
            (None, _, _) => Err("No filename= line".into()),
            (_, None, _) => Err("No size= line".into()),
            _ => Err("No crc32= line".into()),
        }
    }
}

/// Parses a part size: a number with an optional unit (B, KB, MB, GB, also
/// K, M, G; 1024-based, case-insensitive, decimals allowed). A plain number
/// means megabytes.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ',')).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.replace(',', ".").parse().map_err(|_| format!("'{text}' is not a size"))?;
    let unit = unit.trim().to_ascii_lowercase();
    let factor = match unit.as_str() {
        "" | "m" | "mb" | "mib" => MB,
        "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "g" | "gb" | "gib" => 1024 * MB,
        _ => return Err(format!("Unknown unit '{}' (use B, KB, MB or GB)", unit)),
    };
    let bytes = (number * factor as f64).round();
    if !bytes.is_finite() || bytes < 1.0 {
        return Err("The part size must be at least 1 byte".into());
    }
    if bytes >= u64::MAX as f64 {
        return Err("That part size is too large".into());
    }
    Ok(bytes as u64)
}

/// The part files to join, starting at `first` (`x.001`): `x.002`, ... in
/// the same folder until the first missing one. Higher numbers existing
/// after a gap is an error naming the missing part.
pub fn find_parts(first: &Path) -> Result<Vec<PathBuf>, String> {
    let name = first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some((base, start, digits)) = parse_part(&name) else {
        return Err(format!("{name} is not a part file (name.001)"));
    };
    let dir = first.parent().map(Path::to_path_buf).unwrap_or_default();
    let exists = |n: u64| dir.join(part_name(base, n, digits)).is_file();
    if !exists(start) {
        return Err(format!("Part {} not found", part_name(base, start, digits)));
    }
    let mut parts = vec![dir.join(&name)];
    let mut n = start + 1;
    while exists(n) {
        parts.push(dir.join(part_name(base, n, digits)));
        n += 1;
    }
    // Is there anything numbered higher after the gap?
    let later = fs::read_dir(if dir.as_os_str().is_empty() { Path::new(".") } else { &dir })
        .map(|rd| {
            rd.flatten().any(|e| {
                let file = e.file_name().to_string_lossy().into_owned();
                parse_part(&file).is_some_and(|(b, k, d)| b.eq_ignore_ascii_case(base) && d == digits && k > n)
            })
        })
        .unwrap_or(false);
    if later {
        return Err(format!("Part {} is missing", part_name(base, n, digits)));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let p = std::env::temp_dir().join(format!("rc-split-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn part_names() {
        assert_eq!(part_name("a.zip", 1, 3), "a.zip.001");
        assert_eq!(part_name("a.zip", 1000, 3), "a.zip.1000");
        assert_eq!(part_name("a", 7, 4), "a.0007");
        assert_eq!(parse_part("a.zip.012"), Some(("a.zip", 12, 3)));
        assert_eq!(parse_part("a.zip"), None);
        assert_eq!(parse_part(".001"), None);
        assert_eq!(part_count(10, 5), 2);
        assert_eq!(part_count(11, 5), 3);
        assert_eq!(part_count(0, 5), 0);
    }

    #[test]
    fn crc_file_round_trip() {
        let c = CrcFile { filename: "my file.iso".into(), size: 123456789, crc32: 0xDEADBEEF };
        let text = c.format();
        assert!(text.contains("crc32=DEADBEEF"));
        assert_eq!(CrcFile::parse(&text).unwrap(), c);
        assert_eq!(CrcFile::parse("filename=a\nsize=1\ncrc32=0000000a").unwrap().crc32, 10);
        assert!(CrcFile::parse("filename=a\nsize=1").is_err());
        assert!(CrcFile::parse("filename=a\nsize=x\ncrc32=1").is_err());
    }

    #[test]
    fn sizes_with_units() {
        assert_eq!(parse_size("100MB"), Ok(100 * MB));
        assert_eq!(parse_size(" 100 mb "), Ok(100 * MB));
        assert_eq!(parse_size("1.5 GB"), Ok(1536 * MB));
        assert_eq!(parse_size("700"), Ok(700 * MB));
        assert_eq!(parse_size("2048 B"), Ok(2048));
        assert_eq!(parse_size("64kb"), Ok(65536));
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("5 TB").is_err());
        assert!(parse_size("0").is_err());
    }

    #[test]
    fn finds_consecutive_parts_and_names_a_missing_one() {
        let t = Tmp::new("find");
        for n in ["x.001", "x.002", "x.003", "y.001", "y.003"] {
            fs::write(t.0.join(n), b"1").unwrap();
        }
        let parts = find_parts(&t.0.join("x.001")).unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2], t.0.join("x.003"));
        let err = find_parts(&t.0.join("y.001")).unwrap_err();
        assert!(err.contains("y.002"), "{err}");
        // A lone first part is fine; a non-part name is refused.
        fs::write(t.0.join("z.001"), b"1").unwrap();
        assert_eq!(find_parts(&t.0.join("z.001")).unwrap().len(), 1);
        assert!(find_parts(&t.0.join("x.txt")).is_err());
    }
}
