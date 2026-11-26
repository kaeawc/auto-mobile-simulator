//! vfio-user server: serves a PCI function of emu's to a frontend that runs the guest's CPU (QEMU
//! under TCG, software mode on Linux; issue #55), for devices vhost-user can't carry because they
//! aren't virtio: the goldfish address space, which the ranchu gralloc requires (its mapper aborts
//! with "GoldfishAddressSpaceHostMemoryAllocator failed to open" without it).
//!
//! The protocol is QEMU's `docs/interop/vfio-user.rst` with `<linux/vfio.h>`'s PCI layout (regions
//! 0–5 the BARs, 6 the ROM, 7 config space; IRQs INTx, MSI, MSI-X, ERR, REQ). The frontend owns
//! the PCI bus and forwards every config cycle and every access to a message-based BAR here; a BAR
//! offered with an fd it maps straight into the guest. Guest RAM arrives with `DMA_MAP` as fds, the
//! way vhost-user's memory table does, and is mapped once.
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;

use emu_hvf::GuestRam;
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage, SendFlags};

use crate::bus::MmioDevice;
use crate::devices::goldfish_as::{DEVICE, GoldfishAddressSpace, VENDOR};
use crate::devices::pci::{Bar, BarKind, ConfigSpace, PciFunction};

/// Commands (vfio-user.rst, "Commands").
pub mod cmd {
    pub const VERSION: u16 = 1;
    pub const DMA_MAP: u16 = 2;
    pub const DMA_UNMAP: u16 = 3;
    pub const DEVICE_GET_INFO: u16 = 4;
    pub const DEVICE_GET_REGION_INFO: u16 = 5;
    pub const DEVICE_GET_IRQ_INFO: u16 = 7;
    pub const DEVICE_SET_IRQS: u16 = 8;
    pub const REGION_READ: u16 = 9;
    pub const REGION_WRITE: u16 = 10;
    pub const DEVICE_RESET: u16 = 13;
}

const HEADER: usize = 16;
const TYPE_REPLY: u32 = 0x1;
const FLAG_NO_REPLY: u32 = 0x10;
const FLAG_ERROR: u32 = 0x20;
/// The largest message accepted: a region write of up to `max_data_xfer_size`.
const MAX_XFER: usize = 1 << 20;
const MAX_FDS: usize = 8;

/// `<linux/vfio.h>`: PCI regions and IRQs.
pub const CONFIG_REGION: u32 = 7;
const NUM_REGIONS: u32 = 9;
const NUM_IRQS: u32 = 5;
const DEVICE_FLAGS_RESET: u32 = 1 << 0;
const DEVICE_FLAGS_PCI: u32 = 1 << 1;
const REGION_READ: u32 = 1 << 0;
const REGION_WRITE: u32 = 1 << 1;
const REGION_MMAP: u32 = 1 << 2;
const IRQ_INFO_EVENTFD: u32 = 1 << 0;

const ENOSYS: u32 = 38;
const EINVAL: u32 = 22;

/// One region as a function describes it.
#[derive(Clone, Copy, Debug, Default)]
pub struct RegionInfo {
    pub size: u64,
    pub flags: u32,
}

/// A PCI function served over vfio-user.
pub trait Function: Send {
    fn region(&self, index: u32) -> RegionInfo;
    /// The fd the frontend maps a `REGION_MMAP` region from (at offset 0).
    fn region_fd(&self, _index: u32) -> Option<std::os::fd::BorrowedFd<'_>> {
        None
    }
    fn read(&mut self, region: u32, offset: u64, data: &mut [u8]);
    fn write(&mut self, region: u32, offset: u64, data: &[u8]);
    /// Guest RAM, once the frontend has shared it.
    fn guest_ram(&mut self, _ram: Arc<GuestRam>) {}
    fn reset(&mut self) {}
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("vfio-user socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("vfio-user: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn protocol(msg: impl Into<String>) -> Error {
    Error::Protocol(msg.into())
}

/// One message.
#[derive(Debug, Default)]
pub struct Msg {
    pub id: u16,
    pub command: u16,
    pub flags: u32,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

fn u32_at(b: &[u8], o: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(o..o + 4).ok_or_else(|| protocol("payload too short"))?.try_into().unwrap_or_default()))
}

fn u64_at(b: &[u8], o: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(b.get(o..o + 8).ok_or_else(|| protocol("payload too short"))?.try_into().unwrap_or_default()))
}

