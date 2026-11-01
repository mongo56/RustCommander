//! GNU sparse files stored in pax form (formats 0.0, 0.1 and 1.0, told by
//! `GNU.sparse.*` pax records). The `tar` crate expands old GNU sparse
//! members (type 'S') itself but knows nothing of these, so it would show
//! them under a made-up name (`GNUSparseFile.N/…`) with their packed data.
//! [`PaxSparse`] gives a member's real name and size and expands its data.

use std::io::{self, Read};

/// A sparse member described by pax records.
pub(crate) struct PaxSparse {
    /// The real name (`GNU.sparse.name`), if the records give one.
    pub name: Option<Vec<u8>>,
    /// The expanded size.
    pub size: u64,
    /// The data segments `(offset, length)`, or `None` (format 1.0) when
    /// they're listed at the start of the member's data.
    map: Option<Vec<(u64, u64)>>,
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "The tar archive has a damaged sparse file")
}

fn number(v: &[u8]) -> io::Result<u64> {
    std::str::from_utf8(v).ok().and_then(|s| s.trim().parse().ok()).ok_or_else(bad)
}

/// Segments as `offset, length` pairs, checked to be in order, apart, and
/// within `size`.
fn pairs(numbers: &[u64], size: u64) -> io::Result<Vec<(u64, u64)>> {
    if !numbers.len().is_multiple_of(2) {
        return Err(bad());
    }
    let mut map = Vec::with_capacity(numbers.len() / 2);
    let mut end = 0;
    for pair in numbers.chunks(2) {
        let (offset, len) = (pair[0], pair[1]);
        let stop = offset.checked_add(len).ok_or_else(bad)?;
        if offset < end || stop > size {
            return Err(bad());
        }
        end = stop;
        map.push((offset, len));
    }
    Ok(map)
}

impl PaxSparse {
    /// The sparse description in a member's pax `records`, if any.
    pub fn from_pax<'a>(records: impl Iterator<Item = (&'a [u8], &'a [u8])>) -> io::Result<Option<PaxSparse>> {
        let (mut any, mut name, mut size, mut real, mut major) = (false, None, None, None, None);
        let (mut offsets, mut lengths, mut map) = (Vec::new(), Vec::new(), None);
        for (k, v) in records {
            let Some(k) = k.strip_prefix(b"GNU.sparse.") else { continue };
            any = true;
            match k {
                b"name" => name = Some(v.to_vec()),
                b"size" => size = Some(number(v)?),
                b"realsize" => real = Some(number(v)?),
                b"major" => major = Some(number(v)?),
                b"offset" => offsets.push(number(v)?),
                b"numbytes" => lengths.push(number(v)?),
                b"map" => {
                    let list = v.split(|b| *b == b',').filter(|s| !s.is_empty());
                    map = Some(list.map(number).collect::<io::Result<Vec<u64>>>()?);
                }
                _ => {}
            }
        }
        if !any {
            return Ok(None);
        }
        let size = real.or(size).ok_or_else(bad)?;
        let map = match major {
            Some(1) => None,
            Some(0) | None => {
                let numbers = match map {
                    Some(numbers) => numbers,
                    None if offsets.len() == lengths.len() => offsets.into_iter().zip(lengths).flat_map(|(o, l)| [o, l]).collect(),
                    None => return Err(bad()),
                };
                Some(pairs(&numbers, size)?)
            }
            Some(_) => return Err(io::Error::other("The tar archive has a sparse file of an unknown kind")),
        };
        Ok(Some(PaxSparse { name, size, map }))
    }

    /// A reader of the expanded contents, from the member's stored data.
    pub fn expand<R: Read>(self, mut inner: R) -> io::Result<Expand<R>> {
        let map = match self.map {
            Some(map) => map,
            None => {
                // Format 1.0: decimal numbers, one per line (the count, then
                // offset and length pairs), padded to a 512-byte block.
                let mut used = 0u64;
                let mut line = || -> io::Result<u64> {
                    let mut digits = Vec::new();
                    loop {
                        let mut b = [0u8];
                        inner.read_exact(&mut b).map_err(|_| bad())?;
                        used += 1;
                        match b[0] {
                            b'\n' => return number(&digits),
                            b'0'..=b'9' if digits.len() < 20 => digits.push(b[0]),
                            _ => return Err(bad()),
                        }
                    }
                };
                let count = line()?;
                if count > 1 << 24 {
                    return Err(bad());
                }
                let numbers = (0..2 * count).map(|_| line()).collect::<io::Result<Vec<u64>>>()?;
                let pad = (512 - used % 512) % 512;
                if io::copy(&mut (&mut inner).take(pad), &mut io::sink())? != pad {
                    return Err(bad());
                }
                pairs(&numbers, self.size)?
            }
        };
        Ok(Expand { inner, map, next: 0, pos: 0, size: self.size })
    }
}

