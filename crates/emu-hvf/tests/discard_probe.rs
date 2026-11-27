//! Measures how host memory mapped into a Hypervisor.framework VM can be given back to the host
//! (`phys_footprint`) and what the guest reads afterwards. Needs Apple silicon and the hypervisor
//! entitlement (cargo's runner signs test binaries). One VM per process, so run each test on its
//! own: `cargo test -p simee-emu-hvf --test discard_probe -- --ignored --nocapture --exact <name>` (or
//! `cargo nextest run --run-ignored only`, a process per test).
//!
//! 64 MiB, half written by the host and half by the guest, M-series, macOS 26 (2026-11-24):
//!
//! | discard                                   | footprint after  | guest reads | host reads | shared after |
//! |-------------------------------------------|------------------|-------------|------------|--------------|
//! | `madvise(MADV_DONTNEED)`                  | 66.1 -> 66.1 MiB | old data    | old data   | yes          |
//! | `madvise(MADV_FREE)`                      | 66.1 -> 66.1 MiB | old data    | old data   | yes          |
//! | `madvise(MADV_FREE_REUSABLE)`             | 66.1 -> 34.1 MiB | old data    | old data   | yes          |
//! | `mmap(MAP_FIXED)` alone                   | 66.1 -> 34.1 MiB | old data    | zeroes     | **no**       |
//! | `hv_vm_unmap` + `mmap(MAP_FIXED)` + `hv_vm_map` | 66.1 -> 2.0 MiB | zeroes | zeroes     | yes          |
//!
//! The last is `GuestRam::discard`: about 30-45 us per MiB, 0.6-0.8 ms for 32 MiB. Caveat: a
//! page both sides touched counts twice in `phys_footprint` (66 -> 98 MiB when the host reads the
//! guest-written half, with no discard at all).
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
#![allow(clippy::undocumented_unsafe_blocks, clippy::unwrap_used)]

use emu_hvf::{Exit, GuestRam, Reg, SysReg, Vcpu, Vm};

const DATA: u64 = 0x4000_0000;
const LEN: u64 = 64 << 20;
const CODE: u64 = 0x8000_0000;

fn footprint() -> u64 {
    emu_hvf::phys_footprint().unwrap()
}

fn mib(b: u64) -> f64 {
    b as f64 / (1 << 20) as f64
}

struct Guest {
    vcpu: Vcpu,
    /// Keeps the code page mapped while the vCPU runs it.
    _code: GuestRam,
}

const HVC: u32 = 0xd400_0002;

