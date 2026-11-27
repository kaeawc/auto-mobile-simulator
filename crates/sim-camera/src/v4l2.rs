//! Linux cameras: every `/dev/video*` node that can capture video, described by
//! `VIDIOC_QUERYCAP` and its formats by `VIDIOC_ENUM_FMT`, `VIDIOC_ENUM_FRAMESIZES` and
//! `VIDIOC_ENUM_FRAMEINTERVALS`. v4l2loopback devices (and the vivid and akvcam test drivers) are
//! marked virtual. Opening a node for these ioctls doesn't start a capture.
#![allow(unsafe_code)]

use std::ffi::c_ulong;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use crate::catalog::{Format, HostCamera, classify_v4l2};
use crate::error::{Error, Result};
use crate::tcc::Authorization;

const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x0000_1000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const FRMSIZE_TYPE_DISCRETE: u32 = 1;
const FRMIVAL_TYPE_DISCRETE: u32 = 1;

/// `struct v4l2_capability`.
#[repr(C)]
#[derive(Default)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_fmtdesc`.
#[repr(C)]
#[derive(Default)]
struct FmtDesc {
    index: u32,
    kind: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_frmsizeenum`; the union is the stepwise form's six words (the discrete form is
/// its first two).
#[repr(C)]
#[derive(Default)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    kind: u32,
    size: [u32; 6],
    reserved: [u32; 2],
}

/// `struct v4l2_frmivalenum`; the union is the stepwise form's three fractions.
#[repr(C)]
#[derive(Default)]
struct FrmIvalEnum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    kind: u32,
    interval: [u32; 6],
    reserved: [u32; 2],
}

const _: () = assert!(size_of::<Capability>() == 104);
const _: () = assert!(size_of::<FmtDesc>() == 64);
const _: () = assert!(size_of::<FrmSizeEnum>() == 44);
const _: () = assert!(size_of::<FrmIvalEnum>() == 52);

/// `_IOC(dir, 'V', nr, size)` for the asm-generic layout (x86_64, aarch64).
const fn ioc(dir: c_ulong, nr: c_ulong, size: usize) -> c_ulong {
    dir << 30 | (size as c_ulong) << 16 | (b'V' as c_ulong) << 8 | nr
}
const READ: c_ulong = 2;
const READ_WRITE: c_ulong = 3;
const VIDIOC_QUERYCAP: c_ulong = ioc(READ, 0, size_of::<Capability>());
const VIDIOC_ENUM_FMT: c_ulong = ioc(READ_WRITE, 2, size_of::<FmtDesc>());
const VIDIOC_ENUM_FRAMESIZES: c_ulong = ioc(READ_WRITE, 74, size_of::<FrmSizeEnum>());
const VIDIOC_ENUM_FRAMEINTERVALS: c_ulong = ioc(READ_WRITE, 75, size_of::<FrmIvalEnum>());

/// Issues one ioctl whose argument is `T`; false when the driver refuses it (the end of an
/// enumeration, or an unsupported request).
fn ioctl<T>(file: &File, request: c_ulong, arg: &mut T) -> bool {
    // SAFETY: `request` encodes `size_of::<T>()` and `T` is the `#[repr(C)]` struct the request
    // reads and writes; `arg` is a valid, exclusive pointer for the call and the fd is open.
    unsafe { libc::ioctl(file.as_raw_fd(), request as _, std::ptr::from_mut(arg)) == 0 }
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

pub(crate) fn fourcc(code: u32) -> String {
    code.to_le_bytes().iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' }).collect()
}

pub(crate) fn cameras() -> Result<Vec<HostCamera>> {
    let mut nodes: Vec<(u32, String)> = match std::fs::read_dir("/dev") {
        Ok(dir) => dir
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter_map(|n| Some((n.strip_prefix("video")?.parse().ok()?, n)))
            .collect(),
        Err(_) => return Ok(Vec::new()),
    };
    nodes.sort_unstable();
    let (mut cameras, mut denied) = (Vec::new(), 0usize);
    for (_, node) in &nodes {
        let path = format!("/dev/{node}");
        let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                denied += 1;
                continue;
            }
            Err(_) => continue,
        };
        if let Some(camera) = describe(&file, &path) {
            cameras.push(camera);
        }
    }
    if cameras.is_empty() && denied > 0 {
        return Err(Error::Permission {
            status: Authorization::Denied,
            fix: format!(
                "{denied} /dev/video* node(s) can't be opened by this user: add it to the device's group \
                 (usually `sudo usermod -aG video $USER`, then log in again)"
            ),
        });
    }
    // Two capture nodes on one bus (rare) still get distinct ids.
    for i in 1..cameras.len() {
        let id = cameras[i].id.clone();
        let n = cameras[..i].iter().filter(|c| c.id == id || c.id.starts_with(&format!("{id}#"))).count();
        if n > 0 {
            cameras[i].id = format!("{id}#{n}");
        }
    }
    Ok(cameras)
}

