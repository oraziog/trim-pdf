//! Positional reads of the file, so a 6–8 GB PDF is never loaded.
//!
//! Every read is bounded and happens at an absolute offset: the crate holds the cross-reference
//! index and whatever small object it is parsing, never a copy of the document. Positional reads
//! use the platform's own pread-style call (`seek_read` / `read_at`), which needs no locking and
//! no `unsafe`. Sequential work — copying a stream into a part — streams through a small buffer.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// An open PDF file that can be read at any offset.
pub(crate) struct Source {
    file: File,
    path: PathBuf,
    len: u64,
}

impl Source {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let file = File::open(path).map_err(|e| Error::io(path, e))?;
        let len = file.metadata().map_err(|e| Error::io(path, e))?.len();
        Ok(Self { file, path: path.to_path_buf(), len })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Read exactly `buf.len()` bytes at `offset`. A short read is an error: callers size their
    /// buffer from the index, so a short read means the file changed under us.
    pub(crate) fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        let mut done = 0_usize;
        while done < buf.len() {
            let at = offset.saturating_add(done as u64);
            let n = read_at(&self.file, &mut buf[done..], at).map_err(|e| Error::io(&self.path, e))?;
            if n == 0 {
                return Err(Error::Corrupt(format!("il file finisce al byte {at}, prima dell'ultimo byte che l'indice si aspetta")));
            }
            done += n;
        }
        Ok(())
    }

    /// Up to `len` bytes at `offset`, clamped to the end of the file: a short result is normal
    /// near the end of the document.
    pub(crate) fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
        let available = self.len.saturating_sub(offset).min(len as u64);
        let available = usize::try_from(available).unwrap_or(usize::MAX);
        if available == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0_u8; available];
        self.read_exact_at(offset, &mut buf)?;
        Ok(buf)
    }

    /// Fill `buf` from `offset`, stopping at the end of the file. A partial result is normal
    /// here: the caller is streaming through the document rather than reading a known object.
    pub(crate) fn read_into(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let mut done = 0_usize;
        while done < buf.len() {
            let at = offset.saturating_add(done as u64);
            let n = read_at(&self.file, &mut buf[done..], at).map_err(|e| Error::io(&self.path, e))?;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

/// Other platforms: a cloned handle plus `seek`, which keeps the API identical without `unsafe`.
#[cfg(not(any(unix, windows)))]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut clone = file.try_clone()?;
    clone.seek(SeekFrom::Start(offset))?;
    clone.read(buf)
}
