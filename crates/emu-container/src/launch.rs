//! Starting Android's `/init` in a user namespace, Redroid-style, with no container runtime.
//!
//! [`plan`] turns what the device needs (the unpacked image, the display, the host's devices) into
//! data: the namespaces, the id mapping, the mounts in order, and `/init`'s arguments. Tests assert
//! that data. [`Plan::script`] renders it as the `sh` script that runs inside the namespaces, and
//! [`start`] runs `unshare` on it, maps the ids from outside, and lets the script go.
//!
//! What the plan reproduces is what Docker gives Redroid (docs/research/container-backend.md):
//! a root filesystem that is an overlay of the image (the writable layer is the device's own
//! directory), a `/dev` populated with the nodes `redroid.common.rc` chmods, binderfs mounted with
//! its three devices linked where older images look for them, `/proc`, and the host's `/sys`.
//! The network namespace is the host's: the container's adbd listens on the host's port 5555 until
//! a network namespace with forwarding exists (docs/android.md, "Container backend").

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::probe::{HostView, subordinate_range};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{what}: {err}")]
    Io { what: String, err: std::io::Error },
    #[error("mapping the container's ids: {0}")]
    Map(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let what = what.into();
    move |err| Error::Io { what, err }
}

/// How the guest renders (`androidboot.redroid_gpu_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMode {
    /// Software rendering inside the container (SwiftShader): needs nothing from the host.
    Guest,
    /// The host's GPU through its render node.
    Host,
}

impl GpuMode {
    pub fn name(self) -> &'static str {
        match self {
            GpuMode::Guest => "guest",
            GpuMode::Host => "host",
        }
    }
}

/// What the display is (from the AVD's profile).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    /// Redroid's default (15 fps without a GPU) when `None`.
    pub fps: Option<u32>,
}

/// What a container needs to know about its device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The device's own state directory: `upper/`, `work/` and `merged/` live here.
    pub dir: PathBuf,
    /// The unpacked image, the overlay's read-only lower layer.
    pub rootfs: PathBuf,
    /// The image's entrypoint: `/init` and the arguments it was built to take.
    pub entrypoint: Vec<String>,
    pub display: Display,
    pub gpu: GpuMode,
    /// Extra `key=value` arguments for `/init`: Redroid turns `ro.xxx=value` into a property
    /// override and `androidboot.xxx` into `ro.boot.xxx`.
    pub props: Vec<String>,
}

/// How the container's ids map to the host's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdMap {
    /// Root on the host: every id maps to itself, so the image's owners stay as they are.
    Identity,
    /// An unprivileged user: id 0 is that user and ids 1.. are its subordinate range, as
    /// `/etc/subuid` and `/etc/subgid` grant, written through `newuidmap` and `newgidmap`.
    Rootless { uid: u32, gid: u32, sub_uid: (u32, u32), sub_gid: (u32, u32) },
}

/// The most subordinate ids mapped: Android's app uids run from 10000, and user 0 needs less
/// than 20000.
const MAPPED_IDS: u32 = 65536;

impl IdMap {
    /// The mapping for the process `view` describes: identity for root, else the user's
    /// subordinate ranges (an error that says what is missing when it has none).
    pub fn for_host(view: &dyn HostView, gid: u32) -> Result<IdMap> {
        let uid = view.euid();
        if uid == 0 {
            return Ok(IdMap::Identity);
        }
        let range = |file: &str| {
            subordinate_range(view, file, uid)
                .filter(|&(_, n)| n >= MAPPED_IDS)
                .ok_or_else(|| Error::Map(format!("{file} has no range of {MAPPED_IDS} ids for uid {uid}: `sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER`")))
        };
        Ok(IdMap::Rootless { uid, gid, sub_uid: range("/etc/subuid")?, sub_gid: range("/etc/subgid")? })
    }

    /// What the parent does to give the namespace of process `pid` its ids.
    pub fn actions(&self, pid: u32) -> Vec<Action> {
        match self {
            IdMap::Identity => ["uid_map", "gid_map"]
                .iter()
                .map(|f| Action::Write { path: PathBuf::from(format!("/proc/{pid}/{f}")), text: "0 0 4294967295\n".into() })
                .collect(),
            IdMap::Rootless { uid, gid, sub_uid, sub_gid } => [("newuidmap", *uid, *sub_uid), ("newgidmap", *gid, *sub_gid)]
                .iter()
                .map(|&(tool, own, (start, _))| {
                    let mut argv = vec![tool.to_string(), pid.to_string()];
                    for (inside, outside, count) in [(0, own, 1), (1, start, MAPPED_IDS - 1)] {
                        argv.extend([inside.to_string(), outside.to_string(), count.to_string()]);
                    }
                    Action::Run(argv)
                })
                .collect(),
        }
    }
}