fn describe(file: &File, path: &str) -> Option<HostCamera> {
    let mut cap = Capability::default();
    if !ioctl(file, VIDIOC_QUERYCAP, &mut cap) {
        return None;
    }
    let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 { cap.device_caps } else { cap.capabilities };
    if caps & (CAP_VIDEO_CAPTURE | CAP_VIDEO_CAPTURE_MPLANE) == 0 {
        return None; // a metadata or output node
    }
    let (driver, card, bus) = (text(&cap.driver), text(&cap.card), text(&cap.bus_info));
    let (kind, is_virtual) = classify_v4l2(&driver, &card, &bus);
    Some(HostCamera {
        id: if bus.is_empty() { path.to_string() } else { bus },
        name: if card.is_empty() { path.to_string() } else { card },
        kind,
        is_virtual,
        manufacturer: None,
        model: None,
        transport: Some(driver),
        path: Some(path.to_string()),
        formats: formats(file),
    })
}

fn formats(file: &File) -> Vec<Format> {
    let mut out = Vec::new();
    for index in 0..64 {
        let mut desc = FmtDesc { index, kind: BUF_TYPE_VIDEO_CAPTURE, ..FmtDesc::default() };
        if !ioctl(file, VIDIOC_ENUM_FMT, &mut desc) {
            break;
        }
        for size_index in 0..64 {
            let mut size = FrmSizeEnum { index: size_index, pixel_format: desc.pixelformat, ..FrmSizeEnum::default() };
            if !ioctl(file, VIDIOC_ENUM_FRAMESIZES, &mut size) {
                break;
            }
            // Stepwise and continuous ranges (min, max, step of width, then of height) report
            // their maximum.
            let (width, height) =
                if size.kind == FRMSIZE_TYPE_DISCRETE { (size.size[0], size.size[1]) } else { (size.size[1], size.size[4]) };
            let max_fps = max_fps(file, desc.pixelformat, width, height);
            out.push(Format { width, height, pixel_format: fourcc(desc.pixelformat), max_fps });
            if size.kind != FRMSIZE_TYPE_DISCRETE {
                break;
            }
        }
    }
    out
}

fn max_fps(file: &File, pixel_format: u32, width: u32, height: u32) -> Option<f64> {
    let mut best: Option<f64> = None;
    for index in 0..64 {
        let mut ival = FrmIvalEnum { index, pixel_format, width, height, ..FrmIvalEnum::default() };
        if !ioctl(file, VIDIOC_ENUM_FRAMEINTERVALS, &mut ival) {
            break;
        }
        // Discrete: one fraction (seconds per frame); stepwise: min, max, step: the min is fastest.
        let [num, den, ..] = ival.interval;
        if num > 0 {
            let fps = f64::from(den) / f64::from(num);
            best = Some(best.map_or(fps, |b: f64| b.max(fps)));
        }
        if ival.kind != FRMIVAL_TYPE_DISCRETE {
            break;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
    }

    #[test]
    fn strings_and_fourccs_decode() {
        assert_eq!(text(b"uvcvideo\0\0\0junk"), "uvcvideo");
        assert_eq!(fourcc(u32::from_le_bytes(*b"YUYV")), "YUYV");
        assert_eq!(fourcc(1), "????");
    }
}
