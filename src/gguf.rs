//! Rejects truncated GGUF model files before they reach a native loader.
//!
//! The GGUF header (metadata and tensor table) sits at the front of the file,
//! so a download that was cut short still parses. parakeet.cpp and audio.cpp
//! trust the declared tensor sizes. This check compares them with the real
//! file length, so a short file fails fast and loud instead of driving a
//! garbage-sized GPU allocation. Unknown tensor types are skipped, so a newer
//! quant type never gives a false alarm.

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use anyhow::{Result, bail};

/// (block size, bytes per block) of the ggml tensor types.
fn type_size(t: u32) -> Option<(u64, u64)> {
    Some(match t {
        0 => (1, 4),       // F32
        1 => (1, 2),       // F16
        2 => (32, 18),     // Q4_0
        3 => (32, 20),     // Q4_1
        6 => (32, 22),     // Q5_0
        7 => (32, 24),     // Q5_1
        8 => (32, 34),     // Q8_0
        9 => (32, 36),     // Q8_1
        10 => (256, 84),   // Q2_K
        11 => (256, 110),  // Q3_K
        12 => (256, 144),  // Q4_K
        13 => (256, 176),  // Q5_K
        14 => (256, 210),  // Q6_K
        15 => (256, 292),  // Q8_K
        18 => (256, 98),   // IQ3_XXS
        20 => (32, 18),    // IQ4_NL
        21 => (256, 110),  // IQ3_S
        23 => (256, 136),  // IQ4_XS
        24 => (1, 1),      // I8
        25 => (1, 2),      // I16
        26 => (1, 4),      // I32
        27 | 28 => (1, 8), // I64, F64
        30 => (1, 2),      // BF16
        39 => (32, 17),    // MXFP4
        _ => return None,
    })
}

const STRING: u32 = 8;
const ARRAY: u32 = 9;

fn scalar_size(t: u32) -> Option<u64> {
    Some(match t {
        0 | 1 | 7 => 1,
        2 | 3 => 2,
        4..=6 => 4,
        10..=12 => 8,
        _ => return None,
    })
}

struct Reader<R> {
    inner: R,
}

impl<R: Read + Seek> Reader<R> {
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut b = [0; N];
        self.inner
            .read_exact(&mut b)
            .map_err(|_| anyhow::anyhow!("header truncated"))?;
        Ok(b)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes()?))
    }
    fn skip(&mut self, n: u64) -> Result<()> {
        let n = i64::try_from(n).map_err(|_| anyhow::anyhow!("bad length"))?;
        self.inner.seek_relative(n)?;
        Ok(())
    }
    fn string(&mut self) -> Result<Vec<u8>> {
        let len = self.u64()?;
        if len > 1 << 20 {
            bail!("string length {len} is not plausible");
        }
        let mut s = vec![0; len as usize];
        self.inner
            .read_exact(&mut s)
            .map_err(|_| anyhow::anyhow!("header truncated"))?;
        Ok(s)
    }
    fn skip_value(&mut self, t: u32) -> Result<()> {
        if let Some(n) = scalar_size(t) {
            return self.skip(n);
        }
        match t {
            STRING => {
                let len = self.u64()?;
                self.skip(len)
            }
            ARRAY => {
                let elem = self.u32()?;
                let count = self.u64()?;
                if let Some(n) = scalar_size(elem) {
                    return self.skip(n.checked_mul(count).ok_or_else(|| anyhow::anyhow!("bad array"))?);
                }
                for _ in 0..count {
                    self.skip_value(elem)?;
                }
                Ok(())
            }
            _ => bail!("unknown GGUF value type {t}"),
        }
    }
}

/// A string value from the GGUF metadata, for example `tokenizer.chat_template`.
/// None if the file has no such key or the value is no string.
pub fn metadata_string(path: &Path, wanted: &str) -> Result<Option<String>> {
    let mut r = Reader {
        inner: BufReader::new(File::open(path)?),
    };
    if &r.bytes::<4>()? != b"GGUF" {
        bail!("not a GGUF file (bad magic)");
    }
    r.u32()?; // version
    r.u64()?; // tensor count
    let kvs = r.u64()?;
    for _ in 0..kvs {
        let key = r.string()?;
        let t = r.u32()?;
        if key == wanted.as_bytes() && t == STRING {
            return Ok(Some(String::from_utf8_lossy(&r.string()?).into_owned()));
        }
        r.skip_value(t)?;
    }
    Ok(None)
}

