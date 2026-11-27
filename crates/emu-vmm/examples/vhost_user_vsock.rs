//! Serves emu's virtio-vsock, with the adb bridge and boot properties behind it, to a vhost-user
//! frontend: QEMU running an Android guest under TCG on a host without KVM (#55).
//!
//! ```text
//! cargo run --release -p simee-emu-vmm --example vhost_user_vsock -- <socket> <adb port> [prop=value ...]
//! qemu-system-x86_64 ... -object memory-backend-memfd,id=mem,size=2G,share=on -machine memory-backend=mem \
//!     -chardev socket,id=vs,path=<socket> -device vhost-user-vsock-pci,chardev=vs
//! adb connect 127.0.0.1:<adb port>
//! ```
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;

    use simee_emu_vmm::pipe::{PipeRegistry, adb::AdbService, qemud::BootProperties};
    use simee_emu_vmm::virtio::vsock::{Vsock, event_channel};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(path), Some(port)) = (args.first(), args.get(1).and_then(|p| p.parse::<u16>().ok())) else {
        return Err("usage: vhost_user_vsock <socket> <adb port> [prop=value ...]".into());
    };
    let props = args[2..].iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let (events, rx) = event_channel();
    let adb = AdbService::bind(port, events.clone())?;
    let mut registry = PipeRegistry::default();
    registry.register("qemud:adb", Box::new(adb));
    registry.register("qemud:boot-properties", Box::new(BootProperties { props }));
    let dev = Vsock::new(3, Arc::new(registry), events, rx);
    let server = simee_emu_vmm::vhost_user::serve(std::path::Path::new(path), "vsock", Box::new(dev))?;
    println!("vsock on {path}, adb on 127.0.0.1:{port}");
    AdbService::announce_port(port);
    server.join().map_err(|_| "server thread panicked")?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("vhost_user_vsock runs on Linux only");
}