/// Reads one message with any fds attached to its header; `None` at a clean end of stream.
pub fn read_msg(sock: &mut UnixStream) -> Result<Option<Msg>> {
    let mut head = [0u8; HEADER];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let (mut got, mut fds) = (0, Vec::new());
    while got < HEADER {
        let n = rustix::net::recvmsg(&*sock, &mut [std::io::IoSliceMut::new(&mut head[got..])], &mut control, RecvFlags::CMSG_CLOEXEC)
            .map_err(std::io::Error::from)?
            .bytes;
        for m in control.drain() {
            if let RecvAncillaryMessage::ScmRights(r) = m {
                fds.extend(r);
            }
        }
        if n == 0 {
            return if got == 0 { Ok(None) } else { Err(protocol("connection closed inside a message header")) };
        }
        got += n;
    }
    let id = u16::from_le_bytes([head[0], head[1]]);
    let command = u16::from_le_bytes([head[2], head[3]]);
    let size = u32::from_le_bytes(head[4..8].try_into().unwrap_or_default()) as usize;
    let flags = u32::from_le_bytes(head[8..12].try_into().unwrap_or_default());
    if !(HEADER..=HEADER + MAX_XFER + 64).contains(&size) {
        return Err(protocol(format!("command {command}: {size}-byte message")));
    }
    let mut payload = vec![0u8; size - HEADER];
    sock.read_exact(&mut payload)?;
    Ok(Some(Msg { id, command, flags, payload, fds }))
}

