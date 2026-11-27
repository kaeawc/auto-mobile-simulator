//! App lifecycle on a running device, over adb: install, uninstall, launch, terminate, list and
//! open a URL. Any emulator that has an adb serial works (simee's `emu` and the stock one); adb
//! comes from the SDK's platform-tools or the PATH ([`Android::adb`]).
//!
//! `adb shell` hands its arguments to the guest's shell as one string, so every value that came
//! from a caller is either checked against what it must be (a package name) or quoted
//! ([`shell_quote`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::{Android, Device};

/// How long `launch` waits, by default, for the app's process to show up after `am start -W`
/// returned.
const PID_WAIT: Duration = Duration::from_secs(3);
const PID_POLL: Duration = Duration::from_millis(100);

/// adb bound to one running device.
#[derive(Clone, Debug)]
pub struct Adb {
    pub adb: PathBuf,
    pub serial: String,
    /// `bundletool`, when the host has one: what installs an `.aab`.
    pub bundletool: Option<PathBuf>,
    /// How long `launch` looks for the app's process.
    pub pid_wait: Duration,
}

impl Device {
    /// adb for this device. Fails when it isn't running or the host has no adb.
    pub fn adb(&self, android: &Android) -> Result<Adb, String> {
        let serial = self.running()?.serial();
        let adb = android.adb().ok_or("adb not found: install the SDK's platform-tools (`emu -provision`) or put adb on the PATH")?;
        Ok(Adb { adb, serial, bundletool: on_path("bundletool"), pid_wait: PID_WAIT })
    }
}

