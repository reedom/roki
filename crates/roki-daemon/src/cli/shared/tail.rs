//! Tail-suffix readers for byte- and line-oriented file slicing.
//!
//! `tail_bytes` seeks directly to `len - n` and reads only the suffix.
//! `tail_lines` walks the file backward in fixed-size chunks so a
//! multi-MB capture never has to be slurped to retrieve its last few
//! lines.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Chunk size used by `tail_lines` when walking the file backwards.
/// Power-of-two, larger than any reasonable single log line, small
/// enough that a few iterations cover most --tail N requests.
const TAIL_CHUNK_BYTES: u64 = 64 * 1024;

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
///
/// Implementation note: walks the file backward in [`TAIL_CHUNK_BYTES`]
/// windows, counting newlines as it grows the working buffer. Allocates
/// O(window) memory until enough newlines are seen or BOF is reached,
/// so a 200 MB capture file with `n = 50` lines stays bounded.
pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok(Vec::new());
    }
    // Window grows toward BOF. `window_start` is the absolute file offset
    // at which `buf` begins; `buf` always contains bytes[window_start..len].
    let mut window_start = len;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let next_start = window_start.saturating_sub(TAIL_CHUNK_BYTES);
        let read_len = (window_start - next_start) as usize;
        let mut chunk = vec![0u8; read_len];
        f.seek(SeekFrom::Start(next_start))?;
        f.read_exact(&mut chunk)?;
        // Prepend chunk in front of buf so buf stays in file order.
        chunk.extend_from_slice(&buf);
        buf = chunk;
        window_start = next_start;

        // Skip a final trailing newline so it doesn't count as a delimiter.
        let last_is_nl = *buf.last().unwrap() == b'\n';
        let scan_end = if last_is_nl { buf.len() - 1 } else { buf.len() };
        let mut newlines: usize = 0;
        let mut found_start: Option<usize> = None;
        for i in (0..scan_end).rev() {
            if buf[i] == b'\n' {
                newlines += 1;
                if newlines == n {
                    found_start = Some(i + 1);
                    break;
                }
            }
        }
        if let Some(start) = found_start {
            return Ok(buf[start..].to_vec());
        }
        if window_start == 0 {
            // BOF reached — fewer than n line boundaries; return everything.
            return Ok(buf);
        }
    }
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

    #[test]
    fn lines_handles_file_larger_than_chunk_window() {
        // Build a file whose last lines straddle the TAIL_CHUNK_BYTES
        // boundary so the loop must consume more than one chunk.
        let mut body = String::new();
        // 100k lines of "X\n" = 200 KB, well above 64 KB chunk.
        for _ in 0..100_000 {
            body.push_str("X\n");
        }
        body.push_str("LAST1\n");
        body.push_str("LAST2\n");
        let f = fixture(&body);
        let out = tail_lines(f.path(), 2).unwrap();
        assert_eq!(out, b"LAST1\nLAST2\n");
    }

    #[test]
    fn lines_reads_all_when_n_exceeds_file() {
        // Multi-chunk file, ask for many more lines than exist.
        let mut body = String::new();
        for i in 0..1000 {
            body.push_str(&format!("line{i}\n"));
        }
        let f = fixture(&body);
        let out = tail_lines(f.path(), 10_000).unwrap();
        assert_eq!(out.len(), body.len());
    }
}