/// One step of the id mapping, as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Write { path: PathBuf, text: String },
    Run(Vec<String>),
}

/// A mount, as `mount(8)` would take it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// `-t`: a filesystem type, or `None` for a bind mount.
    pub fstype: Option<&'static str>,
    pub source: String,
    pub target: PathBuf,
    /// `-o`, comma-joined.
    pub options: Vec<String>,
    /// `--rbind` instead of `--bind` (a bind mount only).
    pub recursive: bool,
}

/// One step of the setup script, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Mkdir(PathBuf),
    /// An empty file to bind a device node onto.
    Touch(PathBuf),
    Mount(Mount),
    Symlink {
        target: String,
        link: PathBuf,
    },
    /// Fails the setup with `message` unless `path` exists.
    Require {
        path: PathBuf,
        message: String,
    },
}

/// Everything needed to start a container, as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The overlay's merged directory: the container's root.
    pub root: PathBuf,
    pub idmap: IdMap,
    pub steps: Vec<Step>,
    /// `/init` and its arguments, run as the container's pid 1.
    pub init: Vec<String>,
}

/// The namespaces `unshare` creates. Not the network namespace: see the module docs.
pub const NAMESPACES: [&str; 6] = ["--user", "--mount", "--pid", "--uts", "--ipc", "--cgroup"];

/// The nodes `redroid.common.rc` chmods, which a container runtime normally provides.
const DEVICE_NODES: [&str; 6] = ["null", "zero", "full", "random", "urandom", "tty"];

/// The three binder devices, and where older images look for them.
const BINDER_DEVICES: [&str; 3] = ["binder", "hwbinder", "vndbinder"];

fn check_path(what: &str, p: &Path) -> Result<()> {
    let s = p.to_str().ok_or_else(|| Error::Invalid(format!("{what} {} isn't UTF-8", p.display())))?;
    if !p.is_absolute() {
        return Err(Error::Invalid(format!("{what} {s} must be an absolute path")));
    }
    // `,` and `:` end an overlay option's value; NUL and newlines have no place in any argument.
    if s.chars().any(|c| matches!(c, ',' | ':' | '\0' | '\n')) {
        return Err(Error::Invalid(format!(
            "{what} {s:?} contains a comma, colon, newline or NUL, which an overlay mount option can't carry"
        )));
    }
    Ok(())
}

fn check_prop(p: &str) -> Result<()> {
    let ok = p.split_once('=').is_some_and(|(k, _)| !k.is_empty()) && !p.chars().any(|c| c.is_whitespace() || c == '\0');
    if ok { Ok(()) } else { Err(Error::Invalid(format!("init argument {p:?} must be key=value with no whitespace"))) }
}

impl Spec {
    /// The arguments after `/init`: the image's, then Redroid's display, GPU and memfd settings,
    /// then the caller's.
    pub fn init_args(&self) -> Vec<String> {
        let mut args: Vec<String> = self.entrypoint.iter().skip(1).cloned().collect();
        args.push(format!("androidboot.redroid_width={}", self.display.width));
        args.push(format!("androidboot.redroid_height={}", self.display.height));
        args.push(format!("androidboot.redroid_dpi={}", self.display.dpi));
        if let Some(fps) = self.display.fps {
            args.push(format!("androidboot.redroid_fps={fps}"));
        }
        args.push(format!("androidboot.redroid_gpu_mode={}", self.gpu.name()));
        // ashmem left the kernel in 5.18; memfd replaces it.
        args.push("androidboot.use_memfd=true".into());
        args.extend(self.props.iter().cloned());
        args
    }

    fn validate(&self) -> Result<()> {
        check_path("the device directory", &self.dir)?;
        check_path("the root filesystem", &self.rootfs)?;
        if self.entrypoint.first().map(String::as_str) != Some("/init") {
            return Err(Error::Invalid(format!("the image's entrypoint must start with /init, not {:?}", self.entrypoint.first())));
        }
        for a in self.entrypoint.iter().skip(1).chain(&self.props) {
            check_prop(a)?;
        }
        if self.display.width == 0 || self.display.height == 0 || self.display.dpi == 0 {
            return Err(Error::Invalid(format!(
                "the display is {}x{} at {} dpi",
                self.display.width, self.display.height, self.display.dpi
            )));
        }
        Ok(())
    }
}