impl Guest {
    /// A vCPU with the MMU on: an identity map of 1 GiB write-back blocks (with the MMU off the
    /// guest's data accesses are Device memory and bypass the cache the host wrote through).
    fn new(code: GuestRam) -> Guest {
        let vcpu = Vcpu::new().unwrap();
        let table = CODE + 0x1000;
        for gb in [DATA >> 30, CODE >> 30] {
            // Block descriptor: AF, inner shareable, AttrIndx 0, valid block.
            code.write(table + gb * 8, &((gb << 30) | 1 << 10 | 3 << 8 | 0b01).to_le_bytes()).unwrap();
        }
        vcpu.set_sys_reg(SysReg(0xc510), 0xff).unwrap(); // MAIR_EL1: attr0 normal WBWA
        vcpu.set_sys_reg(SysReg(0xc102), 25 | 1 << 8 | 1 << 10 | 3 << 12 | 1 << 23 | 2 << 32).unwrap(); // TCR_EL1
        vcpu.set_sys_reg(SysReg(0xc100), table).unwrap(); // TTBR0_EL1
        vcpu.set_sys_reg(SysReg::SCTLR_EL1, 0x30d0_0800 | 1 | 1 << 2 | 1 << 12).unwrap(); // M, C, I
        // Every program is written before the guest first runs: code written later can meet
        // stale lines the instruction cache (or its prefetcher) fetched already, and the guest
        // then executes zeroes and loops in its own exception vector.
        for (at, prog) in [(LOAD, &LOAD_PROG[..]), (STORE, &STORE_PROG[..]), (TOUCH, &TOUCH_PROG[..])] {
            let bytes: Vec<u8> = prog.iter().flat_map(|i| i.to_le_bytes()).collect();
            code.write(CODE + at, &bytes).unwrap();
        }
        Guest { vcpu, _code: code }
    }
    /// Runs the program at `CODE + at` until its `hvc`.
    fn run(&self, at: u64, x: &[(u32, u64)]) -> u64 {
        self.vcpu.set_reg(Reg::PC, CODE + at).unwrap();
        self.vcpu.set_reg(Reg::CPSR, 0x3c5).unwrap();
        for &(r, v) in x {
            self.vcpu.set_reg(Reg::x(r), v).unwrap();
        }
        let t = std::time::Instant::now();
        loop {
            match self.vcpu.run().unwrap() {
                // The watchdog kicks every 250 ms; a short program that hasn't reached its hvc
                // after 2 s never will.
                Exit::Canceled if t.elapsed() > std::time::Duration::from_secs(2) => {
                    println!("  guest program at {:#x} (x0 {:#x}) HUNG: never reached hvc", CODE + at, x[0].1);
                    std::process::exit(2);
                }
                Exit::Canceled => {}
                Exit::Exception { syndrome, ipa, .. } => {
                    assert_eq!(syndrome >> 26, 0x16, "expected hvc, got esr {syndrome:#x} ipa {ipa:#x}");
                    return self.vcpu.reg(Reg::x(1)).unwrap();
                }
                Exit::VtimerActivated => self.vcpu.set_vtimer_mask(false).unwrap(),
                e => panic!("{e:?}"),
            }
        }
    }
    fn load(&self, ipa: u64) -> u64 {
        self.run(LOAD, &[(0, ipa)])
    }
    fn store(&self, ipa: u64, v: u64) {
        self.run(STORE, &[(0, ipa), (2, v)]);
    }
    /// Stores `v + i` at the start of the `i`th 4 KiB page of `[start, end)`.
    fn touch(&self, start: u64, end: u64, v: u64) {
        self.run(TOUCH, &[(0, start), (2, v), (3, end), (4, PAGE)]);
    }
}

/// ldr x1, [x0]; hvc #0
const LOAD: u64 = 0;
const LOAD_PROG: [u32; 2] = [0xf940_0001, HVC];
/// str x2, [x0]; hvc #0
const STORE: u64 = 0x100;
const STORE_PROG: [u32; 2] = [0xf900_0002, HVC];
/// loop: str x2,[x0]; add x0,x0,x4; add x2,x2,#1; cmp x0,x3; b.ne loop; hvc #0
const TOUCH: u64 = 0x200;
const TOUCH_PROG: [u32; 6] = [0xf900_0002, 0x8b04_0000, 0x9100_0442, 0xeb03_001f, 0x54ff_ff81, HVC];

const PAGE: u64 = 4096;
const HOST_TAG: u64 = 0xab00_0000_0000;
const GUEST_TAG: u64 = 0xcd00_0000_0000;

fn host_word(ram: &GuestRam, ipa: u64) -> u64 {
    ram.read_u64(ipa).unwrap()
}

fn remap(ptr: *mut u8, len: usize) {
    let p = unsafe {
        libc::mmap(ptr.cast(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED, -1, 0)
    };
    assert_eq!(p.cast::<u8>(), ptr);
}

/// What a page reads after the discard: its own tag, zero, or something else.
#[derive(Default, Debug)]
struct Tally {
    own: usize,
    zero: usize,
    other: usize,
}

impl Tally {
    fn add(&mut self, got: u64, want: u64) {
        match got {
            g if g == want => self.own += 1,
            0 => self.zero += 1,
            _ => self.other += 1,
        }
    }
}

/// A VM with `LEN` bytes of data RAM at `DATA` and a vCPU to read and write it, kicked every
/// 250 ms so a guest access that never completes is reported instead of hanging the test.
fn setup() -> (Vm, GuestRam, Guest) {
    let vm = Vm::new().unwrap();
    let ram = GuestRam::new(DATA, LEN).unwrap();
    vm.map_ram(&ram).unwrap();
    let code = GuestRam::new(CODE, 0x4000).unwrap();
    vm.map_ram(&code).unwrap();
    let g = Guest::new(code);
    let kick = g.vcpu.handle();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            kick.kick();
        }
    });
    (vm, ram, g)
}