/// A reply to `m`: its id and command, `payload` (empty for an error, which carries `errno`).
pub fn reply(m: &Msg, payload: &[u8], errno: Option<u32>) -> Vec<u8> {
    let body = if errno.is_some() { &[][..] } else { payload };
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&m.id.to_le_bytes());
    out.extend_from_slice(&m.command.to_le_bytes());
    out.extend_from_slice(&((HEADER + body.len()) as u32).to_le_bytes());
    out.extend_from_slice(&(TYPE_REPLY | if errno.is_some() { FLAG_ERROR } else { 0 }).to_le_bytes());
    out.extend_from_slice(&errno.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// The guest RAM the frontend shared: the fd that maps guest-physical address 0 (other fds are
/// other memory the frontend maps into the guest), mapped once in whole.
#[derive(Default)]
struct Dma {
    file: Option<(u64, u64)>,
    ram: Option<Arc<GuestRam>>,
}

impl Dma {
    /// `DMA_MAP` of `[iova, iova + size)` from `fd` at `offset`. Returns the RAM when this map is
    /// the one that brought it.
    fn map(&mut self, iova: u64, offset: u64, fd: Option<&OwnedFd>) -> Result<Option<Arc<GuestRam>>> {
        let Some(fd) = fd else { return Ok(None) };
        if self.ram.is_some() || iova != 0 {
            return Ok(None);
        }
        let st = rustix::fs::fstat(fd).map_err(std::io::Error::from)?;
        let ram = GuestRam::map_shared(iova.wrapping_sub(offset), st.st_size as u64, fd.as_fd(), 0)
            .map_err(|e| protocol(format!("mapping guest RAM: {e}")))?;
        let ram = Arc::new(ram);
        self.file = Some((st.st_dev, st.st_ino));
        self.ram = Some(ram.clone());
        Ok(Some(ram))
    }
}

/// Handles one message; the reply payload, or an errno.
fn handle(f: &mut dyn Function, dma: &mut Dma, m: &mut Msg) -> Result<std::result::Result<Vec<u8>, u32>> {
    let p = &m.payload;
    Ok(match m.command {
        cmd::VERSION => {
            // Major 0, minor 0, and what this server takes: a subset of the frontend's offer.
            let mut out = vec![0u8; 4];
            out.extend_from_slice(br#"{"capabilities":{"max_msg_fds":8,"max_data_xfer_size":1048576,"pgsizes":4096}}"#);
            out.push(0);
            Ok(out)
        }
        cmd::DMA_MAP => {
            let (offset, iova) = (u64_at(p, 8)?, u64_at(p, 16)?);
            if let Some(ram) = dma.map(iova, offset, m.fds.first())? {
                f.guest_ram(ram);
            }
            Ok(Vec::new())
        }
        cmd::DMA_UNMAP => Ok(p.clone()),
        cmd::DEVICE_GET_INFO => {
            let mut out = 16u32.to_le_bytes().to_vec();
            out.extend_from_slice(&(DEVICE_FLAGS_PCI | DEVICE_FLAGS_RESET).to_le_bytes());
            out.extend_from_slice(&NUM_REGIONS.to_le_bytes());
            out.extend_from_slice(&NUM_IRQS.to_le_bytes());
            Ok(out)
        }
        cmd::DEVICE_GET_REGION_INFO => {
            let index = u32_at(p, 8)?;
            let r = f.region(index);
            let mut out = 32u32.to_le_bytes().to_vec();
            out.extend_from_slice(&r.flags.to_le_bytes());
            out.extend_from_slice(&index.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // no capabilities
            out.extend_from_slice(&r.size.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes()); // mmap offset in the fd
            Ok(out)
        }
        cmd::DEVICE_GET_IRQ_INFO => {
            let index = u32_at(p, 8)?;
            // No interrupts: the function's config space says so (interrupt pin 0).
            let mut out = 16u32.to_le_bytes().to_vec();
            out.extend_from_slice(&IRQ_INFO_EVENTFD.to_le_bytes());
            out.extend_from_slice(&index.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            Ok(out)
        }
        cmd::DEVICE_SET_IRQS => Ok(Vec::new()),
        cmd::REGION_READ => {
            let (offset, region, count) = (u64_at(p, 0)?, u32_at(p, 8)?, u32_at(p, 12)? as usize);
            if count > MAX_XFER {
                return Ok(Err(EINVAL));
            }
            let mut out = p.get(..16).ok_or_else(|| protocol("REGION_READ header"))?.to_vec();
            let mut data = vec![0u8; count];
            f.read(region, offset, &mut data);
            out.extend_from_slice(&data);
            Ok(out)
        }
        cmd::REGION_WRITE => {
            let (offset, region, count) = (u64_at(p, 0)?, u32_at(p, 8)?, u32_at(p, 12)? as usize);
            let data = p.get(16..16 + count).ok_or_else(|| protocol("REGION_WRITE data"))?;
            f.write(region, offset, data);
            Ok(p[..16].to_vec())
        }
        cmd::DEVICE_RESET => {
            f.reset();
            Ok(Vec::new())
        }
        _ => Err(ENOSYS),
    })
}

/// Sends `msg` with `fd` attached.
fn send_with_fd(sock: &mut UnixStream, msg: &[u8], fd: std::os::fd::BorrowedFd<'_>) -> Result<()> {
    let fds = [fd];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(protocol("no room for the region fd"));
    }
    let sent =
        rustix::net::sendmsg(&*sock, &[std::io::IoSlice::new(msg)], &mut control, SendFlags::empty()).map_err(std::io::Error::from)?;
    if sent < msg.len() {
        sock.write_all(&msg[sent..])?;
    }
    Ok(())
}

/// Serves one frontend connection until it closes.
pub fn serve_connection(mut sock: UnixStream, f: &Mutex<Box<dyn Function>>) -> Result<()> {
    let mut dma = Dma::default();
    while let Some(mut m) = read_msg(&mut sock)? {
        let mut func = f.lock().unwrap_or_else(PoisonError::into_inner);
        let result = handle(func.as_mut(), &mut dma, &mut m)?;
        if m.flags & FLAG_NO_REPLY != 0 {
            continue;
        }
        match result {
            Ok(payload) => {
                let out = reply(&m, &payload, None);
                let index = (m.command == cmd::DEVICE_GET_REGION_INFO).then(|| u32_at(&m.payload, 8).unwrap_or(u32::MAX));
                match index.filter(|&i| func.region(i).flags & REGION_MMAP != 0).and_then(|i| func.region_fd(i)) {
                    Some(fd) => send_with_fd(&mut sock, &out, fd)?,
                    None => sock.write_all(&out)?,
                }
            }
            Err(errno) => sock.write_all(&reply(&m, &[], Some(errno)))?,
        }
    }
    Ok(())
}

/// Listens on `path` (replacing a stale socket) and serves frontends one after another.
pub fn serve(path: &Path, name: &str, f: Box<dyn Function>) -> std::io::Result<JoinHandle<()>> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let f = Arc::new(Mutex::new(f));
    let name = name.to_string();
    std::thread::Builder::new().name(format!("vfio-{name}")).spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { continue };
            if let Err(e) = serve_connection(sock, &f) {
                eprintln!("emu: vfio-user {name}: {e}");
            }
        }
    })
}

/// The goldfish address space's host-memory area, as the renderer's DMA path needs to see it: the
/// mapping of the area at the guest-physical address the guest gave it.
pub type AreaHandle = Arc<OnceLock<Arc<GuestRam>>>;

/// The goldfish address space (PCI 607d:f153) as a vfio-user function: config space, its
/// registers (BAR0, by message) and its 16 GiB host-memory area (BAR1, a sparse memfd the
/// frontend maps into the guest). The device model needs guest RAM and the area at its guest
/// address, so it's built on the first register access, when both are known.
pub struct GoldfishAs {
    config: ConfigSpace,
    area_fd: OwnedFd,
    area_size: u64,
    ram: AreaHandle,
    dev: Option<GoldfishAddressSpace>,
    area: AreaHandle,
}