/// The plan for `spec` on the host `view` describes (which optional device nodes it has).
pub fn plan(spec: &Spec, view: &dyn HostView, idmap: IdMap) -> Result<Plan> {
    spec.validate()?;
    let root = spec.dir.join("merged");
    let at = |sub: &str| root.join(sub);
    let (upper, work) = (spec.dir.join("upper"), spec.dir.join("work"));
    let mount = |fstype: Option<&'static str>, source: &str, target: PathBuf, options: &[&str]| {
        Step::Mount(Mount {
            fstype,
            source: source.to_string(),
            target,
            options: options.iter().map(|s| s.to_string()).collect(),
            recursive: false,
        })
    };

    let mut steps = vec![Step::Mkdir(upper.clone()), Step::Mkdir(work.clone()), Step::Mkdir(root.clone())];

    // The root: the image, read-only, under the device's own writable layer. `userxattr` keeps the
    // overlay's bookkeeping in `user.*` attributes, which a user namespace may write.
    steps.push(Step::Mount(Mount {
        fstype: Some("overlay"),
        source: "overlay".into(),
        target: root.clone(),
        options: vec![
            format!("lowerdir={}", spec.rootfs.display()),
            format!("upperdir={}", upper.display()),
            format!("workdir={}", work.display()),
            "userxattr".into(),
        ],
        recursive: false,
    }));

    // /dev: a tmpfs holding the nodes Redroid's init expects to find (it only chmods them).
    steps.push(Step::Mkdir(at("dev")));
    steps.push(mount(Some("tmpfs"), "tmpfs", at("dev"), &["mode=755", "nosuid"]));
    for node in DEVICE_NODES {
        steps.push(Step::Touch(at(&format!("dev/{node}"))));
        steps.push(Step::Mount(Mount {
            fstype: None,
            source: format!("/dev/{node}"),
            target: at(&format!("dev/{node}")),
            options: vec![],
            recursive: false,
        }));
    }
    // Optional nodes: only what this host has.
    let mut optional = vec!["fuse", "ion"];
    if spec.gpu == GpuMode::Host {
        optional.push("dri");
    }
    optional.push("dma_heap");
    for node in optional {
        let host = format!("/dev/{node}");
        if !view.exists(&host) {
            continue;
        }
        let target = at(&format!("dev/{node}"));
        // A directory (dri, dma_heap) is a bind of a directory; a node needs a file to bind onto.
        steps.push(if matches!(node, "dri" | "dma_heap") { Step::Mkdir(target.clone()) } else { Step::Touch(target.clone()) });
        steps.push(Step::Mount(Mount {
            fstype: None,
            source: host,
            target,
            options: vec![],
            recursive: matches!(node, "dri" | "dma_heap"),
        }));
    }
    steps.push(Step::Mkdir(at("dev/pts")));
    steps.push(mount(Some("devpts"), "devpts", at("dev/pts"), &["newinstance", "ptmxmode=0666", "mode=0620"]));
    steps.push(Step::Symlink { target: "pts/ptmx".into(), link: at("dev/ptmx") });
    steps.push(Step::Mkdir(at("dev/shm")));
    steps.push(mount(Some("tmpfs"), "tmpfs", at("dev/shm"), &["mode=1777", "nosuid", "nodev"]));

    // binderfs, with its devices linked where older images look for them (Android's own init
    // mounts another instance over this one on newer images).
    steps.push(Step::Mkdir(at("dev/binderfs")));
    steps.push(mount(Some("binder"), "binder", at("dev/binderfs"), &[]));
    for dev in BINDER_DEVICES {
        steps.push(Step::Require {
            path: at(&format!("dev/binderfs/{dev}")),
            message: format!("binderfs came up without {dev}: load binder_linux with devices=binder,hwbinder,vndbinder (or boot a kernel whose CONFIG_ANDROID_BINDER_DEVICES lists them)"),
        });
        steps.push(Step::Symlink { target: format!("binderfs/{dev}"), link: at(&format!("dev/{dev}")) });
    }

    // /proc is this container's pid namespace; /sys is the host's (a user namespace can't mount a
    // sysfs without owning the network namespace).
    steps.push(Step::Mkdir(at("proc")));
    steps.push(mount(Some("proc"), "proc", at("proc"), &["nosuid", "nodev", "noexec"]));
    steps.push(Step::Mkdir(at("sys")));
    steps.push(Step::Mount(Mount { fstype: None, source: "/sys".into(), target: at("sys"), options: vec![], recursive: true }));

    let mut init = vec!["/init".to_string()];
    init.extend(spec.init_args());
    Ok(Plan { root, idmap, steps, init })
}

/// POSIX-shell single quoting: safe for any bytes but NUL.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn mount_argv(m: &Mount) -> Vec<String> {
    let mut argv = vec!["mount".to_string()];
    match m.fstype {
        Some(t) => argv.extend(["-t".to_string(), t.to_string()]),
        None => argv.push(if m.recursive { "--rbind" } else { "--bind" }.to_string()),
    }
    if !m.options.is_empty() {
        argv.extend(["-o".to_string(), m.options.join(",")]);
    }
    argv.push(m.source.clone());
    argv.push(m.target.display().to_string());
    argv
}

