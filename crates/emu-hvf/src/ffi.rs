//! Raw Hypervisor.framework declarations (arm64). Signatures follow the macOS SDK headers.
use std::ffi::c_void;

/// `hv_vcpu_exit_t`.
#[repr(C)]
pub struct VcpuExit {
    pub reason: u32,
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

#[repr(C)]
pub struct MachTimebaseInfo {
    pub numer: u32,
    pub denom: u32,
}

/// `HV_GIC_REDISTRIBUTOR_REG_GICR_ISPENDR0`.
pub const GICR_ISPENDR0: u32 = 0x10200;
/// `HV_GIC_DISTRIBUTOR_REG_GICD_IROUTER32`; IROUTER<n> follows at 8-byte steps (register offsets).
pub const GICD_IROUTER32: u32 = 0x6100;

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    pub fn hv_vm_create(config: *mut c_void) -> i32;
    pub fn hv_vm_config_create() -> *mut c_void;
    pub fn hv_vm_config_get_max_ipa_size(bits: *mut u32) -> i32;
    pub fn hv_vm_config_set_ipa_size(config: *mut c_void, bits: u32) -> i32;
    pub fn hv_vm_config_set_ipa_granule(config: *mut c_void, granule: u32) -> i32;
    pub fn hv_vm_unmap(ipa: u64, size: usize) -> i32;
    pub fn hv_vm_destroy() -> i32;
    pub fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> i32;

    pub fn hv_vcpu_create(vcpu: *mut u64, exit: *mut *const VcpuExit, config: *mut c_void) -> i32;
    pub fn hv_vcpu_destroy(vcpu: u64) -> i32;
    pub fn hv_vcpu_run(vcpu: u64) -> i32;
    pub fn hv_vcpus_exit(vcpus: *const u64, count: u32) -> i32;
    pub fn hv_vcpu_get_reg(vcpu: u64, reg: u32, value: *mut u64) -> i32;
    pub fn hv_vcpu_set_reg(vcpu: u64, reg: u32, value: u64) -> i32;
    pub fn hv_vcpu_get_sys_reg(vcpu: u64, reg: u16, value: *mut u64) -> i32;
    pub fn hv_vcpu_set_sys_reg(vcpu: u64, reg: u16, value: u64) -> i32;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: u64, masked: bool) -> i32;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: u64, offset: u64) -> i32;

    pub fn hv_gic_config_create() -> *mut c_void;
    pub fn hv_gic_config_set_distributor_base(config: *mut c_void, addr: u64) -> i32;
    pub fn hv_gic_config_set_redistributor_base(config: *mut c_void, addr: u64) -> i32;
    pub fn hv_gic_create(config: *mut c_void) -> i32;
    pub fn hv_gic_get_distributor_size(size: *mut usize) -> i32;
    pub fn hv_gic_get_redistributor_region_size(size: *mut usize) -> i32;
    pub fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> i32;
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> i32;
    pub fn hv_gic_get_distributor_reg(reg: u32, value: *mut u64) -> i32;
    pub fn hv_gic_set_redistributor_reg(vcpu: u64, reg: u32, value: u64) -> i32;
}

unsafe extern "C" {
    pub fn mach_absolute_time() -> u64;
    pub fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}
