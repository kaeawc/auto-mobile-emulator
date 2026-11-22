//! virtio-blk, served by one worker thread per disk so the vCPU that kicked the queue goes
//! straight back to the guest.
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use emu_hvf::GuestRam;

use super::mmio::{F_RING_EVENT_IDX, F_RING_INDIRECT_DESC};
use super::queue::{Chain, Queue};
use super::{Activation, Doorbell, ID_BLOCK, INT_USED_RING, Interrupt, VirtioDevice};
use crate::disk::Disk;

const F_SEG_MAX: u64 = 1 << 2;
const F_FLUSH: u64 = 1 << 9;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const SECTOR: u64 = 512;
const QUEUE_SIZE: u16 = 256;
const SEG_MAX: u32 = 126;

pub struct Blk {
    disk: Arc<Mutex<Disk>>,
    capacity_sectors: u64,
    serial: String,
    worker: Option<(Arc<Doorbell>, JoinHandle<()>)>,
}

impl Blk {
    pub fn new(disk: Disk, serial: impl Into<String>) -> Blk {
        let capacity_sectors = disk.size() / SECTOR;
        Blk { disk: Arc::new(Mutex::new(disk)), capacity_sectors, serial: serial.into(), worker: None }
    }

    fn stop_worker(&mut self) {
        if let Some((bell, handle)) = self.worker.take() {
            bell.stop();
            let _ = handle.join();
        }
    }
}

impl Drop for Blk {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl VirtioDevice for Blk {
    fn device_id(&self) -> u32 {
        ID_BLOCK
    }

    fn features(&self) -> u64 {
        F_SEG_MAX | F_FLUSH | F_RING_INDIRECT_DESC | F_RING_EVENT_IDX
    }

    fn num_queues(&self) -> usize {
        1
    }

    fn queue_max_size(&self) -> u16 {
        QUEUE_SIZE
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 0x40];
        cfg[0x00..0x08].copy_from_slice(&self.capacity_sectors.to_le_bytes());
        cfg[0x0c..0x10].copy_from_slice(&SEG_MAX.to_le_bytes());
        // Logical blocks stay 512 bytes (no VIRTIO_BLK_F_BLK_SIZE): the images' GPTs are laid
        // out in 512-byte LBAs.
        let off = offset as usize;
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(off + i).copied().unwrap_or(0);
        }
    }

    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let cfg = act.queues.first().copied().flatten().ok_or("block request queue not enabled")?;
        self.stop_worker();
        let bell = Arc::new(Doorbell::default());
        let mut w = Worker {
            mem: act.mem,
            irq: act.irq,
            queue: Queue::new(cfg, act.features & F_RING_EVENT_IDX != 0),
            disk: self.disk.clone(),
            serial: self.serial.clone(),
        };
        let b = bell.clone();
        let handle = std::thread::Builder::new()
            .name(format!("blk-{}", self.serial))
            .spawn(move || {
                while b.wait() {
                    w.drain();
                }
            })
            .map_err(|e| e.to_string())?;
        self.worker = Some((bell, handle));
        Ok(())
    }

    fn notify(&mut self, _queue: u16) {
        if let Some((bell, _)) = &self.worker {
            bell.ring();
        }
    }

    fn reset(&mut self) {
        self.stop_worker();
    }
}

struct Worker {
    mem: Arc<GuestRam>,
    irq: Arc<Interrupt>,
    queue: Queue,
    disk: Arc<Mutex<Disk>>,
    serial: String,
}

