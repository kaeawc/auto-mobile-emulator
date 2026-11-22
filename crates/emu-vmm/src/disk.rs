//! Disk images backing virtio-blk devices.
//!
//! - `Persistent`: a raw file the guest reads and writes in place. Sparse on APFS, so an empty
//!   10 GiB userdata costs nothing until written.
//! - `Ephemeral`: a read-only base (a system image in the SDK) with guest writes kept in
//!   memory for the life of the process — the stock emulator's behaviour for system and vendor
//!   images unless `-writable-system` is given.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use emu_hvf::GuestRam;

const CHUNK: u64 = 4096;

pub struct Disk {
    file: File,
    size: u64,
    overlay: Option<BTreeMap<u64, Box<[u8]>>>,
}

impl Disk {
    pub fn persistent(path: &Path) -> io::Result<Disk> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Disk { file, size, overlay: None })
    }

    pub fn ephemeral(path: &Path) -> io::Result<Disk> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Disk { file, size, overlay: Some(BTreeMap::new()) })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    fn check(&self, offset: u64, len: u64) -> io::Result<()> {
        match offset.checked_add(len) {
            Some(end) if end <= self.size => Ok(()),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "access beyond end of disk")),
        }
    }

    /// Reads `len` bytes at disk `offset` into guest memory at `gpa`.
    pub fn read_to_guest(&self, mem: &GuestRam, offset: u64, gpa: u64, len: u64) -> io::Result<()> {
        self.check(offset, len)?;
        let overlay = match &self.overlay {
            Some(o) if o.range(offset / CHUNK * CHUNK..offset + len).next().is_some() => o,
            // Fast path: one pread straight into guest memory.
            _ => return mem.read_from_file(gpa, len as usize, &self.file, offset),
        };
        let mut pos = 0;
        while pos < len {
            let off = offset + pos;
            let chunk = off / CHUNK * CHUNK;
            let in_chunk = off - chunk;
            let n = (CHUNK - in_chunk).min(len - pos);
            match overlay.get(&chunk) {
                Some(data) => mem
                    .write(gpa + pos, &data[in_chunk as usize..(in_chunk + n) as usize])
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                None => mem.read_from_file(gpa + pos, n as usize, &self.file, off)?,
            }
            pos += n;
        }
        Ok(())
    }

    /// Writes `len` bytes from guest memory at `gpa` to disk `offset`.
    pub fn write_from_guest(&mut self, mem: &GuestRam, offset: u64, gpa: u64, len: u64) -> io::Result<()> {
        self.check(offset, len)?;
        let Some(overlay) = self.overlay.as_mut() else {
            return mem.write_to_file(gpa, len as usize, &self.file, offset);
        };
        let mut pos = 0;
        while pos < len {
            let off = offset + pos;
            let chunk = off / CHUNK * CHUNK;
            let in_chunk = (off - chunk) as usize;
            let n = (CHUNK - in_chunk as u64).min(len - pos) as usize;
            let data = match overlay.entry(chunk) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => {
                    let mut base = vec![0u8; CHUNK as usize].into_boxed_slice();
                    read_at(&self.file, &mut base, chunk)?;
                    e.insert(base)
                }
            };
            mem.read(gpa + pos, &mut data[in_chunk..in_chunk + n]).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            pos += n as u64;
        }
        Ok(())
    }

    pub fn flush(&self) -> io::Result<()> {
        if self.overlay.is_some() {
            return Ok(());
        }
        // Plain fsync hands dirty pages to the drive. std's sync_data/sync_all use F_FULLFSYNC on
        // macOS, which also drains the drive's cache: it made guest FLUSHes (ext4 commits,
        // fsync-heavy first boot) the dominant cost of booting. Data written before a flush has
        // already reached the host page cache, so an emulator crash loses nothing either way.
        rustix::fs::fsync(&self.file).map_err(io::Error::from)
    }
}

fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => break, // past EOF reads as zeroes
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Creates a sparse raw image of `size` bytes if `path` doesn't exist.
pub fn ensure_sparse(path: &Path, size: u64) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let f = OpenOptions::new().write(true).create_new(true).open(path)?;
    f.set_len(size)
}

/// Copies `src` to `dst` if `dst` doesn't exist, as an APFS clone when possible (instant, no
/// extra space until either side is written).
pub fn ensure_copy(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.exists() {
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: u64 = 0x4000_0000;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("emu-disk-{}-{name}", std::process::id()))
    }

    #[test]
    fn ephemeral_keeps_writes_in_memory() {
        let path = tmp("eph");
        std::fs::write(&path, vec![0xaa; 3 * CHUNK as usize]).expect("write");
        let mem = GuestRam::new(RAM, 0x10_0000).expect("ram");
        let mut d = Disk::ephemeral(&path).expect("open");
        mem.write(RAM, &[1, 2, 3, 4]).expect("w");
        // Straddles chunks 0 and 1.
        d.write_from_guest(&mem, CHUNK - 2, RAM, 4).expect("write");
        d.read_to_guest(&mem, 0, RAM + 0x10000, 3 * CHUNK).expect("read");
        let mut got = vec![0u8; 3 * CHUNK as usize];
        mem.read(RAM + 0x10000, &mut got).expect("r");
        assert_eq!(&got[CHUNK as usize - 3..CHUNK as usize + 3], &[0xaa, 1, 2, 3, 4, 0xaa]);
        assert!(got[2 * CHUNK as usize..].iter().all(|&b| b == 0xaa));
        assert_eq!(std::fs::read(&path).expect("read")[CHUNK as usize - 2], 0xaa, "base untouched");
        assert!(d.read_to_guest(&mem, 3 * CHUNK - 1, RAM, 2).is_err(), "past the end");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistent_writes_through() {
        let path = tmp("pers");
        ensure_sparse(&path, 2 * CHUNK).expect("create");
        let mem = GuestRam::new(RAM, 0x10_0000).expect("ram");
        let mut d = Disk::persistent(&path).expect("open");
        mem.write(RAM, b"data").expect("w");
        d.write_from_guest(&mem, 512, RAM, 4).expect("write");
        d.flush().expect("flush");
        assert_eq!(&std::fs::read(&path).expect("read")[512..516], b"data");
        let _ = std::fs::remove_file(path);
    }
}