fn probe(name: &str, discard: impl FnOnce(&Vm, &GuestRam)) {
    let (vm, ram, g) = setup();
    let half = DATA + LEN / 2;
    let pages = LEN / PAGE;
    let tag = |i: u64| if i < pages / 2 { HOST_TAG + i } else { GUEST_TAG + (i - pages / 2) };

    let empty = footprint();
    // First half written by the host (every byte), second half by the guest (a word per page).
    let host = ram.host_range(DATA, LEN as usize).unwrap();
    unsafe { std::ptr::write_bytes(host.as_ptr(), 0xab, (LEN / 2) as usize) };
    for i in 0..pages / 2 {
        ram.write(DATA + i * PAGE, &tag(i).to_le_bytes()).unwrap();
    }
    g.touch(half, DATA + LEN, GUEST_TAG);
    for i in [0, pages / 2 - 1, pages / 2, pages - 1] {
        assert_eq!(g.load(DATA + i * PAGE), tag(i), "guest page {i} before");
        assert_eq!(host_word(&ram, DATA + i * PAGE), tag(i), "host page {i} before");
    }
    let before = footprint();
    let t = std::time::Instant::now();
    discard(&vm, &ram);
    let took = t.elapsed();
    let after = footprint();
    let (mut hv, mut gv) = (Tally::default(), Tally::default());
    for i in 0..pages {
        if i % 64 == 0 {
            gv.add(g.load(DATA + i * PAGE), tag(i));
        }
    }
    let after_guest_reads = footprint();
    for i in 0..pages {
        hv.add(host_word(&ram, DATA + i * PAGE), tag(i));
    }
    let after_host_reads = footprint();
    println!(
        "{name}: footprint empty {:.1}, touched {:.1}, after discard {:.1} MiB ({took:?}), after guest reads {:.1}, after host reads {:.1}; \
         guest view {gv:?}; host view {hv:?}",
        mib(empty),
        mib(before),
        mib(after),
        mib(after_guest_reads),
        mib(after_host_reads),
    );
    // Afterwards: a guest store must reach the host and a host store the guest.
    g.store(DATA + 0x10000, 0x1111);
    let guest_to_host = host_word(&ram, DATA + 0x10000) == 0x1111;
    ram.write(half + 0x10000, &0x2222u64.to_le_bytes()).unwrap();
    let host_to_guest = g.load(half + 0x10000) == 0x2222;
    // And the guest can write every page again.
    g.touch(DATA, DATA + LEN, 7);
    println!(
        "  guest store seen by host {guest_to_host}, host store seen by guest {host_to_guest}, guest rewrote all pages, footprint {:.1} MiB",
        mib(footprint())
    );
}

fn madvise(ram: &GuestRam, advice: libc::c_int) {
    let host = ram.host_range(ram.base(), ram.len() as usize).unwrap();
    assert_eq!(unsafe { libc::madvise(host.as_ptr().cast(), host.len(), advice) }, 0);
}

fn remap_all(ram: &GuestRam) {
    let host = ram.host_range(ram.base(), ram.len() as usize).unwrap();
    remap(host.as_ptr(), host.len());
}

#[test]
#[ignore = "needs Apple silicon and the hypervisor entitlement"]
fn probe_madv_dontneed() {
    probe("madvise(MADV_DONTNEED)", |_, ram| madvise(ram, libc::MADV_DONTNEED));
}

#[test]
#[ignore = "needs Apple silicon and the hypervisor entitlement"]
fn probe_madv_free_reusable() {
    probe("madvise(MADV_FREE_REUSABLE)", |_, ram| madvise(ram, libc::MADV_FREE_REUSABLE));
}

#[test]
#[ignore = "needs Apple silicon and the hypervisor entitlement"]
fn probe_madv_free() {
    probe("madvise(MADV_FREE)", |_, ram| madvise(ram, libc::MADV_FREE));
}

#[test]
#[ignore = "needs Apple silicon and the hypervisor entitlement"]
fn probe_mmap_fixed_only() {
    probe("mmap(MAP_FIXED) while hv-mapped", |_, ram| remap_all(ram));
}