const AS_REGS: u64 = 0x1000;

impl GoldfishAs {
    pub fn new(area_size: u64) -> std::io::Result<GoldfishAs> {
        let area_fd = rustix::fs::memfd_create("goldfish-address-space", rustix::fs::MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&area_fd, area_size)?;
        let config = ConfigSpace::new(PciFunction {
            vendor: VENDOR,
            device: DEVICE,
            class: 0x05_00_00,
            bars: vec![
                (0, Bar { kind: BarKind::Mem32, addr: 0, size: AS_REGS }),
                (1, Bar { kind: BarKind::Mem64Prefetch, addr: 0, size: area_size }),
            ],
        });
        Ok(GoldfishAs { config, area_fd, area_size, ram: Arc::default(), dev: None, area: Arc::default() })
    }

    /// Where the renderer's DMA translator finds the area once the guest has placed it.
    pub fn area(&self) -> AreaHandle {
        self.area.clone()
    }

    /// Guest RAM, once the frontend has shared it (for the same translator).
    pub fn guest_ram_handle(&self) -> AreaHandle {
        self.ram.clone()
    }

    fn device(&mut self) -> Option<&GoldfishAddressSpace> {
        if self.dev.is_none() {
            let ram = self.ram.get()?.clone();
            let base = self.config.bar_addr(1);
            if base == 0 {
                return None;
            }
            let area = match GuestRam::map_shared(base, self.area_size, self.area_fd.as_fd(), 0) {
                Ok(a) => Arc::new(a),
                Err(e) => {
                    eprintln!("emu: goldfish address space: mapping its area: {e}");
                    return None;
                }
            };
            let _ = self.area.set(area.clone());
            self.dev = Some(GoldfishAddressSpace::new(ram, area));
        }
        self.dev.as_ref()
    }
}

impl Function for GoldfishAs {
    fn region(&self, index: u32) -> RegionInfo {
        match index {
            0 => RegionInfo { size: AS_REGS, flags: REGION_READ | REGION_WRITE },
            1 => RegionInfo { size: self.area_size, flags: REGION_READ | REGION_WRITE | REGION_MMAP },
            CONFIG_REGION => RegionInfo { size: 256, flags: REGION_READ | REGION_WRITE },
            _ => RegionInfo::default(),
        }
    }

