use std::path::{Path, PathBuf};

use super::super::plist::{self, Plist};
use super::super::{Op, PrefValue, Store, StoreKind, fake::unique};
use super::*;

const UDID: &str = "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000";
const APP: &str = "com.example.app";
/// cfprefsd's file for a test app on iOS 27.2 (`plist.rs`'s fixture).
const EVERY_TYPE: &[u8] = include_bytes!("../fixtures/cfprefsd-every-type.plist");

fn metadata(id: &str) -> Vec<u8> {
    plist::encode(&Plist::Dict(vec![
        ("MCMMetadataIdentifier".into(), Plist::String(id.into())),
        ("MCMMetadataVersion".into(), Plist::Integer(11)),
    ]))
    .expect("encode")
}

fn container(dir: &Path, id: &str) {
    std::fs::create_dir_all(dir.join("Library/Preferences")).expect("dir");
    std::fs::write(dir.join(METADATA), metadata(id)).expect("metadata");
}

fn defaults(entries: &[(&str, Plist)]) -> Vec<u8> {
    plist::encode(&Plist::Dict(entries.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect())).expect("encode")
}

/// Writes a defaults file the way cfprefsd replaces one: a new file renamed over it.
fn replace(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

/// A device set with one booted device: the app (entitled to `group.com.example`, with a
/// defaults file of every type, another domain and a suite named like a group it isn't entitled
/// to), another app, and the group's container.
fn device_set(tag: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("sim-api-prefs-ios-{}", unique(tag)));
    let dev = root.join(UDID);
    std::fs::create_dir_all(&dev).expect("dev");
    // As CoreSimulator writes it: XML, with a date (so not a flat dictionary of scalars).
    std::fs::write(
        dev.join("device.plist"),
        "<?xml version=\"1.0\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>isDeleted</key>\n\t<false/>\n\t<key>lastUsedAt</key>\n\
         \t<date>2026-11-27T12:02:24Z</date>\n\t<key>state</key>\n\t<integer>3</integer>\n</dict>\n</plist>\n",
    )
    .expect("device.plist");
    let data = dev.join("data");
    let app = data.join("Containers/Data/Application/AAAA");
    container(&app, APP);
    std::fs::write(app.join(format!("Library/Preferences/{APP}.plist")), EVERY_TYPE).expect("app plist");
    std::fs::write(app.join("Library/Preferences/other.domain.plist"), defaults(&[("k", Plist::Bool(true))])).expect("other");
    std::fs::write(app.join("Library/Preferences/group.unentitled.plist"), defaults(&[("u", Plist::Integer(1))])).expect("suite");
    container(&data.join("Containers/Data/Application/BBBB"), "com.other");
    let bundle = data.join("Containers/Bundle/Application/CCCC");
    container(&bundle, APP);
    std::fs::create_dir_all(bundle.join("Example.app")).expect("app dir");
    std::fs::write(bundle.join("Example.app/Info.plist"), defaults(&[("CFBundleExecutable", Plist::String("Example".into()))]))
        .expect("info");
    let mut exe = b"\xcf\xfa\xed\xfe junk \x00\x01".to_vec();
    exe.extend_from_slice(
        b"<?xml version=\"1.0\"?><plist><dict><key>com.apple.security.application-groups</key>\n\t<array>\n\t\t<string>group.com.example</string>\n\t</array></dict></plist>\x00more",
    );
    std::fs::write(bundle.join("Example.app/Example"), exe).expect("exe");
    let group = data.join("Containers/Shared/AppGroup/GGGG");
    container(&group, "group.com.example");
    std::fs::write(group.join("Library/Preferences/group.com.example.plist"), defaults(&[("g", Plist::String("shared".into()))]))
        .expect("group");
    container(&data.join("Containers/Shared/AppGroup/HHHH"), "group.unentitled");
    (root, app)
}

fn ids(stores: &[Store]) -> Vec<(String, &'static str)> {
    stores.iter().map(|s| (s.id(), s.kind.as_str())).collect()
}