impl Adb {
    /// Runs adb on the device with `args` after `-s <serial>`: its success and its stdout and
    /// stderr, in that order, as text.
    fn run(&self, args: &[&str]) -> Result<(bool, String), String> {
        let out = Command::new(&self.adb)
            .args(["-s", &self.serial])
            .args(args)
            .output()
            .map_err(|e| format!("couldn't run {}: {e}", self.adb.display()))?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), text))
    }

    /// `adb shell <command>`: what it printed, or why it failed.
    fn shell(&self, command: &str) -> Result<String, String> {
        match self.run(&["shell", command])? {
            (true, text) => Ok(text),
            (false, text) => Err(failure(command, &text)),
        }
    }

    /// Installs an app over the one of the same name, if any: an `.apk` with `adb install -r`, an
    /// `.aab` through `bundletool` when the host has it.
    pub fn install(&self, path: &Path) -> Result<(), String> {
        if !path.is_file() {
            return Err(format!("{} isn't a file", path.display()));
        }
        match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("apk") => {
                let (ok, text) = self.run(&["install", "-r", &path.to_string_lossy()])?;
                install_result(ok, &text)
            }
            Some("aab") => self.install_bundle(path),
            _ => Err(format!("{} isn't an .apk or .aab", path.display())),
        }
    }

    /// An `.aab`: `bundletool build-apks` for this device, then `install-apks`.
    fn install_bundle(&self, bundle: &Path) -> Result<(), String> {
        let bundletool = self.bundletool.as_ref().ok_or(
            "installing an .aab needs `bundletool` on the PATH (https://developer.android.com/tools/bundletool); an .apk installs without it",
        )?;
        let dir = std::env::temp_dir().join(format!("simee-aab-{}-{}", std::process::id(), self.serial));
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let apks = dir.join("app.apks");
        let tool = |args: Vec<String>| -> Result<(), String> {
            let out = Command::new(bundletool).args(args).output().map_err(|e| format!("couldn't run {}: {e}", bundletool.display()))?;
            if out.status.success() {
                return Ok(());
            }
            Err(failure("bundletool", &format!("{}{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout))))
        };
        let (adb, id) = (format!("--adb={}", self.adb.display()), format!("--device-id={}", self.serial));
        let built = tool(vec![
            "build-apks".into(),
            format!("--bundle={}", bundle.display()),
            format!("--output={}", apks.display()),
            "--connected-device".into(),
            "--overwrite".into(),
            adb.clone(),
            id.clone(),
        ]);
        let result = built.and_then(|()| tool(vec!["install-apks".into(), format!("--apks={}", apks.display()), adb, id]));
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    /// `pm uninstall`.
    pub fn uninstall(&self, package: &str) -> Result<(), String> {
        let package = package_name(package)?;
        install_result(true, &self.shell(&format!("pm uninstall {package}"))?)
    }

    /// Starts the app's launcher activity and returns its process id (`None` when no process
    /// named for the package shows up: an activity declared in another process). With `restart`
    /// a running instance is stopped first (`am start -S`); without, it is brought to the front.
    pub fn launch(&self, package: &str, restart: bool) -> Result<Option<u32>, String> {
        let package = package_name(package)?;
        let resolved = self.shell(&format!("cmd package resolve-activity --brief -c android.intent.category.LAUNCHER {package}"))?;
        let component = launcher_component(&resolved)
            .ok_or_else(|| format!("{package} has no launcher activity (is it installed? `apps` lists what is)"))?;
        let stop = if restart { " -S" } else { "" };
        let started = self.shell(&format!(
            "am start -W{stop} -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n {}",
            shell_quote(&component)
        ))?;
        if let Some(error) = am_error(&started) {
            return Err(format!("launching {package}: {error}"));
        }
        let deadline = Instant::now() + self.pid_wait;
        loop {
            if let Some(pid) = parse_pid(&self.shell(&format!("pidof -s {package}"))?) {
                return Ok(Some(pid));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            sleep(PID_POLL);
        }
    }

    /// `am force-stop`.
    pub fn terminate(&self, package: &str) -> Result<(), String> {
        let package = package_name(package)?;
        self.shell(&format!("am force-stop {package}")).map(|_| ())
    }

    /// The installed packages, sorted: the third-party ones, or with `all` the system's too.
    pub fn apps(&self, all: bool) -> Result<Vec<String>, String> {
        Ok(parse_packages(&self.shell(if all { "pm list packages" } else { "pm list packages -3" })?))
    }

    /// Opens `url` with whatever handles it (`am start -a android.intent.action.VIEW -d`).
    pub fn open_url(&self, url: &str) -> Result<(), String> {
        let url = checked_url(url)?;
        let started = self.shell(&format!("am start -a android.intent.action.VIEW -d {}", shell_quote(url)))?;
        match am_error(&started) {
            Some(error) => Err(format!("opening {url}: {error}")),
            None => Ok(()),
        }
    }
}

/// `name` if it is a package name: dot-separated segments of letters, digits and underscores,
/// each starting with a letter, at least two (Android's rule), so it is safe in a shell line.
pub fn package_name(name: &str) -> Result<&str, String> {
    let segment = |s: &str| s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if name.split('.').count() >= 2 && name.split('.').all(segment) {
        Ok(name)
    } else {
        Err(format!("`{name}` isn't a package name (like com.example.app)"))
    }
}

/// `url` if it starts with a scheme (`https:`, `myapp:`).
fn checked_url(url: &str) -> Result<&str, String> {
    let scheme_end = url.find(':').unwrap_or(0);
    let scheme = &url[..scheme_end];
    let valid = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if valid { Ok(url) } else { Err(format!("`{url}` isn't a URL (it needs a scheme, like https://example.com)")) }
}

/// `s` as one word for the guest's shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The outcome of `pm uninstall` and `adb install`, which print `Success` or `Failure [REASON]`
/// and exit 0 or not depending on the adb version.
fn install_result(ok: bool, text: &str) -> Result<(), String> {
    let says_success = text.lines().any(|l| l.trim() == "Success");
    if says_success && !text.contains("Failure") {
        return Ok(());
    }
    let reason = text.lines().rev().find(|l| l.contains("Failure") || l.contains("Error")).unwrap_or("").trim();
    if reason.is_empty() && ok {
        return Err(format!("no `Success` in: {}", text.trim()));
    }
    Err(failure("adb", if reason.is_empty() { text } else { reason }))
}

/// An error from a command's output, its last line if it printed any.
fn failure(what: &str, text: &str) -> String {
    let text = text.trim();
    let line = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
    format!("{what} failed: {}", line.trim())
}

/// The launcher component (`com.example.app/.MainActivity`) in `cmd package resolve-activity
/// --brief`'s output: its last line, when it is a component and not "No activity found".
fn launcher_component(out: &str) -> Option<String> {
    let line = out.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let (package, class) = line.split_once('/')?;
    (!line.contains(' ') && !package.is_empty() && !class.is_empty()).then(|| line.to_owned())
}

/// What `am start` reports as an error (it exits 0 either way): the first `Error` line.
fn am_error(out: &str) -> Option<String> {
    out.lines().map(str::trim).find(|l| l.starts_with("Error") && !l.starts_with("Error type")).map(str::to_owned)
}

/// The first process id `pidof` printed.
fn parse_pid(out: &str) -> Option<u32> {
    out.split_whitespace().next()?.parse().ok()
}

/// The names in `pm list packages`'s `package:<name>` lines, sorted.
fn parse_packages(out: &str) -> Vec<String> {
    let mut names: Vec<String> = out.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(str::to_owned).collect();
    names.sort();
    names.dedup();
    names
}

/// `name` in the PATH's directories.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fake_adb, fake_emulator};

    #[test]
    fn package_names_are_checked_before_they_reach_a_shell() {
        assert_eq!(package_name("com.example.app"), Ok("com.example.app"));
        assert_eq!(package_name("com.example.app_2"), Ok("com.example.app_2"));
        for bad in ["a", "", "com.example;reboot", "com..app", "com.1app", "$(id).x", "com.example app", "-n.x"] {
            assert!(package_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_need_a_scheme_and_are_quoted() {
        assert!(checked_url("https://example.com/a?b=1&c=2").is_ok());
        assert!(checked_url("myapp:open").is_ok());
        for bad in ["example.com", "a", "", "://x", "1http://x"] {
            assert!(checked_url(bad).is_err(), "{bad}");
        }
        assert_eq!(shell_quote("https://x/?a=1&b=it's"), r"'https://x/?a=1&b=it'\''s'");
    }

    #[test]
    fn install_and_uninstall_outcomes() {
        assert_eq!(install_result(true, "Performing Streamed Install\nSuccess\n"), Ok(()));
        assert_eq!(
            install_result(true, "Failure [DELETE_FAILED_INTERNAL_ERROR]\n"),
            Err("adb failed: Failure [DELETE_FAILED_INTERNAL_ERROR]".into())
        );
        assert_eq!(
            install_result(false, "Performing Streamed Install\nadb: failed to install a.apk: Failure [INSTALL_PARSE_FAILED_NOT_APK]\n"),
            Err("adb failed: adb: failed to install a.apk: Failure [INSTALL_PARSE_FAILED_NOT_APK]".into())
        );
        assert!(install_result(true, "").expect_err("silence isn't success").contains("no `Success`"));
        assert!(install_result(false, "error: device offline").expect_err("offline").contains("device offline"));
    }

    #[test]
    fn the_launcher_component_is_the_last_line() {
        let out = "priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\ncom.example.app/.MainActivity\n";
        assert_eq!(launcher_component(out).as_deref(), Some("com.example.app/.MainActivity"));
        assert_eq!(launcher_component("com.example.app/.Main$Inner").as_deref(), Some("com.example.app/.Main$Inner"));
        assert_eq!(launcher_component("No activity found\n"), None);
        assert_eq!(launcher_component(""), None);
    }

    #[test]
    fn am_start_errors_are_found_in_its_output() {
        assert_eq!(am_error("Starting: Intent { }\nStatus: ok\nComplete\n"), None);
        assert_eq!(
            am_error("Starting: Intent { }\nError type 3\nError: Activity class {a/b} does not exist.\n").as_deref(),
            Some("Error: Activity class {a/b} does not exist.")
        );
        assert_eq!(am_error("Warning: Activity not started, its current task has been brought to the front\n"), None);
    }

    #[test]
    fn pids_and_package_lists_parse() {
        assert_eq!(parse_pid("4321\n"), Some(4321));
        assert_eq!(parse_pid("4321 4400\n"), Some(4321));
        assert_eq!(parse_pid(""), None);
        assert_eq!(parse_packages("package:b.x\r\npackage:a.x\n\nWarning: x\n"), ["a.x", "b.x"]);
    }

    struct Fixture {
        root: PathBuf,
        adb: Adb,
        calls: crate::testing::FakeAdb,
        _fake: crate::testing::FakeEmulator,
    }

    /// A running AVD whose adb is the fake.
    fn fixture(tag: &str) -> Fixture {
        let root = PathBuf::from("/tmp").join(format!("sa-apps-{tag}-{}", std::process::id()));
        let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let calls = fake_adb(&android);
        let _fake = fake_emulator(&android, "phone");
        let device = android.device("phone").expect("device");
        let adb = Adb { pid_wait: Duration::ZERO, ..device.adb(&android).expect("adb") };
        Fixture { root, adb, calls, _fake }
    }

    #[test]
    fn launch_resolves_the_activity_and_returns_the_pid() {
        let f = fixture("launch");
        assert_eq!(f.adb.launch("com.example.app", false), Ok(Some(4321)));
        assert_eq!(
            f.calls.calls(),
            [
                "shell cmd package resolve-activity --brief -c android.intent.category.LAUNCHER com.example.app",
                "shell am start -W -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n 'com.example.app/.MainActivity'",
                "shell pidof -s com.example.app",
            ]
        );
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn launch_with_restart_stops_the_running_instance() {
        let f = fixture("restart");
        assert_eq!(f.adb.launch("com.example.app", true), Ok(Some(4321)));
        assert!(f.calls.calls()[1].starts_with("shell am start -W -S "), "{:?}", f.calls.calls());
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn launch_failures_say_why() {
        let f = fixture("launchfail");
        let none = f.adb.launch("com.missing.app", true).expect_err("not installed");
        assert_eq!(none, "com.missing.app has no launcher activity (is it installed? `apps` lists what is)");
        let gone = f.adb.launch("com.example.crash", true).expect_err("activity is gone");
        assert!(gone.starts_with("launching com.example.crash: Error: Activity class"), "{gone}");
        assert!(f.adb.launch("com.example;reboot", true).expect_err("not a package").contains("isn't a package name"));
        assert_eq!(f.calls.calls().len(), 3, "the bad name never reached adb: {:?}", f.calls.calls());
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn a_process_that_never_shows_up_is_no_pid() {
        let f = fixture("nopid");
        assert_eq!(f.adb.launch("com.example.svc", false), Ok(None));
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn install_runs_adb_install_r_and_reports_failures() {
        let f = fixture("install");
        let apk = f.root.join("app.apk");
        std::fs::write(&apk, b"PK").expect("apk");
        assert_eq!(f.adb.install(&apk), Ok(()));
        assert_eq!(f.calls.calls(), [format!("install -r {}", apk.display())]);
        let broken = f.root.join("broken.apk");
        std::fs::write(&broken, b"PK").expect("apk");
        let err = f.adb.install(&broken).expect_err("broken");
        assert!(err.contains("INSTALL_PARSE_FAILED_NOT_APK"), "{err}");
        assert!(f.adb.install(&f.root.join("absent.apk")).expect_err("absent").ends_with("isn't a file"));
        let txt = f.root.join("app.txt");
        std::fs::write(&txt, b"x").expect("txt");
        assert!(f.adb.install(&txt).expect_err("extension").ends_with("isn't an .apk or .aab"));
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn an_aab_needs_bundletool() {
        let mut f = fixture("aab");
        let aab = f.root.join("app.aab");
        std::fs::write(&aab, b"PK").expect("aab");
        f.adb.bundletool = None;
        let err = f.adb.install(&aab).expect_err("no bundletool");
        assert!(err.starts_with("installing an .aab needs `bundletool`"), "{err}");
        assert!(f.calls.calls().is_empty(), "nothing ran");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn an_aab_is_built_and_installed_by_bundletool() {
        use std::os::unix::fs::PermissionsExt;
        let mut f = fixture("bundletool");
        let aab = f.root.join("app.aab");
        std::fs::write(&aab, b"PK").expect("aab");
        let tool = f.root.join("bundletool");
        let log = f.root.join("bundletool.log");
        std::fs::write(&tool, format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display())).expect("tool");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        f.adb.bundletool = Some(tool);
        assert_eq!(f.adb.install(&aab), Ok(()));
        let log = std::fs::read_to_string(&log).expect("log");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(lines[0].starts_with(&format!("build-apks --bundle={} --output=", aab.display())), "{log}");
        assert!(lines[0].contains("--connected-device --overwrite") && lines[0].contains("--device-id=emulator-"), "{log}");
        assert!(lines[1].starts_with("install-apks --apks=") && lines[1].contains("--adb="), "{log}");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn uninstall_terminate_apps_and_open_url() {
        let f = fixture("rest");
        assert_eq!(f.adb.uninstall("com.example.app"), Ok(()));
        assert_eq!(f.adb.uninstall("com.missing.app"), Err("adb failed: Failure [DELETE_FAILED_INTERNAL_ERROR]".into()));
        assert_eq!(f.adb.terminate("com.example.app"), Ok(()));
        assert_eq!(f.adb.apps(false), Ok(vec!["com.example.app".to_owned(), "com.example.zeta".to_owned()]));
        assert_eq!(f.adb.apps(true).expect("all"), ["android", "com.android.settings", "com.example.app", "com.example.zeta"]);
        assert_eq!(f.adb.open_url("https://example.com/"), Ok(()));
        assert!(f.adb.open_url("nowhere://x").expect_err("no handler").starts_with("opening nowhere://x: Error: Activity not started"));
        assert!(f.adb.open_url("example.com").expect_err("no scheme").contains("isn't a URL"));
        assert_eq!(
            f.calls.calls(),
            [
                "shell pm uninstall com.example.app",
                "shell pm uninstall com.missing.app",
                "shell am force-stop com.example.app",
                "shell pm list packages -3",
                "shell pm list packages",
                "shell am start -a android.intent.action.VIEW -d 'https://example.com/'",
                "shell am start -a android.intent.action.VIEW -d 'nowhere://x'",
            ]
        );
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn a_device_that_isnt_running_or_a_host_without_adb_is_refused() {
        let f = fixture("refused");
        let mut bare = Android::new(emu_avd::Env { sdk_root: Some(f.root.join("empty-sdk")), avd_home: f.root.join("avd") });
        bare.running_dir = Some(f.root.join("running"));
        let d = bare.device("phone").expect("device");
        // With no adb in this SDK the PATH's is used; a host without one says so.
        if bare.adb().is_none() {
            assert!(d.adb(&bare).expect_err("no adb").starts_with("adb not found"));
        }
        let idle = Android { running_dir: Some(f.root.join("nothing-running")), ..bare };
        assert_eq!(idle.device("phone").expect("device").adb(&idle).expect_err("not running"), "phone isn't running; boot it first");
        std::fs::remove_dir_all(&f.root).ok();
    }
}
