//! Tests against a real `/dev/kvm` on an x86_64 host (CI's `kvm` job; GitHub's x86_64 runners
//! expose KVM, its arm64 runners don't). They drive tiny real-mode guests through the
//! architecture-neutral half of the crate: opening KVM, memory slots, exits, ioeventfd, irqfd
//! and kicks. Run with `cargo test -p simee-emu-kvm -- --ignored --nocapture` where `/dev/kvm` is
//! accessible.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use emu_hvf::GuestRam;

use crate::{Exit, Kicks, Vcpu, Vm};

const CODE: u64 = 0x1000;
/// A guest physical address with no memory slot: stores there are MMIO.
const DOORBELL: u64 = 0xd000;

fn vm_with(code: &[u8]) -> (Vm, Vcpu, Arc<GuestRam>) {
    let kvm = crate::open().expect("open /dev/kvm (run where it is accessible)");
    let vm = kvm.create_vm(0).expect("vm");
    let ram = Arc::new(GuestRam::new(0, 0x4000).expect("ram"));
    ram.write(CODE, code).expect("code");
    vm.map_ram(&ram).expect("map");
    let vcpu = vm.create_vcpu(0).expect("vcpu");
    let mut sregs = vcpu.fd.get_sregs().expect("sregs");
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.fd.set_sregs(&sregs).expect("set sregs");
    let regs = kvm_bindings::kvm_regs { rip: CODE, rflags: 2, ..Default::default() };
    vcpu.fd.set_regs(&regs).expect("set regs");
    (vm, vcpu, ram)
}

/// `mov eax, 1; mov ecx, n; l: mov [DOORBELL], eax; dec ecx; jnz l; hlt` in real mode.
fn doorbell_loop(n: u32) -> Vec<u8> {
    let mut c = vec![0x66, 0xb8, 1, 0, 0, 0, 0x66, 0xb9];
    c.extend_from_slice(&n.to_le_bytes());
    c.extend_from_slice(&[0x66, 0xa3]);
    c.extend_from_slice(&(DOORBELL as u16).to_le_bytes());
    c.extend_from_slice(&[0x66, 0x49, 0x75, 0xf8, 0xf4]);
    c
}

/// Runs until HLT, answering MMIO; returns how many MMIO stores reached userspace.
fn run_to_halt(vcpu: &mut Vcpu) -> u64 {
    let mut stores = 0;
    loop {
        match vcpu.run().expect("run") {
            Exit::Hlt => return stores,
            Exit::MmioWrite { addr, data } => {
                assert_eq!((addr, data), (DOORBELL, &[1u8, 0, 0, 0][..]));
                stores += 1;
            }
            Exit::Interrupted => {}
            other => panic!("unexpected exit {other:?}"),
        }
    }
}

#[test]
#[ignore = "needs /dev/kvm (x86_64)"]
fn a_guest_runs_and_its_port_and_mmio_accesses_reach_the_vmm() {
    // mov dx, 0x3f8; mov al, 'K'; out dx, al; mov [DOORBELL], al (byte store); hlt
    let code = [0xba, 0xf8, 0x03, 0xb0, b'K', 0xee, 0xa2, 0x00, 0xd0, 0xf4];
    let (_vm, mut vcpu, _ram) = vm_with(&code);
    match vcpu.run().expect("run") {
        Exit::IoOut { port, data } => assert_eq!((port, data), (0x3f8, &b"K"[..])),
        other => panic!("{other:?}"),
    }
    match vcpu.run().expect("run") {
        Exit::MmioWrite { addr, data } => assert_eq!((addr, data), (DOORBELL, &b"K"[..])),
        other => panic!("{other:?}"),
    }
    assert!(matches!(vcpu.run().expect("run"), Exit::Hlt));
}

#[test]
#[ignore = "needs /dev/kvm (x86_64)"]
fn ioeventfd_stores_never_leave_the_kernel_and_other_values_still_exit() {
    const N: u32 = 1000;
    let (vm, mut vcpu, _ram) = vm_with(&doorbell_loop(N));
    // The guest stores 1: a notifier for 2 doesn't match, so every store exits.
    let other = vm.ioeventfd(DOORBELL, Some(2)).expect("ioeventfd");
    assert_eq!(run_to_halt(&mut vcpu), u64::from(N));
    assert!(!other.drain(), "a non-matching store doesn't signal");

    let (vm, mut vcpu, _ram) = vm_with(&doorbell_loop(N));
    let bell = vm.ioeventfd(DOORBELL, Some(1)).expect("ioeventfd");
    assert_eq!(run_to_halt(&mut vcpu), 0, "no store reached userspace");
    assert!(bell.drain(), "signalled");
    assert!(!bell.drain(), "drained in one read (an eventfd counts)");
}

