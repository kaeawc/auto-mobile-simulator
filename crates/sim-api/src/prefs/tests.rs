use serde_json::{Value, json};
use sim_core::platform::Platform;

use super::fake::{Fake, unique};
use super::*;
use crate::methods::{self, METHODS, call as api};

/// A session with one Android device, `pixel` (an AVD, not running: the fake backend needs none),
/// whose preferences are `fake`'s; `app` is unique to the test (feeds are per process).
fn session(tag: &str) -> (methods::Session, Fake, String, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("sim-api-prefs-{}", unique(tag)));
    let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
    std::fs::create_dir_all(&image).expect("image dir");
    std::fs::write(image.join("system.img"), b"s").expect("system.img");
    std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
    let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
    let mut android = sim_android::Android::new(env);
    android.running_dir = Some(root.join("running"));
    android.create("pixel", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
    let app = unique("com.example.app");
    let fake = Fake::default()
        .with_store(
            Store::new(StoreKind::SharedPreferences, &app, &format!("{app}_preferences")),
            [("name".to_owned(), PrefValue::String("Ada".into())), ("launches".to_owned(), PrefValue::Int(3))].into(),
        )
        .with_store(
            Store::new(StoreKind::DataStore, &app, "settings"),
            [("tags".to_owned(), PrefValue::StringSet(["a".to_owned()].into()))].into(),
        );
    let mut s = methods::Session::with_android(android);
    s.prefs.android = Box::new(fake.clone());
    (s, fake, app, root)
}

fn ok(s: &mut methods::Session, method: &str, p: Value) -> Value {
    api(s, method, &p).unwrap_or_else(|e| panic!("{method} {p}: {e}"))
}

#[test]
fn help_lists_every_prefs_method_on_both_platforms() {
    let help = ok(&mut methods::Session::default(), "help", Value::Null);
    for m in super::METHODS {
        let row = help.as_array().and_then(|a| a.iter().find(|x| x["method"] == *m)).unwrap_or_else(|| panic!("{m} in help"));
        assert_eq!(row["platforms"], json!(["android", "ios"]), "{m}");
        assert!(METHODS.iter().any(|(name, ..)| name == m));
    }
}

#[test]
fn stores_have_ids_the_other_methods_take() {
    let parse = |platform, id: &str| Store::parse(platform, "com.example", id);
    let shared = parse(Platform::Android, "shared_prefs/com.example_preferences.xml").expect("shared");
    assert_eq!((shared.kind, shared.name.as_str()), (StoreKind::SharedPreferences, "com.example_preferences"));
    assert_eq!(shared.id(), "shared_prefs/com.example_preferences.xml");
    let ds = parse(Platform::Android, "datastore/settings.preferences_pb").expect("datastore");
    assert_eq!((ds.kind, ds.id()), (StoreKind::DataStore, "datastore/settings.preferences_pb".to_owned()));
    for bad in ["settings", "shared_prefs/.xml", "shared_prefs/a/b.xml", "shared_prefs/../x.xml", "datastore/x.pb", "com.example"] {
        let err = parse(Platform::Android, bad).expect_err(bad);
        assert!(err.starts_with(&format!("`{bad}` isn't an Android preferences store")), "{err}");
    }
    assert_eq!(parse(Platform::Ios, "com.example").expect("app").kind, StoreKind::AppDomain);
    assert_eq!(parse(Platform::Ios, "group.com.example.shared").expect("group").kind, StoreKind::AppGroup);
    assert_eq!(parse(Platform::Ios, "com.apple.Preferences").expect("named").kind, StoreKind::Domain);
    assert!(parse(Platform::Ios, "../etc").is_err() && parse(Platform::Ios, "").is_err());
    assert_eq!(
        shared.to_json(),
        json!({"id": "shared_prefs/com.example_preferences.xml", "kind": "shared_preferences", "name": "com.example_preferences",
            "app": "com.example", "platform": "android"})
    );
}