/// The expanded contents of a sparse member: its data segments, and zeros
/// between them.
pub(crate) struct Expand<R> {
    inner: R,
    map: Vec<(u64, u64)>,
    /// The first segment not wholly before `pos`.
    next: usize,
    pos: u64,
    size: u64,
}

impl<R: Read> Read for Expand<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.next < self.map.len() && self.map[self.next].0 + self.map[self.next].1 <= self.pos {
            self.next += 1;
        }
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let n = match self.map.get(self.next) {
            Some(&(offset, len)) if offset <= self.pos => {
                let n = (offset + len - self.pos).min(buf.len() as u64) as usize;
                let got = self.inner.read(&mut buf[..n])?;
                if got == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "The tar archive is truncated"));
                }
                got
            }
            next => {
                let hole_end = next.map_or(self.size, |s| s.0);
                let n = (hole_end - self.pos).min(buf.len() as u64) as usize;
                buf[..n].fill(0);
                n
            }
        };
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(list: &[(&str, &str)]) -> Vec<(Vec<u8>, Vec<u8>)> {
        list.iter().map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec())).collect()
    }

    fn parse(list: &[(&str, &str)]) -> io::Result<Option<PaxSparse>> {
        let r = records(list);
        PaxSparse::from_pax(r.iter().map(|(k, v)| (k.as_slice(), v.as_slice())))
    }

    fn read_all(sparse: PaxSparse, data: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        sparse.expand(data)?.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn expands_each_format() {
        let want = [&[0u8; 3][..], b"ab", &[0; 5], b"cde", &[0; 2]].concat();
        assert!(parse(&[("path", "x"), ("mtime", "1")]).unwrap().is_none());
        // 0.0: repeated offset / numbytes.
        let old = [("GNU.sparse.size", "15"), ("GNU.sparse.numblocks", "2"), ("GNU.sparse.offset", "3"),
            ("GNU.sparse.numbytes", "2"), ("GNU.sparse.offset", "10"), ("GNU.sparse.numbytes", "3")];
        let s = parse(&old).unwrap().unwrap();
        assert_eq!((s.name.is_none(), s.size), (true, 15));
        assert_eq!(read_all(s, b"abcde").unwrap(), want);
        // 0.1: one map record, a name.
        let s = parse(&[("GNU.sparse.size", "15"), ("GNU.sparse.map", "3,2,10,3"), ("GNU.sparse.name", "a/b")]).unwrap().unwrap();
        assert_eq!(s.name.as_deref(), Some(&b"a/b"[..]));
        assert_eq!(read_all(s, b"abcde").unwrap(), want);
        // 1.0: the map in the data, padded to a block.
        let v1 = [("GNU.sparse.major", "1"), ("GNU.sparse.minor", "0"), ("GNU.sparse.realsize", "15"), ("GNU.sparse.name", "n")];
        let mut data = b"2\n3\n2\n10\n3\n".to_vec();
        data.resize(512, 0);
        data.extend_from_slice(b"abcde");
        assert_eq!(read_all(parse(&v1).unwrap().unwrap(), &data).unwrap(), want);
        // A file that is all hole, and one ending in data.
        let s = parse(&[("GNU.sparse.size", "4"), ("GNU.sparse.map", "")]).unwrap().unwrap();
        assert_eq!(read_all(s, b"").unwrap(), [0; 4]);
        let s = parse(&[("GNU.sparse.size", "4"), ("GNU.sparse.map", "2,2")]).unwrap().unwrap();
        assert_eq!(read_all(s, b"xy").unwrap(), b"\0\0xy");
    }

    #[test]
    fn rejects_bad_maps() {
        for bad in [
            &[("GNU.sparse.size", "4"), ("GNU.sparse.map", "2,3")][..],
            &[("GNU.sparse.size", "9"), ("GNU.sparse.map", "4,2,1,1")],
            &[("GNU.sparse.size", "9"), ("GNU.sparse.map", "1")],
            &[("GNU.sparse.map", "1,1")],
            &[("GNU.sparse.size", "x")],
            &[("GNU.sparse.major", "2"), ("GNU.sparse.realsize", "1")],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        // Data shorter than the map says.
        let s = parse(&[("GNU.sparse.size", "9"), ("GNU.sparse.map", "1,4")]).unwrap().unwrap();
        assert!(read_all(s, b"ab").is_err());
        let v1 = [("GNU.sparse.major", "1"), ("GNU.sparse.realsize", "9")];
        assert!(read_all(parse(&v1).unwrap().unwrap(), b"1\n0\n").is_err());
    }
}