impl Worker {
    fn drain(&mut self) {
        let mut completed = false;
        loop {
            let chain = match self.queue.pop(&self.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("emu: blk-{}: {e}", self.serial);
                    break;
                }
            };
            let written = self.handle(&chain);
            if let Err(e) = self.queue.add_used(&self.mem, chain.head, written) {
                eprintln!("emu: blk-{}: {e}", self.serial);
                break;
            }
            completed = true;
        }
        if completed && self.queue.should_notify(&self.mem) {
            self.irq.raise(INT_USED_RING);
        }
    }

    /// Executes one request; returns the number of bytes written into guest buffers.
    fn handle(&mut self, chain: &Chain) -> u32 {
        let descs = &chain.descs;
        let Some(status) = descs.last().filter(|d| d.write && d.len >= 1) else {
            return 0;
        };
        let header = match descs.first().filter(|d| !d.write && d.len >= 16) {
            Some(h) => h,
            None => return 0,
        };
        let mut hdr = [0u8; 16];
        if self.mem.read(header.addr, &mut hdr).is_err() {
            return 0;
        }
        let typ = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap_or_default());
        let data = &descs[1..descs.len() - 1];
        let mut written = 0u32;
        let result: Result<(), u8> = (|| match typ {
            T_IN => {
                let disk = self.disk.lock().map_err(|_| S_IOERR)?;
                let mut off = sector.checked_mul(SECTOR).ok_or(S_IOERR)?;
                for d in data {
                    if !d.write {
                        return Err(S_IOERR);
                    }
                    disk.read_to_guest(&self.mem, off, d.addr, u64::from(d.len)).map_err(|_| S_IOERR)?;
                    off += u64::from(d.len);
                    written += d.len;
                }
                Ok(())
            }
            T_OUT => {
                let mut disk = self.disk.lock().map_err(|_| S_IOERR)?;
                let mut off = sector.checked_mul(SECTOR).ok_or(S_IOERR)?;
                for d in data {
                    if d.write {
                        return Err(S_IOERR);
                    }
                    disk.write_from_guest(&self.mem, off, d.addr, u64::from(d.len)).map_err(|_| S_IOERR)?;
                    off += u64::from(d.len);
                }
                Ok(())
            }
            T_FLUSH => self.disk.lock().map_err(|_| S_IOERR)?.flush().map_err(|_| S_IOERR),
            T_GET_ID => {
                let mut id = [0u8; 20];
                let s = self.serial.as_bytes();
                id[..s.len().min(20)].copy_from_slice(&s[..s.len().min(20)]);
                let d = data.first().filter(|d| d.write).ok_or(S_IOERR)?;
                let n = (d.len as usize).min(20);
                self.mem.write(d.addr, &id[..n]).map_err(|_| S_IOERR)?;
                written += n as u32;
                Ok(())
            }
            _ => Err(S_UNSUPP),
        })();
        let code = match result {
            Ok(()) => S_OK,
            Err(c) => c,
        };
        if self.mem.write(status.addr, &[code]).is_ok() {
            written += 1;
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::TestLine;
    use crate::virtio::queue::tests::{Driver, RAM_BASE};

    fn request(mem: &GuestRam, drv: &mut Driver, typ: u32, sector: u64, data: (u64, u32, bool)) -> u16 {
        let hdr = RAM_BASE + 0x20000;
        let mut h = [0u8; 16];
        h[..4].copy_from_slice(&typ.to_le_bytes());
        h[8..].copy_from_slice(&sector.to_le_bytes());
        mem.write(hdr, &h).expect("hdr");
        drv.push(mem, &[(hdr, 16, false), data, (RAM_BASE + 0x21000, 1, true)])
    }

    #[test]
    fn reads_writes_and_reports_status() {
        let path = std::env::temp_dir().join(format!("emu-blk-{}", std::process::id()));
        std::fs::write(&path, vec![7u8; 8192]).expect("write");
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        let irq = Arc::new(Interrupt::new(line.clone()));
        let mut drv = Driver::new(16);
        let blk = Blk::new(Disk::persistent(&path).expect("open"), "vda");
        let mut w =
            Worker { mem: mem.clone(), irq: irq.clone(), queue: Queue::new(drv.cfg, false), disk: blk.disk.clone(), serial: "vda".into() };
        let buf = RAM_BASE + 0x30000;
        mem.write(buf, &[9u8; 512]).expect("w");
        request(&mem, &mut drv, T_OUT, 2, (buf, 512, false));
        w.drain();
        assert_eq!(mem.read_u16(RAM_BASE + 0x21000).expect("status") & 0xff, u16::from(S_OK));
        assert!(line.level());
        request(&mem, &mut drv, T_IN, 1, (buf, 1024, true));
        w.drain();
        let mut got = vec![0u8; 1024];
        mem.read(buf, &mut got).expect("r");
        assert!(got[..512].iter().all(|&b| b == 7) && got[512..].iter().all(|&b| b == 9));
        assert_eq!(drv.used(&mem, 1).1, 1025);
        request(&mem, &mut drv, T_IN, 16, (buf, 512, true)); // past the end
        w.drain();
        let mut st = [0u8; 1];
        mem.read(RAM_BASE + 0x21000, &mut st).expect("r");
        assert_eq!(st[0], S_IOERR);
        request(&mem, &mut drv, 99, 0, (buf, 512, true));
        w.drain();
        mem.read(RAM_BASE + 0x21000, &mut st).expect("r");
        assert_eq!(st[0], S_UNSUPP);
        let _ = std::fs::remove_file(path);
    }
}
