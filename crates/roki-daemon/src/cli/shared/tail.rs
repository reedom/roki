//! Tail-suffix readers for byte- and line-oriented file slicing.
//!
//! Both functions read the file in its entirety for simplicity since
//! roki state files (cycle.log, events.jsonl) are small and bounded.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Return the last `n` bytes of `path`, or the whole file when it is
/// shorter than `n`.
pub fn tail_bytes(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(n);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Return the last `n` newline-delimited lines of `path` (including
/// their terminators). A missing trailing newline on the last line is
/// preserved verbatim.
pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    if n == 0 || bytes.is_empty() {
        return Ok(Vec::new());
    }
    // Walk backwards through the byte slice counting newline boundaries.
    let mut newlines: usize = 0;
    // Skip a final trailing newline so it doesn't count as a delimiter for line n.
    let last_is_nl = *bytes.last().unwrap() == b'\n';
    let scan_end = if last_is_nl {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    for i in (0..scan_end).rev() {
        if bytes[i] == b'\n' {
            newlines += 1;
            if newlines == n {
                let start = i + 1;
                return Ok(bytes[start..].to_vec());
            }
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        f.write_all(body.as_bytes()).unwrap();
        f
    }

    #[test]
    fn lines_returns_last_n_lines() {
        let f = fixture("a\nb\nc\nd\ne\n");
        let out = tail_lines(f.path(), 2).unwrap();
        assert_eq!(out, b"d\ne\n");
    }

    #[test]
    fn lines_returns_whole_file_when_fewer_than_n() {
        let f = fixture("a\nb\n");
        let out = tail_lines(f.path(), 10).unwrap();
        assert_eq!(out, b"a\nb\n");
    }

    #[test]
    fn lines_handles_missing_trailing_newline() {
        let f = fixture("x\ny\nz");
        let out = tail_lines(f.path(), 2).unwrap();
        assert_eq!(out, b"y\nz");
    }

    #[test]
    fn bytes_returns_suffix() {
        let f = fixture("abcdef");
        let out = tail_bytes(f.path(), 3).unwrap();
        assert_eq!(out, b"def");
    }

    #[test]
    fn bytes_returns_whole_file_when_shorter() {
        let f = fixture("xy");
        let out = tail_bytes(f.path(), 100).unwrap();
        assert_eq!(out, b"xy");
    }
}
