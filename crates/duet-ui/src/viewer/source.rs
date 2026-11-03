// SPDX-License-Identifier: MIT
//! The bytes behind the viewer (T-5.3.6): a positional reader over the
//! open file with a small LRU of 1 MiB chunks, so scrolling a page up
//! and down re-reads nothing and a long line's forward/backward walks
//! hit memory. Only the worker thread ever touches it -- the UI thread
//! does no I/O (design.md §9.6, the UI-thread guard).
//!
//! [`ByteSource`] is the seam the pure engine (`text.rs`, `search.rs`,
//! `hex.rs`) is written against; `Vec<u8>` implements it for the unit
//! tests, [`FileSource`] for real files.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

/// Chunk granularity of the cache and of every streaming scan (the
/// index builder, search).
pub(crate) const CHUNK: u64 = 1 << 20;
/// 64 MiB of cache per open viewer at most.
const CACHE_CHUNKS: usize = 64;

pub(crate) trait ByteSource {
    /// The length at open time. A file that grows afterwards is shown
    /// as it was opened (no `tail -f`); reopen to see the rest.
    fn len(&self) -> u64;
    /// `[offset, offset + len)` clamped to the file; shorter than `len`
    /// only at the end.
    fn read(&mut self, offset: u64, len: usize) -> std::io::Result<Vec<u8>>;
}

impl ByteSource for Vec<u8> {
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }

    fn read(&mut self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let total = Vec::len(self) as u64;
        let start = offset.min(total) as usize;
        let end = offset.saturating_add(len as u64).min(total) as usize;
        Ok(self[start..end].to_vec())
    }
}

pub(crate) struct FileSource {
    file: File,
    len: u64,
    /// Most recently used last.
    cache: Vec<(u64, Arc<Vec<u8>>)>,
}

impl FileSource {
    pub(crate) fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "is a directory",
            ));
        }
        Ok(Self {
            file,
            len: meta.len(),
            cache: Vec::new(),
        })
    }

    fn chunk(&mut self, index: u64) -> std::io::Result<Arc<Vec<u8>>> {
        if let Some(pos) = self.cache.iter().position(|(i, _)| *i == index) {
            let entry = self.cache.remove(pos);
            let data = entry.1.clone();
            self.cache.push(entry);
            return Ok(data);
        }
        let start = index * CHUNK;
        let want = self.len.saturating_sub(start).min(CHUNK) as usize;
        let mut buf = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            match self.file.read_at(&mut buf[filled..], start + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        buf.truncate(filled);
        let data = Arc::new(buf);
        if self.cache.len() >= CACHE_CHUNKS {
            self.cache.remove(0);
        }
        self.cache.push((index, data.clone()));
        Ok(data)
    }
}

impl ByteSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&mut self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let end = offset.saturating_add(len as u64).min(self.len);
        if offset >= end {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            let chunk = self.chunk(pos / CHUNK)?;
            let within = (pos % CHUNK) as usize;
            let available = chunk.len().saturating_sub(within);
            let take = ((end - pos) as usize).min(available);
            if take == 0 {
                // The file shrank under us; serve what there is.
                break;
            }
            out.extend_from_slice(&chunk[within..within + take]);
            pos += take as u64;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_source_reads_across_chunk_boundaries_and_clamps_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let data: Vec<u8> = (0..(CHUNK as usize * 2 + 100))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&path, &data).unwrap();
        let mut src = FileSource::open(&path).unwrap();
        assert_eq!(src.len(), data.len() as u64);
        let span = src.read(CHUNK - 10, 20).unwrap();
        assert_eq!(span, &data[(CHUNK as usize - 10)..(CHUNK as usize + 10)]);
        let tail = src.read(src.len() - 5, 100).unwrap();
        assert_eq!(tail, &data[data.len() - 5..]);
        assert!(src.read(src.len() + 1, 10).unwrap().is_empty());
        // Second read of the same span is served from the cache.
        assert_eq!(src.read(CHUNK - 10, 20).unwrap(), span);
        assert_eq!(src.cache.len(), 3);
    }

    #[test]
    fn opening_a_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(FileSource::open(dir.path()).is_err());
    }
}