/// Minimum file size that the header declares: data start plus the end of the
/// last tensor.
pub fn expected_min_size(path: &Path) -> Result<u64> {
    let mut r = Reader {
        inner: BufReader::new(File::open(path)?),
    };
    if &r.bytes::<4>()? != b"GGUF" {
        bail!("not a GGUF file (bad magic)");
    }
    r.u32()?; // version
    let tensors = r.u64()?;
    let kvs = r.u64()?;
    let mut alignment = 32u64;
    for _ in 0..kvs {
        let key = r.string()?;
        let t = r.u32()?;
        match scalar_size(t) {
            Some(n) if key == b"general.alignment" => {
                let mut buf = [0u8; 8];
                r.inner.read_exact(&mut buf[..n as usize])?;
                alignment = u64::from_le_bytes(buf).max(1);
            }
            _ => r.skip_value(t)?,
        }
    }
    let mut max_end = 0u64;
    for _ in 0..tensors {
        r.string()?;
        let dims = r.u32()?;
        let mut elems = 1u64;
        for _ in 0..dims {
            elems = elems.saturating_mul(r.u64()?);
        }
        let t = r.u32()?;
        let offset = r.u64()?;
        if let Some((block, size)) = type_size(t) {
            max_end = max_end.max(offset.saturating_add(elems / block * size));
        }
    }
    let mut start = r.inner.stream_position()?;
    if start % alignment != 0 {
        start += alignment - start % alignment;
    }
    Ok(start + max_end)
}

/// Checks one file. Prints the problem and returns false if it is missing,
/// corrupt or too short.
pub fn check(path: &Path) -> bool {
    if !path.is_file() {
        eprintln!("MISSING {}", path.display());
        return false;
    }
    let want = match expected_min_size(path) {
        Ok(n) => n,
        Err(err) => {
            eprintln!("CORRUPT {}: {err}", path.display());
            return false;
        }
    };
    let got = path.metadata().map(|m| m.len()).unwrap_or(0);
    if got < want {
        eprintln!(
            "TRUNCATED {}: header declares at least {want} bytes, file is {got} bytes ({} bytes short)",
            path.display(),
            want - got
        );
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gguf(tensor_bytes: usize) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(b"GGUF");
        f.extend_from_slice(&3u32.to_le_bytes());
        f.extend_from_slice(&1u64.to_le_bytes()); // tensors
        f.extend_from_slice(&1u64.to_le_bytes()); // kvs
        let key = b"general.name";
        f.extend_from_slice(&(key.len() as u64).to_le_bytes());
        f.extend_from_slice(key);
        f.extend_from_slice(&STRING.to_le_bytes());
        f.extend_from_slice(&4u64.to_le_bytes());
        f.extend_from_slice(b"test");
        let name = b"w";
        f.extend_from_slice(&(name.len() as u64).to_le_bytes());
        f.extend_from_slice(name);
        f.extend_from_slice(&1u32.to_le_bytes()); // dims
        f.extend_from_slice(&64u64.to_le_bytes()); // 64 F32 = 256 bytes
        f.extend_from_slice(&0u32.to_le_bytes()); // F32
        f.extend_from_slice(&0u64.to_le_bytes()); // offset
        while f.len() % 32 != 0 {
            f.push(0);
        }
        f.extend(std::iter::repeat_n(0u8, tensor_bytes));
        f
    }

    #[test]
    fn complete_and_truncated_files() {
        let mut ok = tempfile::NamedTempFile::new().unwrap();
        ok.write_all(&gguf(256)).unwrap();
        assert!(check(ok.path()));
        let mut short = tempfile::NamedTempFile::new().unwrap();
        short.write_all(&gguf(100)).unwrap();
        assert!(!check(short.path()));
        let mut bad = tempfile::NamedTempFile::new().unwrap();
        bad.write_all(b"NOPE").unwrap();
        assert!(!check(bad.path()));
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn reads_a_string_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.gguf");
        let mut f = Vec::new();
        f.extend_from_slice(b"GGUF");
        f.extend_from_slice(&3u32.to_le_bytes());
        f.extend_from_slice(&0u64.to_le_bytes()); // tensors
        f.extend_from_slice(&2u64.to_le_bytes()); // kvs
        for (key, value) in [("general.name", "test"), ("tokenizer.chat_template", "{{ messages }}")] {
            f.extend_from_slice(&(key.len() as u64).to_le_bytes());
            f.extend_from_slice(key.as_bytes());
            f.extend_from_slice(&STRING.to_le_bytes());
            f.extend_from_slice(&(value.len() as u64).to_le_bytes());
            f.extend_from_slice(value.as_bytes());
        }
        std::fs::write(&path, f).unwrap();
        assert_eq!(
            metadata_string(&path, "tokenizer.chat_template").unwrap().as_deref(),
            Some("{{ messages }}")
        );
        assert_eq!(metadata_string(&path, "missing").unwrap(), None);
    }
}