#[test]
fn a_store_refuses_types_it_cannot_hold() {
    let shared = StoreKind::SharedPreferences;
    assert!(shared.check(&PrefValue::Float(1.0), "value").is_ok());
    assert_eq!(
        shared.check(&PrefValue::Double(1.0), "value").expect_err("double"),
        "`value` is a `double`, which Android SharedPreferences can't hold (it holds string, int, long, float, bool, string_set)"
    );
    assert!(StoreKind::DataStore.check(&PrefValue::Double(1.0), "value").is_ok());
    assert!(StoreKind::DataStore.check(&PrefValue::Date(Date::from_unix(0, 0)), "value").is_err());
    assert!(StoreKind::AppDomain.check(&PrefValue::StringSet(Default::default()), "value").is_err());
    // Nested values are checked too: an iOS array can't hold an Android string set.
    let nested = PrefValue::Array(vec![PrefValue::Int(1), PrefValue::StringSet(Default::default())]);
    assert!(StoreKind::AppGroup.check(&nested, "value").expect_err("nested").contains("`string_set`"));
    assert!(StoreKind::Domain.check(&PrefValue::Array(vec![PrefValue::Data(vec![1])]), "value").is_ok());
}

#[test]
fn stores_and_reads_come_back_typed() {
    let (mut s, _fake, app, root) = session("read");
    let stores = ok(&mut s, "prefs_stores", json!({"device": "pixel", "app": app}));
    let ids: Vec<&str> = stores.as_array().expect("array").iter().filter_map(|st| st["id"].as_str()).collect();
    let shared_id = format!("shared_prefs/{app}_preferences.xml");
    assert_eq!(ids, [shared_id.as_str(), "datastore/settings.preferences_pb"]);

    let all = ok(&mut s, "prefs_read", json!({"device": "pixel", "app": app}));
    assert_eq!(all.as_array().map(Vec::len), Some(2));
    assert_eq!(
        all[0]["values"],
        json!({"launches": {"type": "int", "value": 3}, "name": {"type": "string", "value": "Ada"}}),
        "every key with its type"
    );
    assert_eq!(all[1]["values"]["tags"], json!({"type": "string_set", "value": ["a"]}));
    let one = ok(&mut s, "prefs_read", json!({"device": "pixel", "app": app, "store": "datastore/settings.preferences_pb"}));
    assert_eq!((one.as_array().map(Vec::len), one[0]["store"]["kind"].clone()), (Some(1), json!("datastore")));
    let err = api(&mut s, "prefs_read", &json!({"device": "pixel", "app": app, "store": 3})).expect_err("not a string");
    assert_eq!(err, "`store` must be a string, not 3");
    assert_eq!(api(&mut s, "prefs_read", &json!({"device": "pixel"})).expect_err("no app"), "missing string parameter `app`");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn writes_are_typed_and_checked_before_the_backend_sees_them() {
    let (mut s, fake, app, root) = session("write");
    let store_id = format!("shared_prefs/{app}_preferences.xml");
    let store = Store::parse(Platform::Android, &app, &store_id).expect("store");
    let write = |s: &mut methods::Session, key: &str, ty: &str, value: Value| {
        api(s, "prefs_write", &json!({"device": "pixel", "app": app, "store": store_id, "key": key, "type": ty, "value": value}))
    };
    assert_eq!(
        write(&mut s, "launches", "long", json!(4_000_000_000_i64)),
        Ok(json!({"store": store_id, "key": "launches", "visible": "now"}))
    );
    assert_eq!(fake.entries(&store).expect("store")["launches"], PrefValue::Long(4_000_000_000));
    write(&mut s, "ratio", "float", json!(0.25)).expect("float");
    write(&mut s, "seen", "string_set", json!(["b", "a"])).expect("set");

    let before = fake.entries(&store);
    let refused = [
        ("int", json!(1.5), "`value` must be a whole number"),
        ("int", json!("3"), "`value` must be a whole number"),
        ("bool", json!(1), "`value` must be true or false"),
        ("double", json!(1.5), "which Android SharedPreferences can't hold"),
        ("date", json!("2026-11-27T10:00:00Z"), "which Android SharedPreferences can't hold"),
        ("dictionary", json!({}), "which Android SharedPreferences can't hold"),
        ("number", json!(1), "unknown type `number`"),
    ];
    for (ty, value, want) in refused {
        let err = write(&mut s, "k", ty, value.clone()).expect_err(ty);
        assert!(err.contains(want), "{ty} {value}: {err}");
    }
    assert_eq!(fake.entries(&store), before, "nothing refused reached the store");
    let err = api(&mut s, "prefs_write", &json!({"device": "pixel", "app": app, "store": store_id, "key": "k", "type": "int"}));
    assert_eq!(err.expect_err("no value"), "missing parameter `value`");

    // A backend that can only reach the app after a relaunch says so.
    fake.reports(WriteResult { visible: Visibility::AfterRelaunch, note: Some("the app is running: relaunch it".into()) });
    let done = write(&mut s, "name", "string", json!("Grace")).expect("write");
    assert_eq!((done["visible"].clone(), done["note"].clone()), (json!("after_relaunch"), json!("the app is running: relaunch it")));

    let gone = ok(&mut s, "prefs_delete", json!({"device": "pixel", "app": app, "store": store_id, "key": "name"}));
    assert_eq!(gone["key"], "name");
    assert!(!fake.entries(&store).expect("store").contains_key("name"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_batch_is_all_or_nothing() {
    let (mut s, fake, app, root) = session("batch");
    let store_id = "datastore/settings.preferences_pb";
    let store = Store::parse(Platform::Android, &app, store_id).expect("store");
    let batch = |s: &mut methods::Session, ops: Value| {
        api(s, "prefs_batch", &json!({"device": "pixel", "app": app, "store": store_id, "ops": ops}))
    };
    let done = batch(
        &mut s,
        json!([{"key": "volume", "type": "double", "value": 0.5}, {"key": "tags", "delete": true}, {"key": "count", "type": "int", "value": 7}]),
    )
    .expect("batch");
    assert_eq!(done, json!({"store": store_id, "keys": ["volume", "tags", "count"], "visible": "now"}));
    let after = fake.entries(&store).expect("store");
    assert_eq!(after, [("count".to_owned(), PrefValue::Int(7)), ("volume".to_owned(), PrefValue::Double(0.5))].into());

    // One bad op refuses the whole batch before anything is written.
    let refused = [
        (
            json!([{"key": "a", "type": "int", "value": 1}, {"key": "b", "type": "int", "value": "x"}]),
            "`ops[1].value` must be a whole number",
        ),
        (json!([{"key": "a", "type": "int", "value": 1}, {"key": "a", "delete": true}]), "`ops` names `a` twice"),
        (json!([{"key": "a", "type": "date", "value": "2026-11-27T10:00:00Z"}]), "can't hold"),
        (json!([{"key": "a", "delete": true, "value": 1}]), "takes no `type` or `value`"),
        (json!([{"key": "a", "type": "int"}]), "give a string `type` and a `value`"),
        (json!([{"type": "int", "value": 1}]), "`ops[0]` has no string `key`"),
        (json!([{"key": "a", "delete": "yes"}]), "`ops[0].delete` must be true or false"),
        (json!([{"key": "a", "type": "int", "value": 1, "extra": 1}]), "`ops[0]` has `extra`"),
        (json!(["a"]), "`ops[0]` must be"),
        (json!([]), "`ops` is empty"),
        (json!({"key": "a"}), "`ops` must be an array"),
    ];
    for (ops, want) in refused {
        let err = batch(&mut s, ops.clone()).expect_err(&ops.to_string());
        assert!(err.contains(want), "{ops}: {err}");
        assert_eq!(fake.entries(&store).as_ref(), Some(&after), "{ops} wrote something");
    }
    // A backend failing part-way changes nothing either.
    fake.fail_on("b");
    let err = batch(&mut s, json!([{"key": "a", "type": "int", "value": 1}, {"key": "b", "type": "int", "value": 2}])).expect_err("fails");
    assert_eq!(err, "the fake failed writing `b`");
    assert_eq!(fake.entries(&store), Some(after));
    let err = api(&mut s, "prefs_batch", &json!({"device": "pixel", "app": app, "store": store_id})).expect_err("no ops");
    assert_eq!(err, "missing array parameter `ops`");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_watch_feed_reports_changes_in_order_with_old_and_new() {
    let (mut s, fake, app, root) = session("watch");
    let store_id = format!("shared_prefs/{app}_preferences.xml");
    let watch = |s: &mut methods::Session, extra: Value| {
        let mut p = json!({"device": "pixel", "app": app});
        p.as_object_mut().expect("object").extend(extra.as_object().cloned().unwrap_or_default());
        ok(s, "prefs_watch", p)
    };
    let first = watch(&mut s, json!({}));
    assert_eq!(first["events"], json!([]), "the first call is the baseline");
    let cursor = first["cursor"].as_str().expect("cursor").to_owned();
    assert_eq!(watch(&mut s, json!({"cursor": cursor}))["events"], json!([]), "nothing changed");

    let set = |s: &mut methods::Session, key: &str, ty: &str, value: Value| {
        ok(s, "prefs_write", json!({"device": "pixel", "app": app, "store": store_id, "key": key, "type": ty, "value": value}));
    };
    set(&mut s, "launches", "int", json!(4));
    set(&mut s, "fresh", "bool", json!(true));
    set(&mut s, "launches", "int", json!(4));
    ok(&mut s, "prefs_delete", json!({"device": "pixel", "app": app, "store": store_id, "key": "name"}));
    ok(
        &mut s,
        "prefs_write",
        json!({"device": "pixel", "app": app, "store": "datastore/settings.preferences_pb", "key": "x", "type": "long", "value": 1}),
    );

    let got = watch(&mut s, json!({"cursor": cursor}));
    let events = got["events"].as_array().expect("events");
    let rows: Vec<(String, String, Value, Value)> = events
        .iter()
        .map(|e| {
            (e["store"].as_str().unwrap_or("").to_owned(), e["key"].as_str().unwrap_or("").to_owned(), e["old"].clone(), e["new"].clone())
        })
        .collect();
    let int = |n: i64| json!({"type": "int", "value": n});
    assert_eq!(
        rows,
        [
            (store_id.clone(), "launches".to_owned(), int(3), int(4)),
            (store_id.clone(), "fresh".to_owned(), Value::Null, json!({"type": "bool", "value": true})),
            (store_id.clone(), "name".to_owned(), json!({"type": "string", "value": "Ada"}), Value::Null),
            ("datastore/settings.preferences_pb".to_owned(), "x".to_owned(), Value::Null, json!({"type": "long", "value": 1})),
        ],
        "in order, an unchanged write reports nothing, every store of the app"
    );
    let ats: Vec<&str> = events.iter().filter_map(|e| e["at"].as_str()).collect();
    assert_eq!(ats[0], "2027-01-15T08:00:00Z", "ISO 8601");
    assert!(ats.windows(2).all(|w| Date::parse_iso8601(w[0]).ok() < Date::parse_iso8601(w[1]).ok()));
    let next = got["cursor"].as_str().expect("cursor").to_owned();
    assert_eq!(watch(&mut s, json!({"cursor": next}))["events"], json!([]), "read once, past the cursor");
    // An old cursor reads the same changes again (while the feed keeps them).
    assert_eq!(watch(&mut s, json!({"cursor": cursor}))["events"].as_array().map(Vec::len), Some(4));

    // One store's feed is its own, and sees only that store.
    let one = watch(&mut s, json!({"store": "datastore/settings.preferences_pb"}));
    set(&mut s, "launches", "int", json!(5));
    ok(
        &mut s,
        "prefs_write",
        json!({"device": "pixel", "app": app, "store": "datastore/settings.preferences_pb", "key": "x", "type": "long", "value": 2}),
    );
    let only = watch(&mut s, json!({"store": "datastore/settings.preferences_pb", "cursor": one["cursor"]}));
    assert_eq!(only["events"].as_array().map(|e| e.iter().map(|e| e["key"].clone()).collect::<Vec<_>>()), Some(vec![json!("x")]));

    // A cursor that isn't one, or one of a feed that has gone, starts over.
    assert_eq!(watch(&mut s, json!({"cursor": "999999:1"}))["reset"], true);
    assert_eq!(watch(&mut s, json!({"cursor": format!("{next}9")}))["reset"], true, "past the feed's head");
    let err = api(&mut s, "prefs_watch", &json!({"device": "pixel", "app": app, "wait_ms": -1})).expect_err("wait");
    assert!(err.starts_with("`wait_ms` must be a whole number"), "{err}");

    // The device stopping closes the feed: what was sent is still read, then `ended`, then a new feed.
    let last = watch(&mut s, json!({}))["cursor"].as_str().expect("cursor").to_owned();
    set(&mut s, "launches", "int", json!(6));
    fake.close_feeds();
    let ended = watch(&mut s, json!({"cursor": last}));
    assert_eq!((ended["events"].as_array().map(Vec::len), ended["ended"].clone()), (Some(1), json!(true)));
    let again = watch(&mut s, json!({"cursor": ended["cursor"]}));
    assert_eq!((again["reset"].clone(), again.get("ended")), (json!(true), None), "a new feed");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_watch_waits_for_the_next_change() {
    let (mut s, fake, app, root) = session("wait");
    let store = Store::parse(Platform::Android, &app, "datastore/settings.preferences_pb").expect("store");
    let cursor = ok(&mut s, "prefs_watch", json!({"device": "pixel", "app": app}))["cursor"].clone();
    let start = std::time::Instant::now();
    let quiet = ok(&mut s, "prefs_watch", json!({"device": "pixel", "app": app, "cursor": cursor, "wait_ms": 50}));
    assert_eq!(quiet["events"], json!([]));
    assert!(start.elapsed() >= std::time::Duration::from_millis(50), "{:?}", start.elapsed());
    // Another tool (the app) writes while the call waits.
    let mut other = fake.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        other.write("pixel", &store, "late", &PrefValue::Bool(true)).expect("write");
    });
    let start = std::time::Instant::now();
    let woke = ok(&mut s, "prefs_watch", json!({"device": "pixel", "app": app, "cursor": quiet["cursor"], "wait_ms": 10_000}));
    writer.join().expect("writer");
    assert_eq!(woke["events"][0]["key"], "late");
    assert!(start.elapsed() < std::time::Duration::from_secs(5), "answered when the change came: {:?}", start.elapsed());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_feed_keeps_the_latest_changes_and_counts_the_ones_it_dropped() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut feed = Feed::new(rx);
    let cursor = feed.poll(None, Duration::ZERO)["cursor"].as_str().expect("cursor").to_owned();
    let store = Store::new(StoreKind::AppDomain, "com.example", "com.example");
    for i in 0..FEED_EVENTS + 5 {
        let at = Date::from_unix(i as i64, 0);
        tx.send(Change { store: store.clone(), key: format!("k{i}"), old: None, new: Some(PrefValue::Int(1)), at }).expect("send");
    }
    let got = feed.poll(Some(&cursor), Duration::ZERO);
    assert_eq!((got["events"].as_array().map(Vec::len), got["missed"].clone()), (Some(FEED_EVENTS), json!(5)));
    assert_eq!(got["events"][0]["key"], "k5");
}

#[test]
fn an_idle_feed_stops_and_its_backend_stops_sending() {
    let fake = Fake::default();
    let mut backend = fake.clone();
    let store = Store::new(StoreKind::AppDomain, "com.example", "com.example");
    let mut feeds = Feeds::new(Duration::ZERO);
    let key = FeedKey { platform: Platform::Ios, device: "d".into(), app: "com.example".into(), store: None };
    let first = feeds.get(key.clone(), || backend.watch("d", "com.example", None)).expect("feed");
    let id = lock(&first).id;
    drop(first);
    let second = feeds.get(key, || backend.watch("d", "com.example", None)).expect("feed");
    assert_ne!(lock(&second).id, id, "the idle one stopped and a new one started");
    // The stopped feed's receiver is gone: the backend drops its sender on the next change.
    assert_eq!(fake.watchers(), 2);
    backend.write("d", &store, "k", &PrefValue::Int(1)).expect("write");
    assert_eq!(fake.watchers(), 1);
}

#[test]
fn changes_between_two_reads_are_by_key() {
    let store = Store::new(StoreKind::SharedPreferences, "a", "a");
    let before: Entries =
        [("same".into(), PrefValue::Int(1)), ("gone".into(), PrefValue::Bool(true)), ("moved".into(), PrefValue::Int(1))].into();
    let after: Entries =
        [("same".into(), PrefValue::Int(1)), ("moved".into(), PrefValue::Long(1)), ("new".into(), PrefValue::Float(0.5))].into();
    let at = Date::from_unix(5, 0);
    let got: Vec<(String, Option<PrefValue>, Option<PrefValue>)> =
        changes(&store, &before, &after, at).into_iter().map(|c| (c.key, c.old, c.new)).collect();
    assert_eq!(
        got,
        [
            ("gone".to_owned(), Some(PrefValue::Bool(true)), None),
            ("moved".to_owned(), Some(PrefValue::Int(1)), Some(PrefValue::Long(1))),
            ("new".to_owned(), None, Some(PrefValue::Float(0.5))),
        ],
        "an int that became a long is a change"
    );
}

#[test]
fn devices_without_a_backend_yet_say_so_after_checking_parameters() {
    let (mut s, _fake, app, root) = session("unavailable");
    s.prefs = Session::default();
    for (method, extra) in [
        ("prefs_stores", json!({})),
        ("prefs_read", json!({})),
        ("prefs_write", json!({"store": "shared_prefs/a.xml", "key": "k", "type": "int", "value": 1})),
        ("prefs_delete", json!({"store": "shared_prefs/a.xml", "key": "k"})),
        ("prefs_batch", json!({"store": "shared_prefs/a.xml", "ops": [{"key": "k", "delete": true}]})),
        ("prefs_watch", json!({})),
    ] {
        let mut p = json!({"device": "pixel", "app": app});
        p.as_object_mut().expect("object").extend(extra.as_object().cloned().unwrap_or_default());
        assert_eq!(
            api(&mut s, method, &p).expect_err(method),
            "app preferences (`prefs_*`) are not yet available on Android devices",
            "{method}"
        );
    }
    // A bad parameter is still reported as that, before the platform's gap.
    let err = api(&mut s, "prefs_write", &json!({"device": "pixel", "app": app, "store": "x", "key": "k", "type": "int", "value": 1}));
    assert!(err.expect_err("bad store").starts_with("`x` isn't an Android preferences store"));
    let mut ios = Unavailable(Platform::Ios);
    let err = call(&mut ios, Platform::Ios, "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000", "prefs_read", &json!({"app": "com.example"}));
    assert_eq!(err.expect_err("ios"), "app preferences (`prefs_*`) are not yet available on iOS devices");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn defaults_read_and_write_are_unchanged() {
    let row = |m: &str| METHODS.iter().find(|(name, ..)| *name == m).copied().unwrap_or_else(|| panic!("{m}"));
    assert_eq!(row("defaults_read"), ("defaults_read", "device, domain", "Read a preferences domain"));
    assert_eq!(
        row("defaults_write"),
        ("defaults_write", "device, domain, key, value, type?", "Write a preference (type: string|int|float|bool)")
    );
    for m in ["defaults_read", "defaults_write"] {
        assert_eq!(methods::platforms(m), [Platform::Ios], "{m}");
    }
    assert_eq!(crate::input_schema("defaults_read").expect("schema")["required"], json!(["device", "domain"]));
    let write = crate::input_schema("defaults_write").expect("schema");
    assert_eq!(write["required"], json!(["device", "domain", "key", "value"]));
    assert_eq!(write["properties"]["type"], json!({"type": "string"}));
    // Still the iOS backend's: on an Android device, refused as before, not by the prefs methods.
    let (mut s, _fake, _app, root) = session("defaults");
    let err = api(&mut s, "defaults_read", &json!({"device": "pixel", "domain": "x"})).expect_err("iOS only");
    assert_eq!(err, "`defaults_read` isn't available on Android devices (it works on iOS)");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_prefs_schemas_type_every_parameter() {
    let write = crate::input_schema("prefs_write").expect("schema");
    assert_eq!(write["required"], json!(["device", "app", "store", "key", "value", "type"]));
    let types: Vec<&str> = Type::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(write["properties"]["type"]["enum"], json!(types), "the grammar names every type");
    assert_eq!(crate::input_schema("prefs_read").expect("schema")["required"], json!(["device", "app"]));
    let batch = crate::input_schema("prefs_batch").expect("schema");
    assert_eq!(batch["required"], json!(["device", "app", "store", "ops"]));
    assert_eq!(batch["properties"]["ops"], json!({"type": "array", "items": {"type": "object"}}));
    let watch = crate::input_schema("prefs_watch").expect("schema");
    assert_eq!(watch["required"], json!(["device", "app"]));
    assert_eq!(
        (watch["properties"]["cursor"]["type"].clone(), watch["properties"]["wait_ms"]["type"].clone()),
        (json!("string"), json!("integer"))
    );
}
