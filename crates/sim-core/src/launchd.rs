//! A simulator's launchd disabled-jobs database, as a file.
//!
//! An iOS simulator's `launchd_sim` keeps `launchctl disable`/`enable` overrides in
//! `/private/var/tmp/com.apple.CoreSimulator.SimDevice.<UDID>/disabled.plist` on the host: an XML
//! property list of label → bool (`true` disabled, `false` enabled), one file for every domain of
//! the device. It reads the file when it starts, so overrides written there before a boot apply
//! to that boot: a job disabled there is never loaded. The file lives outside the device's data
//! directory and is keyed by UDID, which is why a clone (a new UDID) starts with none.
//!
//! CoreSimulator's own boot option for this, `disabled_jobs` (`simctl boot --disabledJob=`),
//! writes the same kind of file under `/private/tmp` instead ([`boot_plist_path`]) and deletes it
//! on a boot without the option. Runtimes differ in which of the two their launchd_sim reads:
//! iOS 18.6 to 27.2 read `/private/var/tmp`, iOS 18.0 reads `/private/tmp` (measured, CoreSimulator
//! 1174.21). launchd_sim writes its `launchd.log` beside the file it uses ([`data_dir_in_use`]).

/// The overrides file of the simulator `udid`.
pub fn disabled_plist_path(udid: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/private/var/tmp/com.apple.CoreSimulator.SimDevice.{udid}/disabled.plist"))
}

/// The overrides file CoreSimulator writes for a boot of `udid` with `disabled_jobs`.
pub fn boot_plist_path(udid: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/private/tmp/com.apple.CoreSimulator.SimDevice.{udid}/disabled.plist"))
}

/// Which of the two overrides directories a booted launchd_sim used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataDir {
    /// `/private/var/tmp/…` ([`disabled_plist_path`]).
    VarTmp,
    /// `/private/tmp/…` ([`boot_plist_path`]).
    Tmp,
}

/// The directory whose `launchd.log` was written since `boot_started` (each log's modification
/// time, `None` when it doesn't exist): the one this boot's launchd_sim used. `None` when neither
/// or both were, so a caller trusts neither file.
pub fn data_dir_in_use(
    var_tmp_log: Option<std::time::SystemTime>,
    tmp_log: Option<std::time::SystemTime>,
    boot_started: std::time::SystemTime,
) -> Option<DataDir> {
    let fresh = |t: Option<std::time::SystemTime>| t.is_some_and(|t| t >= boot_started);
    match (fresh(var_tmp_log), fresh(tmp_log)) {
        (true, false) => Some(DataDir::VarTmp),
        (false, true) => Some(DataDir::Tmp),
        _ => None,
    }
}

/// Whether `entries` holds every one of `labels` disabled (`true`).
pub fn all_disabled(entries: &[(String, bool)], labels: &[&str]) -> bool {
    let off: std::collections::HashSet<&str> = disabled(entries).collect();
    labels.iter().all(|l| off.contains(l))
}

/// `entries` with each of `labels` set to `disabled` (added at the end when new).
pub fn with(entries: &[(String, bool)], labels: &[&str], disabled: bool) -> Vec<(String, bool)> {
    let mut all: Vec<(String, bool)> = entries.to_vec();
    for label in labels {
        match all.iter_mut().find(|(l, _)| l == label) {
            Some(e) => e.1 = disabled,
            None => all.push(((*label).to_owned(), disabled)),
        }
    }
    all
}

/// The overrides in `text` (label, disabled), in file order; `None` when it isn't an XML
/// property list holding one dictionary of booleans (a binary plist, or anything else), so a
/// caller never rewrites a file it couldn't read.
pub fn parse(text: &str) -> Option<Vec<(String, bool)>> {
    let body = text.split_once("<plist")?.1.split_once('>')?.1;
    let body = body.rsplit_once("</plist>")?.0.trim();
    if body == "<dict/>" {
        return Some(Vec::new());
    }
    let mut rest = body.strip_prefix("<dict>")?.strip_suffix("</dict>")?.trim_start();
    let mut out = Vec::new();
    while !rest.is_empty() {
        let (key, after) = rest.strip_prefix("<key>")?.split_once("</key>")?;
        let after = after.trim_start();
        let (value, after) = if let Some(a) = after.strip_prefix("<true/>") { (true, a) } else { (false, after.strip_prefix("<false/>")?) };
        out.push((unescape(key)?, value));
        rest = after.trim_start();
    }
    Some(out)
}

/// `entries` with each of `labels` set to `disabled` (added at the end when new), as the XML
/// property list launchd writes.
pub fn render(entries: &[(String, bool)], labels: &[&str], disabled: bool) -> String {
    let all = with(entries, labels, disabled);
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    for (label, value) in &all {
        s.push_str(&format!("\t<key>{}</key>\n\t<{}/>\n", escape(label), value));
    }
    s.push_str("</dict>\n</plist>\n");
    s
}

