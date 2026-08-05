/// A persistent one-sector write-back cache for the File Allocation Table.
///
/// All FAT entry reads/writes go through a freshly-built slice over the shared
/// `disk`, whose own buffering is evicted by interleaved file-data I/O. Because
/// clusters are usually allocated/followed sequentially (128 FAT32 entries per
/// 512-byte sector), buffering the current FAT sector here and only loading /
/// flushing it on a sector change collapses ~128 read-modify-write cycles into a
/// single read plus a single (mirrored) write. Data I/O bypasses this cache, so
/// it is not thrashed by it.
use core::cmp;
use embedded_io_async::{Read, Seek, SeekFrom, Write};

use crate::error::Error;
use crate::fs::{fat_slice, FileSystem, FsIoAdapter, ReadWriteSeek};
use crate::io::IoBase;

pub(crate) const FAT_CACHE_SIZE: usize = 512;

pub(crate) struct FatCache {
    pub(crate) window: Option<u64>,
    pub(crate) dirty: bool,
    pub(crate) buf: [u8; FAT_CACHE_SIZE],
}

impl FatCache {
    pub(crate) fn new() -> Self {
        Self {
            window: None,
            dirty: false,
            buf: [0; FAT_CACHE_SIZE],
        }
    }
}

pub(crate) struct CachedFatSlice<'a, IO: ReadWriteSeek, TP, OCC> {
    pub(crate) fs: &'a FileSystem<IO, TP, OCC>,
    offset: u64,
    size: u64,
}

impl<'a, IO: ReadWriteSeek, TP, OCC> CachedFatSlice<'a, IO, TP, OCC> {
    pub(crate) fn new(fs: &'a FileSystem<IO, TP, OCC>, size: u64) -> Self {
        Self { fs, offset: 0, size }
    }

    pub(crate) async fn ensure_window(&mut self, window: u64) -> Result<(), Error<IO::Error>> {
        let mut cache = self.fs.fat_cache.borrow_mut();
        if cache.window == Some(window) {
            return Ok(());
        }
        if cache.dirty {
            if let Some(old) = cache.window {
                let mut raw = fat_slice(FsIoAdapter { fs: self.fs }, &self.fs.bpb);
                raw.seek(SeekFrom::Start(old * FAT_CACHE_SIZE as u64)).await?;
                raw.write_all(&cache.buf).await?;
            }
            cache.dirty = false;
        }
        let mut raw = fat_slice(FsIoAdapter { fs: self.fs }, &self.fs.bpb);
        raw.seek(SeekFrom::Start(window * FAT_CACHE_SIZE as u64)).await?;
        raw.read_exact(&mut cache.buf).await?;
        cache.window = Some(window);
        Ok(())
    }
}

impl<IO: ReadWriteSeek, TP, OCC> IoBase for CachedFatSlice<'_, IO, TP, OCC> {
    type Error = Error<IO::Error>;
}

impl<IO: ReadWriteSeek, TP, OCC> Read for CachedFatSlice<'_, IO, TP, OCC> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let avail = self.size.saturating_sub(self.offset);
        let want = cmp::min(buf.len() as u64, avail) as usize;
        if want == 0 {
            return Ok(0);
        }
        let window = self.offset / FAT_CACHE_SIZE as u64;
        let in_window = (self.offset % FAT_CACHE_SIZE as u64) as usize;
        let n = cmp::min(want, FAT_CACHE_SIZE - in_window);
        self.ensure_window(window).await?;
        {
            let cache = self.fs.fat_cache.borrow();
            buf[..n].copy_from_slice(&cache.buf[in_window..in_window + n]);
        }
        self.offset += n as u64;
        Ok(n)
    }
}

impl<IO: ReadWriteSeek, TP, OCC> Write for CachedFatSlice<'_, IO, TP, OCC> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let avail = self.size.saturating_sub(self.offset);
        let want = cmp::min(buf.len() as u64, avail) as usize;
        if want == 0 {
            return Ok(0);
        }
        let window = self.offset / FAT_CACHE_SIZE as u64;
        let in_window = (self.offset % FAT_CACHE_SIZE as u64) as usize;
        let n = cmp::min(want, FAT_CACHE_SIZE - in_window);
        self.ensure_window(window).await?;
        {
            let mut cache = self.fs.fat_cache.borrow_mut();
            cache.buf[in_window..in_window + n].copy_from_slice(&buf[..n]);
            cache.dirty = true;
        }
        self.offset += n as u64;
        Ok(n)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.fs.flush_fat_cache().await
    }
}

impl<IO: ReadWriteSeek, TP, OCC> Seek for CachedFatSlice<'_, IO, TP, OCC> {
    async fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        self.offset = match pos {
            SeekFrom::Start(x) => x,
            SeekFrom::Current(x) => (self.offset as i64 + x) as u64,
            SeekFrom::End(x) => (self.size as i64 + x) as u64,
        };
        Ok(self.offset)
    }
}
