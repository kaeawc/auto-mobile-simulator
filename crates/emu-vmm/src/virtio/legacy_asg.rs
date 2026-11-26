//! Address-space graphics (ASG) as API 31–33 guests set it up, served on the renderer's current
//! protocol.
//!
//! Every graphics stream of a `virtio-gpu-asg` guest (each process's GLES, Vulkan and
//! renderControl connection) is an ASG ring shared with a host render thread. API 34 and newer
//! guests make the ring themselves: a host-memory blob (`RESOURCE_CREATE_BLOB`, blob id 0), then
//! `GFXSTREAM_CONTEXT_CREATE {resource}` and `GFXSTREAM_CONTEXT_PING {resource}` on the context.
//! API 31–33 guests (goldfish-opengl's `virtgpu_address_space_*`, Android 12 and 13) speak the
//! protocol before it, with the same opcodes:
//!
//! 1. a 4096-byte guest-backed response page (`RESOURCE_CREATE_3D`, `PIPE_BUFFER`,
//!    `VIRGL_BIND_CUSTOM`), then `0x1001 {subdevice type}`, "create with subdevice";
//! 2. `0x1003 {response resource, ping}`, "ping with response", for `ASG_GET_RING`,
//!    `ASG_GET_BUFFER` and `ASG_SET_VERSION`, whose answers (the ring and buffer sizes, and the id
//!    of the host memory to map) the host writes into the response page;
//! 3. a blob of ring plus buffer size with that id (`RESOURCE_CREATE_BLOB`, blob id = the host
//!    memory id), mapped, and one more `ASG_GET_CONFIG`;
//! 4. `0x1002 {ping}` with `ASG_NOTIFY_AVAILABLE` to wake the render thread.
//!
//! The renderer (gfxstream as emulator 37.2 ships it) serves only the new protocol: it reads the
//! old `0x1001 {10}` as `GFXSTREAM_CONTEXT_CREATE` of resource 10 and fails ("failed to create ASG
//! instance on context 1: failed to map resource 10"), so the guest's answers stay zero, it asks
//! for a host memory blob of size 0, and every graphics client aborts in `HostConnection::connect`.
//!
//! [`LegacyAsg`] translates: it answers the pings itself with what the new protocol fixes (the
//! ring storage of three pages, a 1 MiB buffer after it, version 1), hands the guest a host memory
//! id of its own, and when the guest creates the blob with that id, has the device create it as
//! the new protocol's ring blob (blob id 0) and send `GFXSTREAM_CONTEXT_CREATE` for it; later
//! notifies become `GFXSTREAM_CONTEXT_PING` of that blob. The guest's layout is the new one: ring
//! storage at the start of the blob and the buffer after it. Pure logic; the device
//! (`virtio/gpu.rs`) carries out what it says.
use std::collections::HashMap;

/// `kVirtioGpuAddressSpaceContextCreateWithSubdevice`, and the new `GFXSTREAM_CONTEXT_CREATE`.
pub const OP_CREATE: u32 = 0x1001;
/// `kVirtioGpuAddressSpacePing`, and the new `GFXSTREAM_CONTEXT_PING`.
pub const OP_PING: u32 = 0x1002;
/// `kVirtioGpuAddressSpacePingWithResponse`.
pub const OP_PING_WITH_RESPONSE: u32 = 0x1003;

/// `GoldfishAddressSpaceSubdeviceType::VirtioGpuGraphics`.
const SUBDEVICE_VIRTIO_GPU_GRAPHICS: u32 = 10;

/// `sizeof(struct asg_ring_storage)`: three 4 KiB rings, at the start of the blob.
pub const RING_STORAGE: u64 = 3 * 4096;
/// The per-context buffer after the rings; the size API 34 and newer guests give the blob too.
pub const BUFFER: u64 = 1 << 20;
/// The ASG protocol version the host agrees to (the newer guests' fixed version).
const VERSION: u32 = 1;

/// `asg_command`, a ping's `metadata`.
const ASG_GET_RING: u64 = 0;
const ASG_GET_BUFFER: u64 = 1;
const ASG_SET_VERSION: u64 = 2;
const ASG_NOTIFY_AVAILABLE: u64 = 3;

/// The host memory ids handed to the guest: this tag over the context id, so one can't be taken
/// for a renderer's id (those count up from 1).
const HOSTMEM_TAG: u64 = 0x5349_4d45_0000_0000;

/// What the device does with a context's command stream.
#[derive(Debug, PartialEq, Eq)]
pub enum Submit {
    /// Not the old protocol: the renderer gets it as it is.
    Pass,
    /// Taken care of: the renderer gets nothing.
    Done,
    /// Write `words` (little-endian) at the start of resource `resource`'s guest backing, the
    /// response page; the renderer gets nothing.
    Respond { resource: u32, words: [u32; 10] },
    /// Wake the render thread of ring blob `resource`: the renderer gets
    /// `GFXSTREAM_CONTEXT_PING {resource}` instead.
    Ping { resource: u32 },
}