#[test]
fn stores_are_the_app_domain_its_entitled_groups_and_its_other_domains() {
    let (root, _) = device_set("stores");
    let mut ios = Ios::with_root(root.clone());
    let stores = ios.list(UDID, APP).expect("stores");
    assert_eq!(
        ids(&stores),
        [
            (APP.to_owned(), "app_domain"),
            ("group.com.example".to_owned(), "app_group"),
            ("group.unentitled".to_owned(), "app_group"),
            ("other.domain".to_owned(), "domain"),
        ]
    );
    let err = ios.list(UDID, "com.absent").expect_err("absent");
    assert_eq!(err, "`com.absent` has no data container on this device: is it installed?");
    assert!(ios.list("1111AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000", APP).expect_err("no device").contains("doesn't exist"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn reads_decode_the_file_cfprefsd_uses_for_each_store() {
    let (root, app) = device_set("reads");
    let mut ios = Ios::with_root(root.clone());
    let read = |ios: &mut Ios, id: &str| ios.read_host(UDID, &Store::parse(Platform::Ios, APP, id).expect("store")).expect(id);
    let own = read(&mut ios, APP);
    assert_eq!(own.get("t_long"), Some(&PrefValue::Long(1_099_511_627_776)));
    assert_eq!(own.len(), 9);
    assert_eq!(read(&mut ios, "group.com.example").get("g"), Some(&PrefValue::String("shared".into())), "the group's container");
    assert_eq!(read(&mut ios, "group.unentitled").get("u"), Some(&PrefValue::Long(1)), "not entitled: the app's own container");
    assert_eq!(read(&mut ios, "never.written"), Entries::new(), "nothing written yet");
    // A reinstalled app gets a new container: found again, not read from the old path.
    let moved = app.with_file_name("ZZZZ");
    std::fs::rename(&app, &moved).expect("move");
    assert_eq!(read(&mut ios, APP).len(), 9);
    std::fs::write(moved.join("Library/Preferences/bad.plist"), b"<?xml junk").expect("bad");
    let err = ios.read_host(UDID, &Store::new(StoreKind::Domain, APP, "bad")).expect_err("bad");
    assert!(err.ends_with("not a binary property list (bplist00) (`fresh: true` reads it through cfprefsd)"), "{err}");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_tracker_reports_each_changed_key_as_files_are_replaced() {
    let (root, app) = device_set("tracker");
    let mut ios = Ios::with_root(root.clone());
    let mut tracker = ios.tracker(UDID, APP, None).expect("tracker");
    let prefs = app.join("Library/Preferences");
    let paths = tracker.paths();
    assert!(paths.contains(&prefs) && paths.contains(&prefs.join(format!("{APP}.plist"))), "{paths:?}");
    let at = Date::from_unix(1_800_000_000, 0);
    assert_eq!(tracker.poll(at), (vec![], false), "nothing changed");

    // The app's file replaced: one key changed, one added, the rest the same.
    let mut entries = plist::decode(EVERY_TYPE).expect("decode");
    if let Plist::Dict(e) = &mut entries {
        e.retain(|(k, _)| k != "t_bool");
        e.iter_mut().filter(|(k, _)| k == "app_tick").for_each(|(_, v)| *v = Plist::Integer(50));
        e.push(("added".into(), Plist::Real(0.5)));
    }
    replace(&prefs.join(format!("{APP}.plist")), &plist::encode(&entries).expect("encode"));
    let (changes, grew) = tracker.poll(at);
    let got: Vec<(String, Option<PrefValue>, Option<PrefValue>)> = changes.into_iter().map(|c| (c.key, c.old, c.new)).collect();
    assert_eq!(
        got,
        [
            ("added".to_owned(), None, Some(PrefValue::Double(0.5))),
            ("app_tick".to_owned(), Some(PrefValue::Long(49)), Some(PrefValue::Long(50))),
            ("t_bool".to_owned(), Some(PrefValue::Bool(true)), None),
        ]
    );
    assert!(!grew);

    // A file half-written (it can't be read) is read again on the next look.
    std::fs::write(prefs.join("other.domain.plist"), b"bplist00").expect("torn");
    assert_eq!(tracker.poll(at).0, vec![]);
    replace(&prefs.join("other.domain.plist"), &defaults(&[("k", Plist::Bool(false))]));
    let (changes, _) = tracker.poll(at);
    assert_eq!(changes.iter().map(|c| (c.store.id(), c.key.as_str())).collect::<Vec<_>>(), [("other.domain".to_owned(), "k")]);

    // A new domain is followed from empty; a removed file removes its keys.
    std::fs::write(prefs.join("new.domain.plist"), defaults(&[("n", Plist::Integer(1))])).expect("new");
    let (changes, grew) = tracker.poll(at);
    assert!(grew, "a new store to watch");
    assert_eq!(
        changes.iter().map(|c| (c.store.id(), c.key.as_str(), c.old.is_none())).collect::<Vec<_>>(),
        [("new.domain".to_owned(), "n", true)]
    );
    std::fs::remove_file(prefs.join("new.domain.plist")).expect("rm");
    let (changes, _) = tracker.poll(at);
    assert_eq!(changes.iter().map(|c| (c.key.as_str(), c.new.is_none())).collect::<Vec<_>>(), [("n", true)]);

    // One store's tracker sees only its store.
    let group = Store::parse(Platform::Ios, APP, "group.com.example").expect("store");
    let mut one = ios.tracker(UDID, APP, Some(&group)).expect("tracker");
    replace(&prefs.join("other.domain.plist"), &defaults(&[("k", Plist::Bool(true))]));
    assert_eq!(one.poll(at), (vec![], false));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn entitled_groups_come_from_every_entitlements_copy_in_the_executable() {
    let exe = b"\x00<key>com.apple.security.application-groups</key><array><string>group.b</string>\n<string>group.a</string></array>\
        \x00\x01<key>com.apple.security.application-groups</key>\r\n<array>\t<string>group.a</string></array>\
        <key>com.apple.security.application-groups</key><string>not-an-array</string>";
    assert_eq!(entitled_groups(exe), ["group.a", "group.b"]);
    assert_eq!(entitled_groups(b"no entitlements"), Vec::<String>::new());
}

#[test]
fn a_write_request_carries_every_op_for_the_helper() {
    let store = Store::new(StoreKind::AppDomain, APP, APP);
    let ops = [
        Op::Set { key: "f".into(), value: PrefValue::Float(1.5) },
        Op::Set { key: "a".into(), value: PrefValue::Array(vec![PrefValue::Int(1)]) },
        Op::Delete { key: "gone".into() },
    ];
    let request = plist::decode(&write_request(&store, Path::new("/c"), &ops).expect("request")).expect("decode");
    assert_eq!(
        request,
        Plist::Dict(vec![
            ("domain".into(), Plist::String(APP.into())),
            ("container".into(), Plist::String("/c".into())),
            ("set".into(), Plist::Dict(vec![("f".into(), Plist::Real(1.5)), ("a".into(), Plist::Array(vec![Plist::Integer(1)]))])),
            ("delete".into(), Plist::Array(vec![Plist::String("gone".into())])),
        ])
    );
}

#[test]
fn the_helpers_read_reply_is_decoded_and_its_errors_said() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(EVERY_TYPE);
    assert_eq!(parse_read_reply(&format!("ok {b64}\n")).expect("ok").len(), 9);
    assert_eq!(parse_read_reply("error this runtime has no X\n").expect_err("err"), "cfprefsd: this runtime has no X");
    assert_eq!(parse_read_reply("").expect_err("empty"), "cfprefsd: the helper said nothing");
}

#[test]
fn a_device_is_booted_by_its_device_plist() {
    let (root, _) = device_set("booted");
    let dev = root.join(UDID);
    assert!(booted(&dev));
    let shutdown = std::fs::read_to_string(dev.join("device.plist")).expect("read").replace("<integer>3</integer>", "<integer>1</integer>");
    std::fs::write(dev.join("device.plist"), shutdown).expect("write");
    assert!(!booted(&dev));
    let binary = plist::encode(&Plist::Dict(vec![("state".into(), Plist::Integer(3))])).expect("encode");
    std::fs::write(dev.join("device.plist"), binary).expect("write");
    assert!(booted(&dev), "a binary device.plist");
    assert!(!booted(&root.join("gone")), "a deleted device");
    std::fs::remove_dir_all(root).ok();
}
