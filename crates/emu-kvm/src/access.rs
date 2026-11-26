//! Why `/dev/kvm` can't be used, and what the user does about it.
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub const KVM_PATH: &str = "/dev/kvm";

/// A `/dev/kvm` that can't be opened, with the fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessProblem {
    /// No device: the kernel has no KVM (module not loaded, virtualization off in firmware, or a
    /// VM without nested virtualization).
    Missing,
    /// The device exists, but this user may not open it.
    Denied {
        /// The group that owns the device (normally `kvm`), when it can be named.
        group: Option<String>,
    },
    /// Anything else, as the OS reported it.
    Other(i32),
}

impl std::fmt::Display for AccessProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessProblem::Missing => write!(
                f,
                "{KVM_PATH} doesn't exist, so this host has no KVM. Turn virtualization on in the firmware and load the \
                 module (`sudo modprobe kvm`; on x86 also `kvm_intel` or `kvm_amd`). A cloud VM needs nested \
                 virtualization or a bare-metal instance. On arm64 the kernel must boot at EL2 (`dmesg | grep kvm` \
                 says `HYP mode not available` when it didn't)."
            ),
            AccessProblem::Denied { group } => {
                let group = group.as_deref().unwrap_or("kvm");
                write!(
                    f,
                    "{KVM_PATH} exists but this user can't open it. Add yourself to the `{group}` group \
                     (`sudo usermod -aG {group} $USER`) and log in again (or run `newgrp {group}`). emu needs no root \
                     after that."
                )
            }
            AccessProblem::Other(errno) => {
                write!(f, "opening {KVM_PATH} failed: {}", std::io::Error::from_raw_os_error(*errno))
            }
        }
    }
}

/// What `errno` from opening `path` means for the user.
pub fn diagnose(path: &Path, errno: i32) -> AccessProblem {
    match errno {
        libc::ENOENT | libc::ENXIO | libc::ENODEV => AccessProblem::Missing,
        libc::EACCES | libc::EPERM => {
            AccessProblem::Denied { group: std::fs::metadata(path).ok().and_then(|m| group_name(m.gid(), "/etc/group")) }
        }
        e => AccessProblem::Other(e),
    }
}

/// The name of group `gid` in a group file (`name:password:gid:members`).
fn group_name(gid: u32, group_file: &str) -> Option<String> {
    let groups = std::fs::read_to_string(group_file).ok()?;
    groups.lines().find_map(|line| {
        let mut f = line.split(':');
        let name = f.next()?;
        let id: u32 = f.nth(1)?.parse().ok()?;
        (id == gid).then(|| name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_device_says_how_to_get_kvm() {
        let p = diagnose(Path::new("/nonexistent/kvm"), libc::ENOENT);
        assert_eq!(p, AccessProblem::Missing);
        let msg = p.to_string();
        assert!(msg.contains("modprobe kvm") && msg.contains("nested") && msg.contains("HYP mode"), "{msg}");
    }

    #[test]
    fn a_denied_device_names_the_group_to_join() {
        let dir = std::env::temp_dir().join(format!("emu-kvm-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let groups = dir.join("group");
        std::fs::write(&groups, "root:x:0:\nkvm:x:993:alice\nbroken-line\n").expect("write");
        let groups = groups.to_str().expect("utf-8");
        assert_eq!(group_name(993, groups).as_deref(), Some("kvm"));
        assert_eq!(group_name(0, groups).as_deref(), Some("root"));
        assert_eq!(group_name(5, groups), None);
        assert_eq!(group_name(993, "/nonexistent/group"), None);
        let msg = AccessProblem::Denied { group: Some("kvm".into()) }.to_string();
        assert!(msg.contains("sudo usermod -aG kvm $USER") && msg.contains("no root"), "{msg}");
        // A group that can't be named still gets the usual advice.
        assert!(AccessProblem::Denied { group: None }.to_string().contains("`kvm` group"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn eperm_is_denied_and_other_errors_pass_through() {
        assert!(matches!(diagnose(Path::new("/nonexistent/kvm"), libc::EPERM), AccessProblem::Denied { group: None }));
        let p = diagnose(Path::new(KVM_PATH), libc::EBUSY);
        assert_eq!(p, AccessProblem::Other(libc::EBUSY));
        assert!(p.to_string().starts_with("opening /dev/kvm failed"), "{p}");
    }
}