#[derive(Debug, Default)]
struct Context {
    /// The ring blob once the guest has made it.
    ring: Option<u32>,
}

/// The contexts that set up ASG the old way ([module docs](self)).
#[derive(Debug, Default)]
pub struct LegacyAsg {
    contexts: HashMap<u32, Context>,
}

fn word(cmd: &[u8], i: usize) -> u32 {
    cmd.get(i * 4..i * 4 + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_of(lo: u32, hi: u32) -> u64 {
    u64::from(lo) | (u64::from(hi) << 32)
}

/// The host memory id context `ctx`'s guest maps its ring with.
pub fn hostmem_id(ctx: u32) -> u64 {
    HOSTMEM_TAG | u64::from(ctx)
}

impl LegacyAsg {
    /// What to do with command stream `cmd` that context `ctx` submitted. `ctx_blob(id)` says
    /// whether resource `id` is a blob this context created: the new protocol's create names
    /// one; the old one names the subdevice type, before the guest has made any blob.
    pub fn submit(&mut self, ctx: u32, cmd: &[u8], ctx_blob: impl Fn(u32) -> bool) -> Submit {
        // The new protocol's create and ping are 8 bytes; the old pings 44 and 48.
        match (word(cmd, 0), cmd.len()) {
            (OP_CREATE, 8) => {
                let arg = word(cmd, 1);
                if arg != SUBDEVICE_VIRTIO_GPU_GRAPHICS || ctx_blob(arg) {
                    return Submit::Pass;
                }
                self.contexts.insert(ctx, Context::default());
                Submit::Done
            }
            (OP_PING, 44) if self.contexts.contains_key(&ctx) => {
                let metadata = u64_of(word(cmd, 5), word(cmd, 6));
                self.notify(ctx, metadata)
            }
            (OP_PING_WITH_RESPONSE, 48) if self.contexts.contains_key(&ctx) => {
                let resource = word(cmd, 1);
                let mut words = [0u32; 10];
                for (i, w) in words.iter_mut().enumerate() {
                    *w = word(cmd, 2 + i);
                }
                let (size, metadata) = (u64_of(words[2], words[3]), u64_of(words[4], words[5]));
                let (size, metadata) = match metadata {
                    ASG_GET_RING => (RING_STORAGE, 0),
                    ASG_GET_BUFFER => (BUFFER, RING_STORAGE),
                    ASG_SET_VERSION => (size.min(u64::from(VERSION)), hostmem_id(ctx)),
                    // ASG_GET_CONFIG restores the ring's config, which the renderer wrote when it
                    // made the ring and nothing has changed since; the rest answer 0 too.
                    _ => (size, 0),
                };
                words[2..4].copy_from_slice(&[size as u32, (size >> 32) as u32]);
                words[4..6].copy_from_slice(&[metadata as u32, (metadata >> 32) as u32]);
                Submit::Respond { resource, words }
            }
            _ => Submit::Pass,
        }
    }

    fn notify(&self, ctx: u32, metadata: u64) -> Submit {
        match self.contexts.get(&ctx).and_then(|c| c.ring) {
            Some(resource) if metadata == ASG_NOTIFY_AVAILABLE => Submit::Ping { resource },
            _ => Submit::Done,
        }
    }

    /// Whether the blob context `ctx` creates with `blob_id` is its ring: the device then
    /// creates it as a ring blob (blob id 0) and sends `GFXSTREAM_CONTEXT_CREATE` for it.
    pub fn is_ring(&self, ctx: u32, blob_id: u64) -> bool {
        blob_id == hostmem_id(ctx) && self.contexts.get(&ctx).is_some_and(|c| c.ring.is_none())
    }

    /// The renderer made context `ctx`'s ring as resource `resource`.
    pub fn ring_created(&mut self, ctx: u32, resource: u32) {
        if let Some(c) = self.contexts.get_mut(&ctx) {
            c.ring = Some(resource);
        }
    }

    /// The guest unreferenced resource `id`: a ring goes with it.
    pub fn unref(&mut self, id: u32) {
        for c in self.contexts.values_mut() {
            if c.ring == Some(id) {
                c.ring = None;
            }
        }
    }

    /// The guest destroyed context `ctx`.
    pub fn destroy(&mut self, ctx: u32) {
        self.contexts.remove(&ctx);
    }

    /// A reset: no context is left.
    pub fn clear(&mut self) {
        self.contexts.clear();
    }

    /// Whether any context set up ASG the old way.
    pub fn is_empty(&self) -> bool {
        self.contexts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// `virtgpu_address_space_ping_with_response`'s words for `metadata` (Android 13).
    fn ping_with_response(resp: u32, size: u64, metadata: u64) -> Vec<u8> {
        cmd(&[OP_PING_WITH_RESPONSE, resp, 0, 0, size as u32, (size >> 32) as u32, metadata as u32, (metadata >> 32) as u32, 0, 0, 0, 0])
    }

    fn ping(metadata: u64) -> Vec<u8> {
        cmd(&[OP_PING, 0, 0, 0, 0, metadata as u32, (metadata >> 32) as u32, 0, 0, 0, 0])
    }

    fn answer(s: Submit) -> (u32, u64, u64) {
        match s {
            Submit::Respond { resource, words } => (resource, u64_of(words[2], words[3]), u64_of(words[4], words[5])),
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn an_android_13_guest_gets_its_ring_on_the_new_protocol() {
        let mut asg = LegacyAsg::default();
        let no_blobs = |_| false;
        // Create with subdevice, after the response page (resource 9).
        assert_eq!(asg.submit(1, &cmd(&[OP_CREATE, SUBDEVICE_VIRTIO_GPU_GRAPHICS]), no_blobs), Submit::Done);
        assert_eq!(answer(asg.submit(1, &ping_with_response(9, 0, ASG_GET_RING), no_blobs)), (9, RING_STORAGE, 0));
        assert_eq!(answer(asg.submit(1, &ping_with_response(9, 0, ASG_GET_BUFFER), no_blobs)), (9, BUFFER, RING_STORAGE));
        let (_, version, id) = answer(asg.submit(1, &ping_with_response(9, 1, ASG_SET_VERSION), no_blobs));
        assert_eq!((version, id), (1, hostmem_id(1)));
        // A notify before the ring exists reaches no one.
        assert_eq!(asg.submit(1, &ping(ASG_NOTIFY_AVAILABLE), no_blobs), Submit::Done);
        // The blob with that id is the ring; another blob of the context isn't.
        assert!(!asg.is_ring(1, 77) && !asg.is_ring(2, id));
        assert!(asg.is_ring(1, id));
        asg.ring_created(1, 12);
        assert!(!asg.is_ring(1, id), "one ring per context");
        assert_eq!(answer(asg.submit(1, &ping_with_response(9, 0, 4), no_blobs)).2, 0, "ASG_GET_CONFIG");
        assert_eq!(asg.submit(1, &ping(ASG_NOTIFY_AVAILABLE), no_blobs), Submit::Ping { resource: 12 });
        assert_eq!(asg.submit(1, &ping(ASG_GET_RING), no_blobs), Submit::Done, "only notifies wake the ring");
        // The guest drops the ring, then the context.
        asg.unref(12);
        assert_eq!(asg.submit(1, &ping(ASG_NOTIFY_AVAILABLE), no_blobs), Submit::Done);
        asg.destroy(1);
        assert!(asg.is_empty());
    }

    #[test]
    fn the_new_protocol_passes_through() {
        let mut asg = LegacyAsg::default();
        // GFXSTREAM_CONTEXT_CREATE of the context's own blob, even one numbered 10.
        assert_eq!(asg.submit(1, &cmd(&[OP_CREATE, 10]), |id| id == 10), Submit::Pass);
        assert_eq!(asg.submit(1, &cmd(&[OP_CREATE, 4]), |_| false), Submit::Pass);
        assert_eq!(asg.submit(1, &cmd(&[OP_PING, 10]), |_| true), Submit::Pass, "GFXSTREAM_CONTEXT_PING");
        assert!(asg.is_empty());
        // Other commands (renderControl, export syncs) and the old pings of a context that never
        // made a subdevice.
        assert_eq!(asg.submit(1, &cmd(&[0x1004, 1, 2, 3]), |_| false), Submit::Pass);
        assert_eq!(asg.submit(1, &ping(ASG_NOTIFY_AVAILABLE), |_| false), Submit::Pass);
        assert_eq!(asg.submit(1, &ping_with_response(9, 0, ASG_GET_RING), |_| false), Submit::Pass);
        assert_eq!(asg.submit(1, &[], |_| false), Submit::Pass);
        assert!(!asg.is_ring(1, hostmem_id(1)));
    }

    #[test]
    fn contexts_are_apart_and_a_reset_forgets_them() {
        let mut asg = LegacyAsg::default();
        for ctx in [1, 2] {
            asg.submit(ctx, &cmd(&[OP_CREATE, SUBDEVICE_VIRTIO_GPU_GRAPHICS]), |_| false);
        }
        assert_ne!(hostmem_id(1), hostmem_id(2));
        asg.ring_created(2, 30);
        assert!(asg.is_ring(1, hostmem_id(1)));
        assert_eq!(asg.submit(2, &ping(ASG_NOTIFY_AVAILABLE), |_| false), Submit::Ping { resource: 30 });
        assert_eq!(asg.submit(1, &ping(ASG_NOTIFY_AVAILABLE), |_| false), Submit::Done);
        asg.clear();
        assert!(asg.is_empty());
        assert_eq!(asg.submit(2, &ping(ASG_NOTIFY_AVAILABLE), |_| false), Submit::Pass);
    }
}
