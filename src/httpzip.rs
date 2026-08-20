//! A `Read + Seek` view over a remote file served with byte ranges.
//!
//! GitHub hands artifact downloads off to blob storage with a signed URL, and that
//! storage honours `Range`. Pointing the `zip` crate at one of these lets us pull a
//! handful of small config files out of a 700 MB pack instead of downloading it —
//! which is how the pristine "before" side of the merge is recovered on first run.

use anyhow::{bail, Context, Result};
use std::io::{self, Read, Seek, SeekFrom};

/// Blob storage rejects suffix ranges (`bytes=-N`), so every request is explicit.
const CHUNK: u64 = 1024 * 1024;

pub struct HttpRangeReader {
    agent: ureq::Agent,
    url: String,
    len: u64,
    pos: u64,
    /// One cached window of the remote file.
    buf: Vec<u8>,
    buf_start: u64,
    /// Re-resolves the signed URL when it expires mid-read.
    refresh: Option<Box<dyn Fn() -> Result<String> + Send>>,
}

impl HttpRangeReader {
    pub fn new(
        agent: ureq::Agent,
        url: String,
        refresh: Option<Box<dyn Fn() -> Result<String> + Send>>,
    ) -> Result<Self> {
        let len = content_length(&agent, &url)?;
        Ok(Self {
            agent,
            url,
            len,
            pos: 0,
            buf: Vec::new(),
            buf_start: 0,
            refresh,
        })
    }

    fn fetch(&mut self, start: u64, end_inclusive: u64) -> Result<Vec<u8>> {
        match range_request(&self.agent, &self.url, start, end_inclusive) {
            Ok(bytes) => Ok(bytes),
            Err(first) => {
                // Signed URLs are short-lived; ask for a fresh one and retry once.
                let Some(refresh) = &self.refresh else {
                    return Err(first);
                };
                let fresh = refresh().context("refresh expired artifact URL")?;
                self.url = fresh;
                range_request(&self.agent, &self.url, start, end_inclusive)
            }
        }
    }

    fn ensure(&mut self, want: u64) -> io::Result<()> {
        let in_buf = want >= self.buf_start && want < self.buf_start + self.buf.len() as u64;
        if in_buf {
            return Ok(());
        }
        let start = (want / CHUNK) * CHUNK;
        let end = (start + CHUNK - 1).min(self.len.saturating_sub(1));
        if start >= self.len {
            self.buf.clear();
            self.buf_start = start;
            return Ok(());
        }
        let bytes = self
            .fetch(start, end)
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.buf = bytes;
        self.buf_start = start;
        Ok(())
    }
}

impl Read for HttpRangeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        // Big sequential reads (entry payloads) bypass the window entirely.
        if out.len() as u64 >= CHUNK {
            let end = (self.pos + out.len() as u64 - 1).min(self.len - 1);
            let bytes = self
                .fetch(self.pos, end)
                .map_err(|e| io::Error::other(e.to_string()))?;
            let n = bytes.len().min(out.len());
            out[..n].copy_from_slice(&bytes[..n]);
            self.pos += n as u64;
            return Ok(n);
        }
        self.ensure(self.pos)?;
        let off = (self.pos - self.buf_start) as usize;
        if off >= self.buf.len() {
            return Ok(0);
        }
        let n = out.len().min(self.buf.len() - off);
        out[..n].copy_from_slice(&self.buf[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpRangeReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let new = match from {
            SeekFrom::Start(v) => v as i64,
            SeekFrom::End(v) => self.len as i64 + v,
            SeekFrom::Current(v) => self.pos as i64 + v,
        };
        if new < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of file",
            ));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

fn content_length(agent: &ureq::Agent, url: &str) -> Result<u64> {
    let resp = agent.head(url).call().context("HEAD remote zip")?;
    let len = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    match len {
        Some(n) if n > 0 => Ok(n),
        _ => bail!("remote zip did not report a Content-Length"),
    }
}

fn range_request(agent: &ureq::Agent, url: &str, start: u64, end: u64) -> Result<Vec<u8>> {
    let mut resp = agent
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .call()
        .with_context(|| format!("range {start}-{end}"))?;
    let status = resp.status().as_u16();
    if status != 206 && status != 200 {
        bail!("range request returned HTTP {status}");
    }
    let want = (end - start + 1) as usize;
    Ok(resp
        .body_mut()
        .with_config()
        .limit(want as u64 + 1024)
        .read_to_vec()?)
}