/// The labels `entries` holds disabled.
pub fn disabled(entries: &[(String, bool)]) -> impl Iterator<Item = &str> {
    entries.iter().filter(|e| e.1).map(|e| e.0.as_str())
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub(crate) fn unescape(s: &str) -> Option<String> {
    if s.contains('<') {
        return None;
    }
    Some(s.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As the iOS 27.2 runtime's launchd_sim wrote it after `launchctl disable` of two jobs and
    /// nanoregistrylaunchd's `enable` of one.
    const WRITTEN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.weatherd</key>
	<true/>
	<key>com.apple.companionappd</key>
	<false/>
	<key>com.apple.newsd</key>
	<true/>
</dict>
</plist>
"#;

    #[test]
    fn launchds_file_is_read_as_label_and_disabled() {
        let e = parse(WRITTEN).expect("parsed");
        assert_eq!(
            e,
            [("com.apple.weatherd".to_owned(), true), ("com.apple.companionappd".to_owned(), false), ("com.apple.newsd".to_owned(), true)]
        );
        assert_eq!(disabled(&e).collect::<Vec<_>>(), ["com.apple.weatherd", "com.apple.newsd"]);
    }

    #[test]
    fn disabling_keeps_other_overrides_and_flips_an_enabled_one() {
        let e = parse(WRITTEN).expect("parsed");
        let out = render(&e, &["com.apple.companionappd", "com.apple.PosterBoard"], true);
        let back = parse(&out).expect("round trip");
        assert_eq!(
            back,
            [
                ("com.apple.weatherd".to_owned(), true),
                ("com.apple.companionappd".to_owned(), true),
                ("com.apple.newsd".to_owned(), true),
                ("com.apple.PosterBoard".to_owned(), true)
            ]
        );
        let undone = parse(&render(&back, &["com.apple.newsd"], false)).expect("round trip");
        assert_eq!(undone[2], ("com.apple.newsd".to_owned(), false));
    }

    #[test]
    fn an_empty_file_and_a_new_one_are_written_whole() {
        assert_eq!(parse(&render(&[], &[], true)), Some(vec![]));
        assert_eq!(parse("<?xml?><plist version=\"1.0\"><dict/></plist>"), Some(vec![]));
        assert_eq!(parse(&render(&[], &["a&b"], true)), Some(vec![("a&b".to_owned(), true)]));
    }

    #[test]
    fn a_file_that_isnt_a_dict_of_booleans_is_not_read() {
        assert_eq!(parse("bplist00\u{1}\u{2}"), None, "binary");
        assert_eq!(parse("<plist><dict><key>a</key><string>x</string></dict></plist>"), None);
        assert_eq!(parse("<plist><array/></plist>"), None);
        assert_eq!(parse("<plist><dict><key>a</key></dict></plist>"), None, "a key without a value");
    }

    #[test]
    fn the_file_is_keyed_by_udid_outside_the_data_dir() {
        assert_eq!(disabled_plist_path("ABC").to_str(), Some("/private/var/tmp/com.apple.CoreSimulator.SimDevice.ABC/disabled.plist"));
        assert_eq!(boot_plist_path("ABC").to_str(), Some("/private/tmp/com.apple.CoreSimulator.SimDevice.ABC/disabled.plist"));
    }

    #[test]
    fn the_directory_in_use_is_the_one_whose_log_this_boot_wrote() {
        let boot = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        let before = Some(boot - std::time::Duration::from_secs(5));
        let after = Some(boot + std::time::Duration::from_millis(300));
        assert_eq!(data_dir_in_use(after, None, boot), Some(DataDir::VarTmp), "iOS 18.6-27.2");
        assert_eq!(data_dir_in_use(before, after, boot), Some(DataDir::Tmp), "iOS 18.0, an earlier boot's log in the other");
        assert_eq!(data_dir_in_use(None, None, boot), None, "a runtime that logs in neither");
        assert_eq!(data_dir_in_use(before, before, boot), None, "only an earlier boot's logs");
        assert_eq!(data_dir_in_use(after, after, boot), None, "both: can't tell");
    }

    #[test]
    fn every_label_must_be_disabled_for_the_file_to_hold_them() {
        let e = parse(WRITTEN).expect("parsed");
        assert!(all_disabled(&e, &["com.apple.weatherd", "com.apple.newsd"]));
        assert!(all_disabled(&e, &[]));
        assert!(!all_disabled(&e, &["com.apple.weatherd", "com.apple.companionappd"]), "enabled (nanoregistrylaunchd's `false`)");
        assert!(!all_disabled(&e, &["com.apple.weatherd", "com.apple.PosterBoard"]), "missing");
        assert!(!all_disabled(&[], &["com.apple.weatherd"]));
    }

    #[test]
    fn labels_set_over_entries_keep_the_others_in_order() {
        let e = parse(WRITTEN).expect("parsed");
        assert_eq!(
            with(&e, &["com.apple.companionappd", "com.apple.PosterBoard"], true),
            [
                ("com.apple.weatherd".to_owned(), true),
                ("com.apple.companionappd".to_owned(), true),
                ("com.apple.newsd".to_owned(), true),
                ("com.apple.PosterBoard".to_owned(), true)
            ]
        );
    }
}