/// What a virtio QueueNotify costs the vCPU: a store that exits to userspace (emu's path on
/// Hypervisor.framework) against one ioeventfd completes in the kernel.
/// `cargo test --release -p simee-emu-kvm notify_cost -- --ignored --nocapture`
#[test]
#[ignore = "needs /dev/kvm (x86_64); benchmark"]
fn notify_cost() {
    const N: u32 = 200_000;
    let per = |ioeventfd: bool| -> f64 {
        let mut best = f64::MAX;
        for _ in 0..3 {
            let (vm, mut vcpu, _ram) = vm_with(&doorbell_loop(N));
            let _bell = ioeventfd.then(|| vm.ioeventfd(DOORBELL, Some(1)).expect("ioeventfd"));
            let t = Instant::now();
            let exits = run_to_halt(&mut vcpu);
            let ns = t.elapsed().as_nanos() as f64 / f64::from(N);
            assert_eq!(exits, if ioeventfd { 0 } else { u64::from(N) });
            best = best.min(ns);
        }
        best
    };
    let user = per(false);
    let kernel = per(true);
    println!("notify_cost: MMIO exit to userspace {user:.0} ns per store; ioeventfd {kernel:.0} ns per store ({:.1}x)", user / kernel);
}

#[test]
#[ignore = "needs /dev/kvm (x86_64)"]
fn memory_slots_map_unmap_and_refuse_overlaps() {
    let kvm = crate::open().expect("open");
    let vm = kvm.create_vm(0).expect("vm");
    let ram = GuestRam::new(0x10_0000, 0x10_0000).expect("ram");
    vm.map_ram(&ram).expect("map");
    let other = GuestRam::new(0x18_0000, 0x10_0000).expect("ram");
    assert!(vm.map_ram(&other).is_err(), "overlap");
    let blob = ram.host_range(0x10_0000, 0x1000).expect("range");
    assert!(vm.map_raw(blob, 0x40_0000).is_ok(), "the same host memory may appear twice");
    vm.unmap_raw(0x40_0000, 0x1000).expect("unmap");
    assert!(vm.unmap_raw(0x40_0000, 0x1000).is_err(), "already unmapped");
}

#[test]
#[ignore = "needs /dev/kvm (x86_64)"]
fn irqfds_register_on_an_in_kernel_irqchip() {
    let kvm = crate::open().expect("open");
    let vm = kvm.create_vm(0).expect("vm");
    assert!(vm.irqfd(5).is_err(), "no irqchip yet");
    vm.fd.create_irq_chip().expect("irqchip");
    let line = vm.irqfd(5).expect("irqfd");
    line.signal().expect("signal");
    vm.set_irq_line(6, true).expect("irq line");
    vm.set_irq_line(6, false).expect("irq line");
}

/// A vCPU idling in the guest (a `jmp $` spin, as close to KVM_RUN never returning as a test
/// guest gets) is forced out by a kick.
#[test]
#[ignore = "needs /dev/kvm (x86_64)"]
fn a_kick_forces_a_running_vcpu_out() {
    let (_vm, mut vcpu, _ram) = vm_with(&[0xeb, 0xfe]);
    let kicks = Arc::new(Kicks::new(1).expect("kicks"));
    let stop = Arc::new(AtomicBool::new(false));
    let t = std::thread::spawn({
        let (kicks, stop) = (kicks.clone(), stop.clone());
        move || {
            let _running = kicks.register(0);
            let mut runs = 0;
            while !stop.load(Ordering::SeqCst) {
                runs += 1;
                assert!(matches!(vcpu.run().expect("run"), Exit::Interrupted));
            }
            runs
        }
    });
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::SeqCst);
    let start = Instant::now();
    while !t.is_finished() {
        assert!(start.elapsed() < Duration::from_secs(5), "the vCPU never left the guest");
        kicks.kick_running();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(t.join().expect("vcpu thread") >= 1);
}