    fn region_fd(&self, index: u32) -> Option<std::os::fd::BorrowedFd<'_>> {
        (index == 1).then(|| self.area_fd.as_fd())
    }

    fn read(&mut self, region: u32, offset: u64, data: &mut [u8]) {
        data.fill(0);
        match region {
            CONFIG_REGION => self.config.read(offset as usize, data),
            0 => {
                if let Some(d) = self.device() {
                    d.read(offset, data);
                }
            }
            1 => {
                if let Some(a) = self.area.get() {
                    let _ = a.read(a.base() + offset, data);
                }
            }
            _ => {}
        }
    }

    fn write(&mut self, region: u32, offset: u64, data: &[u8]) {
        match region {
            CONFIG_REGION => self.config.write(offset as usize, data),
            0 => {
                if let Some(d) = self.device() {
                    d.write(offset, data);
                }
            }
            1 => {
                if let Some(a) = self.area.get() {
                    let _ = a.write(a.base() + offset, data);
                }
            }
            _ => {}
        }
    }

    fn guest_ram(&mut self, ram: Arc<GuestRam>) {
        let _ = self.ram.set(ram);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QEMU's side of the conversation.
    struct Client {
        sock: UnixStream,
        id: u16,
    }

    impl Client {
        fn call(&mut self, command: u16, payload: &[u8], fd: Option<&OwnedFd>) -> (u32, Vec<u8>, Vec<OwnedFd>) {
            self.id += 1;
            let mut msg = self.id.to_le_bytes().to_vec();
            msg.extend_from_slice(&command.to_le_bytes());
            msg.extend_from_slice(&((HEADER + payload.len()) as u32).to_le_bytes());
            msg.extend_from_slice(&0u32.to_le_bytes());
            msg.extend_from_slice(&0u32.to_le_bytes());
            msg.extend_from_slice(payload);
            match fd {
                Some(fd) => send_with_fd(&mut self.sock, &msg, fd.as_fd()).expect("send"),
                None => self.sock.write_all(&msg).expect("send"),
            }
            let r = read_msg(&mut self.sock).expect("reply").expect("not closed");
            assert_eq!((r.id, r.command, r.flags & 0xf), (self.id, command, TYPE_REPLY));
            (r.flags, r.payload, r.fds)
        }

        fn region(&mut self, write: Option<&[u8]>, region: u32, offset: u64, count: u32) -> Vec<u8> {
            let mut p = offset.to_le_bytes().to_vec();
            p.extend_from_slice(&region.to_le_bytes());
            p.extend_from_slice(&count.to_le_bytes());
            let command = match write {
                Some(d) => {
                    p.extend_from_slice(d);
                    cmd::REGION_WRITE
                }
                None => cmd::REGION_READ,
            };
            let (flags, payload, _) = self.call(command, &p, None);
            assert_eq!(flags & FLAG_ERROR, 0);
            payload[16..].to_vec()
        }
    }

    /// What QEMU's vfio-user-pci does at realize and the guest then: version, regions (the area
    /// comes with its fd), config space and BAR sizing, guest RAM by DMA_MAP, BAR placement, and
    /// a register write and read the device model handles.
    #[test]
    fn the_goldfish_address_space_comes_up_the_way_qemu_drives_it() {
        let (a, b) = UnixStream::pair().expect("pair");
        let f: Box<dyn Function> = Box::new(GoldfishAs::new(64 << 20).expect("device"));
        let f = Arc::new(Mutex::new(f));
        let server = {
            let f = f.clone();
            std::thread::spawn(move || serve_connection(b, &f))
        };
        let mut c = Client { sock: a, id: 0 };
        let (_, v, _) = c.call(cmd::VERSION, b"\0\0\0\0{}\0", None);
        assert!(String::from_utf8_lossy(&v[4..]).contains("max_msg_fds"));
        let (_, info, _) = c.call(cmd::DEVICE_GET_INFO, &[0; 16], None);
        assert_eq!((u32_at(&info, 8).ok(), u32_at(&info, 12).ok()), (Some(9), Some(5)));
        let req = |i: u32| {
            let mut p = 32u32.to_le_bytes().to_vec();
            p.extend_from_slice(&[0; 4]);
            p.extend_from_slice(&i.to_le_bytes());
            p.extend_from_slice(&[0; 20]);
            p
        };
        let (_, r1, fds) = c.call(cmd::DEVICE_GET_REGION_INFO, &req(1), None);
        assert_eq!((u64_at(&r1, 16).ok(), fds.len()), (Some(64 << 20), 1), "the area comes with its fd");
        let (_, r0, fds) = c.call(cmd::DEVICE_GET_REGION_INFO, &req(0), None);
        assert_eq!((u64_at(&r0, 16).ok(), fds.len()), (Some(0x1000), 0), "registers go by message");
        assert_eq!(c.region(None, CONFIG_REGION, 0, 4), 0xf153_607du32.to_le_bytes());
        c.region(Some(&u32::MAX.to_le_bytes()), CONFIG_REGION, 0x10, 4);
        assert_eq!(c.region(None, CONFIG_REGION, 0x10, 4), 0xffff_f000u32.to_le_bytes(), "BAR0 sizes as 4 KiB");
        // Guest RAM, then the BARs where the guest put them.
        let ram = rustix::fs::memfd_create("ram", rustix::fs::MemfdFlags::CLOEXEC).expect("memfd");
        rustix::fs::ftruncate(&ram, 0x100_0000).expect("size");
        let mut map = 32u32.to_le_bytes().to_vec();
        map.extend_from_slice(&3u32.to_le_bytes());
        for v in [0u64, 0, 0x100_0000] {
            map.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(c.call(cmd::DMA_MAP, &map, Some(&ram)).0 & FLAG_ERROR, 0);
        c.region(Some(&0xfe00_0000u32.to_le_bytes()), CONFIG_REGION, 0x10, 4);
        c.region(Some(&0x0u32.to_le_bytes()), CONFIG_REGION, 0x14, 4);
        c.region(Some(&0x8u32.to_le_bytes()), CONFIG_REGION, 0x18, 4);
        // GUEST_PAGE_SIZE (offset 8) is written and read back by the device model.
        c.region(Some(&4096u32.to_le_bytes()), 0, 8, 4);
        assert_eq!(c.region(None, 0, 8, 4), 4096u32.to_le_bytes());
        let area = f.lock().unwrap_or_else(PoisonError::into_inner).region(1);
        assert_eq!(area.size, 64 << 20);
        let (flags, _, _) = c.call(99, &[], None);
        assert_ne!(flags & FLAG_ERROR, 0, "unknown commands get an error reply");
        drop(c);
        server.join().expect("join").expect("clean end");
    }
}