#[test]
#[ignore = "needs Apple silicon and the hypervisor entitlement"]
fn probe_unmap_remap() {
    probe("hv_vm_unmap + mmap(MAP_FIXED) + hv_vm_map", |vm, ram| {
        Vm::unmap_raw(ram.base(), ram.len() as usize).unwrap();
        remap_all(ram);
        vm.map_ram(ram).unwrap();
    });
}

/// Baseline, no discard at all: the guest reads never-touched pages, then stores to them.
#[test]
#[ignore = "needs Apple silicon and the hypervisor entitlement"]
fn probe_guest_read_then_store_fresh_pages() {
    let (_vm, ram, g) = setup();
    for i in 0..64u64 {
        let a = DATA + i * (1 << 20);
        assert_eq!(g.load(a), 0);
        g.store(a, i + 1);
        assert_eq!(host_word(&ram, a), i + 1);
    }
    println!("fresh pages: 64 read-then-store pairs completed");
}

#[test]
#[ignore = "needs Apple silicon and the hypervisor entitlement"]
fn probe_madv_free_reusable_then_guest_store() {
    // Host-written pages the guest has only read, MADV_FREE_REUSABLE, then a guest store.
    let (_vm, ram, g) = setup();
    ram.write(DATA, &vec![0xab; LEN as usize]).unwrap();
    assert_eq!(g.load(DATA), 0xabab_abab_abab_abab); // the guest reads, never writes, before
    madvise(&ram, libc::MADV_FREE_REUSABLE);
    g.store(DATA + 0x10000, 0x1111);
    println!("MADV_FREE_REUSABLE then guest store: completed, host sees {:#x}", host_word(&ram, DATA + 0x10000));
}

/// The shipped primitive: `GuestRam::discard` on part of VM-mapped RAM releases the pages, both
/// views read zeroes, the rest is untouched and host and guest still share the memory.
#[test]
#[ignore = "needs Apple silicon and the hypervisor entitlement"]
fn guest_ram_discard_in_a_vm() {
    let (_vm, ram, g) = setup();
    let pages = LEN / PAGE;
    let tag = |i: u64| HOST_TAG + i;
    for i in 0..pages {
        ram.write(DATA + i * PAGE, &tag(i).to_le_bytes()).unwrap();
    }
    g.touch(DATA, DATA + LEN, GUEST_TAG); // the guest has every page mapped at stage 2 too
    for i in 0..pages {
        ram.write(DATA + i * PAGE, &tag(i).to_le_bytes()).unwrap();
    }
    let touched = footprint();

    // A 1 MiB run starting 8 KiB into a granule: the partial granule at each end is kept.
    let t = std::time::Instant::now();
    let n = ram.discard(DATA + (16 << 20) + 0x2000, 1 << 20).unwrap();
    let one_mib = t.elapsed();
    assert_eq!(n, (1 << 20) - 0x4000);
    let (lo, hi) = (DATA + (16 << 20) + 0x4000, DATA + (17 << 20));
    for i in 0..pages {
        let a = DATA + i * PAGE;
        let want = if (lo..hi).contains(&a) { 0 } else { tag(i) };
        assert_eq!(g.load(a), want, "guest page {i}");
        assert_eq!(host_word(&ram, a), want, "host page {i}");
    }
    // Then 32 MiB in one call.
    let t = std::time::Instant::now();
    assert_eq!(ram.discard(DATA + (32 << 20), 32 << 20).unwrap(), 32 << 20);
    let big = t.elapsed();
    let after = footprint();
    assert_eq!(g.load(DATA + (40 << 20)), 0);
    assert_eq!(g.load(DATA + (31 << 20)), tag((31 << 20) / PAGE));

    // Shared again: each side sees the other's stores to discarded pages.
    g.store(DATA + (48 << 20), 0x1111);
    assert_eq!(host_word(&ram, DATA + (48 << 20)), 0x1111);
    ram.write(DATA + (50 << 20), &0x2222u64.to_le_bytes()).unwrap();
    assert_eq!(g.load(DATA + (50 << 20)), 0x2222);
    let released = touched.saturating_sub(after);
    println!(
        "GuestRam::discard: 1 MiB in {one_mib:?}, 32 MiB in {big:?}; footprint {:.1} -> {:.1} MiB ({:.1} MiB released)",
        mib(touched),
        mib(after),
        mib(released)
    );
    assert!(released >= 32 << 20, "released only {released} bytes");
}