impl Plan {
    /// The `mount` command each mount step runs, in order: for tests and `-verbose`.
    pub fn mount_commands(&self) -> Vec<Vec<String>> {
        self.steps.iter().filter_map(|s| if let Step::Mount(m) = s { Some(mount_argv(m)) } else { None }).collect()
    }

    /// The setup as an `sh` script: waits for the parent to map the ids, does each step (naming
    /// the one that failed), and replaces itself with `/init` as the container's pid 1.
    pub fn script(&self) -> String {
        self.script_with(&self.init)
    }

    /// [`Plan::script`] ending in `argv` instead of `/init`, for exercising the mounts.
    pub fn script_with(&self, argv: &[String]) -> String {
        let mut s = String::from("set -eu\nstep() { \"$@\" || { echo \"emu: container setup failed: $*\" >&2; exit 1; }; }\n");
        s.push_str("# The parent writes the id maps, then a line.\nread -r _go\n");
        for step in &self.steps {
            let line = match step {
                Step::Mkdir(p) => format!("step mkdir -p {}", quote(&p.display().to_string())),
                Step::Touch(p) => format!("step touch {}", quote(&p.display().to_string())),
                Step::Mount(m) => format!("step {}", mount_argv(m).iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")),
                Step::Symlink { target, link } => format!("step ln -sf {} {}", quote(target), quote(&link.display().to_string())),
                Step::Require { path, message } => format!(
                    "[ -e {} ] || {{ echo {} >&2; exit 1; }}",
                    quote(&path.display().to_string()),
                    quote(&format!("emu: container setup failed: {message}"))
                ),
            };
            let _ = writeln!(s, "{line}");
        }
        let _ = write!(s, "exec chroot {}", quote(&self.root.display().to_string()));
        for a in argv {
            let _ = write!(s, " {}", quote(a));
        }
        s.push('\n');
        s
    }

    /// The `unshare` command line that runs [`Plan::script`] in fresh namespaces.
    pub fn command(&self) -> Vec<String> {
        unshare_command(&self.script())
    }
}

/// `unshare` with the container's namespaces around `sh -c <script>`. `--fork` makes the script
/// pid 1 of the new pid namespace; `--kill-child` takes it down with the launcher.
pub fn unshare_command(script: &str) -> Vec<String> {
    let mut argv = vec!["unshare".to_string()];
    argv.extend(NAMESPACES.iter().map(|s| s.to_string()));
    argv.extend(["--fork".into(), "--kill-child".into(), "--".into(), "sh".into(), "-c".into(), script.into(), "simee-container".into()]);
    argv
}

/// Runs `script` (which starts by reading a line from stdin: see [`Plan::script`]) in the
/// namespaces, maps its ids, and releases it. Returns the running `unshare`.
pub fn spawn_isolated(idmap: &IdMap, script: &str, stdout: Stdio, stderr: Stdio) -> Result<Child> {
    let argv = unshare_command(script);
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|e| Error::Io { what: "starting unshare (util-linux)".into(), err: e })?;
    let released = map_and_release(idmap, &mut child);
    if let Err(e) = released {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok(child)
}

/// `unshare` creates the user namespace in its own process, so its pid is the one to map. The
/// script waits on stdin until the maps are written.
fn map_and_release(idmap: &IdMap, child: &mut Child) -> Result<()> {
    use std::io::Write;
    wait_for_namespace(child)?;
    for action in idmap.actions(child.id()) {
        match action {
            Action::Write { path, text } => std::fs::write(&path, text).map_err(io(format!("writing {}", path.display())))?,
            Action::Run(argv) => {
                let out = Command::new(&argv[0]).args(&argv[1..]).output().map_err(io(format!("running {}", argv[0])))?;
                if !out.status.success() {
                    return Err(Error::Map(format!("`{}` failed: {}", argv.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
                }
            }
        }
    }
    let stdin = child.stdin.as_mut().ok_or_else(|| Error::Map("no stdin to release the container with".into()))?;
    stdin.write_all(b"go\n").map_err(io("releasing the container"))?;
    // Closing stdin: the script reads one line and never again.
    drop(child.stdin.take());
    Ok(())
}

/// Waits until the child has made its user namespace (its `ns/user` link differs from ours), so
/// writing its maps can't race `unshare`'s own setup. It polls the kernel's answer, not the
/// clock, and stops as soon as the namespace exists or the child is gone.
fn wait_for_namespace(child: &mut Child) -> Result<()> {
    let link = PathBuf::from(format!("/proc/{}/ns/user", child.id()));
    let ours = std::fs::read_link("/proc/self/ns/user").map_err(io("reading our user namespace"))?;
    loop {
        if let Some(status) = child.try_wait().map_err(io("waiting for unshare"))? {
            return Err(Error::Map(format!(
                "unshare exited ({status}) before its user namespace could be mapped; its message is in the log"
            )));
        }
        match std::fs::read_link(&link) {
            Ok(theirs) if theirs != ours => return Ok(()),
            _ => std::thread::yield_now(),
        }
    }
}

/// A running container.
pub struct Container {
    child: Child,
}

impl Container {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether `/init` is still running.
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Waits for the container to exit.
    pub fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    /// Stops the container hard: `unshare --kill-child` takes the pid namespace's init and
    /// everything under it down. (Android shuts down gracefully through `adb reboot -p`.)
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()?;
        self.child.wait().map(drop)
    }
}

/// Starts `/init` for `plan`, its console and logcat-less boot output going to `log`.
pub fn start(plan: &Plan, log: &Path) -> Result<Container> {
    let file = std::fs::File::create(log).map_err(io(format!("creating {}", log.display())))?;
    let err = file.try_clone().map_err(io("duplicating the log handle"))?;
    Ok(Container { child: spawn_isolated(&plan.idmap, &plan.script(), Stdio::from(file), Stdio::from(err))? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeView;

    fn spec() -> Spec {
        Spec {
            dir: "/avd/Pixel.avd/simee/container".into(),
            rootfs: "/cache/redroid-15-x86_64/rootfs".into(),
            entrypoint: ["/init", "qemu=1", "androidboot.hardware=redroid"].map(String::from).to_vec(),
            display: Display { width: 720, height: 1280, dpi: 320, fps: None },
            gpu: GpuMode::Guest,
            props: vec![],
        }
    }

    fn host() -> FakeView {
        FakeView::container_host()
    }

    fn plan_of(spec: &Spec, view: &FakeView) -> Plan {
        plan(spec, view, IdMap::Identity).expect("plan")
    }

    #[test]
    fn init_gets_the_images_arguments_then_redroids_settings() {
        let args = spec().init_args();
        assert_eq!(
            args,
            [
                "qemu=1",
                "androidboot.hardware=redroid",
                "androidboot.redroid_width=720",
                "androidboot.redroid_height=1280",
                "androidboot.redroid_dpi=320",
                "androidboot.redroid_gpu_mode=guest",
                "androidboot.use_memfd=true",
            ]
        );
        assert_eq!(plan_of(&spec(), &host()).init[0], "/init");
    }

    #[test]
    fn fps_gpu_mode_and_caller_props_come_through() {
        let mut s = spec();
        s.display.fps = Some(30);
        s.gpu = GpuMode::Host;
        s.props = vec!["ro.secure=0".into(), "androidboot.redroid_net_ndns=1".into()];
        let args = s.init_args();
        assert!(args.contains(&"androidboot.redroid_fps=30".to_string()));
        assert!(args.contains(&"androidboot.redroid_gpu_mode=host".to_string()));
        assert_eq!(args[args.len() - 2..], ["ro.secure=0", "androidboot.redroid_net_ndns=1"]);
        assert_eq!(args.iter().filter(|a| a.starts_with("androidboot.use_memfd")).count(), 1, "memfd is always on");
    }

    #[test]
    fn the_overlay_is_the_image_under_the_devices_own_layer() {
        let p = plan_of(&spec(), &host());
        assert_eq!(p.root, PathBuf::from("/avd/Pixel.avd/simee/container/merged"));
        assert_eq!(
            p.mount_commands()[0],
            [
                "mount",
                "-t",
                "overlay",
                "-o",
                "lowerdir=/cache/redroid-15-x86_64/rootfs,upperdir=/avd/Pixel.avd/simee/container/upper,workdir=/avd/Pixel.avd/simee/container/work,userxattr",
                "overlay",
                "/avd/Pixel.avd/simee/container/merged",
            ]
        );
        // The writable directories exist before the overlay is mounted.
        let first_mount = p.steps.iter().position(|s| matches!(s, Step::Mount(_))).expect("mount");
        let made: Vec<_> =
            p.steps[..first_mount].iter().filter_map(|s| if let Step::Mkdir(d) = s { Some(d.clone()) } else { None }).collect();
        assert_eq!(made.len(), 3);
        assert!(
            made.iter().any(|d| d.ends_with("upper"))
                && made.iter().any(|d| d.ends_with("work"))
                && made.iter().any(|d| d.ends_with("merged"))
        );
    }

    #[test]
    fn the_mounts_in_order() {
        let p = plan_of(&spec(), &host());
        let kinds: Vec<String> = p
            .mount_commands()
            .iter()
            .map(|c| {
                let t = c.iter().position(|a| a == "-t").map(|i| c[i + 1].clone());
                format!(
                    "{} {}",
                    t.unwrap_or_else(|| c[1].trim_start_matches('-').to_string()),
                    c.last().expect("target").trim_start_matches("/avd/Pixel.avd/simee/container/merged")
                )
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "overlay ",
                "tmpfs /dev",
                "bind /dev/null",
                "bind /dev/zero",
                "bind /dev/full",
                "bind /dev/random",
                "bind /dev/urandom",
                "bind /dev/tty",
                "rbind /dev/dma_heap",
                "devpts /dev/pts",
                "tmpfs /dev/shm",
                "binder /dev/binderfs",
                "proc /proc",
                "rbind /sys",
            ],
        );
    }

    #[test]
    fn binderfs_is_mounted_checked_and_linked_where_older_images_look() {
        let p = plan_of(&spec(), &host());
        let root = &p.root;
        let binder = p.mount_commands().into_iter().find(|c| c.contains(&"binder".to_string())).expect("binderfs");
        assert_eq!(binder, ["mount", "-t", "binder", "binder", &root.join("dev/binderfs").display().to_string()]);
        for dev in ["binder", "hwbinder", "vndbinder"] {
            let required = p.steps.iter().any(|s| matches!(s, Step::Require { path, message } if *path == root.join("dev/binderfs").join(dev) && message.contains("binder_linux")));
            let linked = p.steps.contains(&Step::Symlink { target: format!("binderfs/{dev}"), link: root.join("dev").join(dev) });
            assert!(required && linked, "{dev}");
        }
        // The devices are checked after the mount and before the links they would dangle from.
        let at = |f: &dyn Fn(&Step) -> bool| p.steps.iter().position(f).expect("step");
        let mount = at(&|s| matches!(s, Step::Mount(m) if m.fstype == Some("binder")));
        let require = at(&|s| matches!(s, Step::Require { .. }));
        assert!(mount < require);
    }

    #[test]
    fn dev_has_the_nodes_redroid_chmods_and_ptmx_through_a_private_devpts() {
        let p = plan_of(&spec(), &host());
        for node in ["null", "zero", "full", "random", "urandom", "tty"] {
            assert!(p.steps.contains(&Step::Touch(p.root.join("dev").join(node))), "{node}");
        }
        assert!(p.mount_commands().iter().any(|c| c.contains(&"newinstance,ptmxmode=0666,mode=0620".to_string())));
        assert!(p.steps.contains(&Step::Symlink { target: "pts/ptmx".into(), link: p.root.join("dev/ptmx") }));
    }

    #[test]
    fn optional_device_nodes_are_bound_only_when_the_host_has_them() {
        let bound = |p: &Plan, n: &str| p.mount_commands().iter().any(|c| c[c.len() - 2] == format!("/dev/{n}"));
        let bare = FakeView::container_host();
        let mut none = bare.clone();
        none.paths.clear();
        let p = plan_of(&spec(), &none);
        for n in ["fuse", "ion", "dri", "dma_heap"] {
            assert!(!bound(&p, n), "{n}");
        }
        let rich = bare.path("/dev/fuse").path("/dev/ion").path("/dev/dri");
        let p = plan_of(&spec(), &rich);
        assert!(bound(&p, "fuse") && bound(&p, "ion") && bound(&p, "dma_heap"));
        assert!(!bound(&p, "dri"), "the render node is for GPU mode host only");
        let mut host_gpu = spec();
        host_gpu.gpu = GpuMode::Host;
        let p = plan_of(&host_gpu, &rich);
        assert!(bound(&p, "dri"));
        // A directory is bound recursively; a node is bound onto a file.
        let dri = p
            .steps
            .iter()
            .find_map(|s| {
                if let Step::Mount(m) = s
                    && m.source == "/dev/dri"
                {
                    Some(m.clone())
                } else {
                    None
                }
            })
            .expect("dri mount");
        assert!(dri.recursive);
        assert!(p.steps.contains(&Step::Mkdir(p.root.join("dev/dri"))));
        assert!(p.steps.contains(&Step::Touch(p.root.join("dev/fuse"))));
    }

    #[test]
    fn the_network_namespace_is_the_hosts() {
        assert!(!NAMESPACES.contains(&"--net"));
        for ns in ["--user", "--mount", "--pid", "--uts", "--ipc", "--cgroup"] {
            assert!(NAMESPACES.contains(&ns), "{ns}");
        }
        let cmd = plan_of(&spec(), &host()).command();
        assert_eq!(cmd[0], "unshare");
        assert!(cmd.contains(&"--fork".to_string()) && cmd.contains(&"--kill-child".to_string()));
        assert_eq!(cmd[cmd.len() - 4..cmd.len() - 2], ["sh", "-c"]);
    }

    #[test]
    fn the_script_waits_for_the_id_maps_then_sets_up_then_execs_init() {
        let p = plan_of(&spec(), &host());
        let script = p.script();
        let read = script.find("read -r _go").expect("waits for the maps");
        let first_step = script.find("step mkdir").expect("steps");
        assert!(read < first_step, "nothing runs before the ids are mapped");
        let last = script.lines().last().expect("exec line");
        assert_eq!(
            last,
            "exec chroot '/avd/Pixel.avd/simee/container/merged' '/init' 'qemu=1' 'androidboot.hardware=redroid' 'androidboot.redroid_width=720' 'androidboot.redroid_height=1280' 'androidboot.redroid_dpi=320' 'androidboot.redroid_gpu_mode=guest' 'androidboot.use_memfd=true'"
        );
        assert!(script.contains("step 'mount' '-t' 'overlay' '-o' 'lowerdir=/cache/redroid-15-x86_64/rootfs,"), "{script}");
        assert!(script.contains("[ -e '/avd/Pixel.avd/simee/container/merged/dev/binderfs/hwbinder' ] ||"), "{script}");
        assert!(script.starts_with("set -eu\n"));
    }

    #[test]
    fn script_with_swaps_only_the_final_command() {
        let p = plan_of(&spec(), &host());
        let s = p.script_with(&["/bin/sh".into(), "-c".into(), "echo hi".into()]);
        assert!(s.ends_with("exec chroot '/avd/Pixel.avd/simee/container/merged' '/bin/sh' '-c' 'echo hi'\n"), "{s}");
        assert_eq!(s.lines().count(), p.script().lines().count());
    }

    #[test]
    fn quoting_survives_quotes_spaces_and_dollars() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's $HOME `x`"), r"'it'\''s $HOME `x`'");
        // A prop value can't smuggle a command into the script: it is one quoted word.
        let mut s = spec();
        s.props = vec!["ro.x=a;touch/tmp/pwned".into()];
        let script = plan_of(&s, &host()).script();
        assert!(script.contains("'ro.x=a;touch/tmp/pwned'"), "{script}");
    }

    #[test]
    fn bad_specs_are_refused_before_anything_is_planned() {
        let bad = |f: &dyn Fn(&mut Spec)| {
            let mut s = spec();
            f(&mut s);
            plan(&s, &host(), IdMap::Identity).expect_err("refused").to_string()
        };
        assert!(bad(&|s| s.dir = "relative/dir".into()).contains("absolute"));
        assert!(bad(&|s| s.rootfs = "/has,comma/rootfs".into()).contains("comma"));
        assert!(bad(&|s| s.dir = "/has:colon".into()).contains("colon"));
        assert!(bad(&|s| s.dir = "/new\nline".into()).contains("newline"));
        assert!(bad(&|s| s.props = vec!["noequals".into()]).contains("key=value"));
        assert!(bad(&|s| s.props = vec!["a b=c".into()]).contains("whitespace"));
        assert!(bad(&|s| s.props = vec!["=x".into()]).contains("key=value"));
        assert!(bad(&|s| s.entrypoint = vec!["/bin/sh".into()]).contains("/init"));
        assert!(bad(&|s| s.entrypoint = vec![]).contains("/init"));
        assert!(bad(&|s| s.display.width = 0).contains("0x1280"));
    }

    #[test]
    fn root_maps_every_id_to_itself() {
        let actions = IdMap::Identity.actions(4242);
        assert_eq!(
            actions,
            [
                Action::Write { path: "/proc/4242/uid_map".into(), text: "0 0 4294967295\n".into() },
                Action::Write { path: "/proc/4242/gid_map".into(), text: "0 0 4294967295\n".into() },
            ]
        );
    }

    #[test]
    fn an_unprivileged_user_maps_itself_to_0_and_its_range_above() {
        let map = IdMap::Rootless { uid: 1000, gid: 1001, sub_uid: (100000, 65536), sub_gid: (200000, 65536) };
        assert_eq!(
            map.actions(7),
            [
                Action::Run(["newuidmap", "7", "0", "1000", "1", "1", "100000", "65535"].map(String::from).to_vec()),
                Action::Run(["newgidmap", "7", "0", "1001", "1", "1", "200000", "65535"].map(String::from).to_vec()),
            ]
        );
    }

    #[test]
    fn id_maps_are_derived_from_the_host() {
        assert_eq!(IdMap::for_host(&host(), 0).expect("root"), IdMap::Identity);
        let user = host()
            .euid(1000)
            .file("/etc/passwd", "ci:x:1000:1000::/home/ci:/bin/sh\n")
            .file("/etc/subuid", "ci:100000:65536\n")
            .file("/etc/subgid", "ci:300000:70000\n");
        assert_eq!(
            IdMap::for_host(&user, 1000).expect("rootless"),
            IdMap::Rootless { uid: 1000, gid: 1000, sub_uid: (100000, 65536), sub_gid: (300000, 70000) }
        );
        let short = user.clone().file("/etc/subuid", "ci:100000:1000\n");
        assert!(IdMap::for_host(&short, 1000).expect_err("too few").to_string().contains("usermod --add-subuids"));
        let none = host().euid(1000);
        assert!(matches!(IdMap::for_host(&none, 1000), Err(Error::Map(_))));
    }

    /// The launcher against the real kernel, in a user namespace as the current user. Needs
    /// `unshare` and a kernel that lets this user mount an overlay in one; it runs the mount
    /// sequence up to binderfs, which is where a host without binder stops: that message is the
    /// assertion there. Run with `SIMEE_E2E_CONTAINER=1`.
    #[test]
    fn real_host_mounts_in_a_user_namespace() {
        if std::env::var_os("SIMEE_E2E_CONTAINER").is_none() {
            eprintln!("skipped: set SIMEE_E2E_CONTAINER=1 to run the launcher against this host's kernel");
            return;
        }
        let tmp = crate::image::fixture::tmp("e2e");
        let rootfs = tmp.join("rootfs");
        std::fs::create_dir_all(rootfs.join("bin")).expect("rootfs");
        // A rootfs with a shell, so the final `chroot … sh` has something to run: the host's own
        // shell and the libraries it links.
        let sh = std::fs::canonicalize("/bin/sh").expect("sh");
        let ldd = Command::new("ldd").arg(&sh).output().expect("ldd");
        let mut files = vec![sh.clone()];
        for l in String::from_utf8_lossy(&ldd.stdout).lines() {
            if let Some(p) = l.split_whitespace().find(|w| w.starts_with('/')) {
                files.push(PathBuf::from(p));
            }
        }
        for f in files {
            let dest = rootfs.join(f.strip_prefix("/").expect("absolute"));
            std::fs::create_dir_all(dest.parent().expect("parent")).expect("mkdir");
            std::fs::copy(&f, &dest).expect("copy");
        }
        let view = crate::probe::FsView;
        let idmap = IdMap::for_host(&view, rustix::process::getegid().as_raw()).expect("an id mapping for this user");
        let mut s = spec();
        s.dir = tmp.join("device");
        s.rootfs = rootfs;
        let p = plan(&s, &view, idmap).expect("plan");
        let binder = crate::probe::probe(&view).get(crate::probe::Feature::Binder).is_some_and(|c| c.ok());
        let sh_path = format!("/{}", sh.strip_prefix("/").expect("absolute").display());
        let sh_path_str = sh_path.clone();
        let script = p.script_with(&[sh_path, "-c".into(), "echo started; [ -e /proc/self ] && echo proc; [ -e /dev/binderfs/hwbinder ] && echo hwbinder; [ -e /dev/null ] && echo devnull".into()]);
        let out = spawn_isolated(&p.idmap, &script, Stdio::piped(), Stdio::piped()).expect("spawn").wait_with_output().expect("wait");
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        eprintln!("binder on this host: {binder}\nstdout: {stdout}\nstderr: {stderr}");
        if binder {
            assert!(out.status.success(), "{stderr}");
            for want in ["started", "proc", "hwbinder", "devnull"] {
                assert!(stdout.contains(want), "{want} missing: {stdout}");
            }
            return;
        }
        assert!(!out.status.success());
        assert!(
            stderr.contains("emu: container setup failed: mount -t binder binder"),
            "a host without binder should stop at binderfs, naming it: {stderr}"
        );
        // Everything before binderfs worked, so run the rest without it: /proc, the host's /sys,
        // and the chroot into the overlay.
        let mut rest = p.clone();
        rest.steps.retain(|s| match s {
            Step::Mount(m) => m.fstype != Some("binder"),
            Step::Require { .. } => false,
            Step::Symlink { link, .. } => !link.to_string_lossy().contains("binder"),
            _ => true,
        });
        let script = rest.script_with(&[sh_path_str, "-c".into(), "echo started; [ -e /proc/self ] && echo proc; [ -e /sys/kernel ] && echo sys; [ -e /dev/null ] && echo devnull; [ -L /dev/ptmx ] && echo ptmx".into()]);
        let out = spawn_isolated(&rest.idmap, &script, Stdio::piped(), Stdio::piped()).expect("spawn").wait_with_output().expect("wait");
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        eprintln!("without binderfs: stdout: {stdout}\nstderr: {stderr}");
        assert!(out.status.success(), "{stderr}");
        for want in ["started", "proc", "sys", "devnull", "ptmx"] {
            assert!(stdout.contains(want), "{want} missing: {stdout}");
        }
    }
}
