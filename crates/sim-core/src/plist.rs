//! Flat XML property lists: one dictionary of strings, integers, reals and booleans, the shape of
//! the simulator's small settings files (a device's `audiosettings.plist`). Read and rewritten in
//! process, so a settings change spawns no `plutil`.

/// One value of a flat dictionary.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
    String(String),
    Integer(i64),
    Real(f64),
    Bool(bool),
}

/// The entries of `text` (key, value), in file order; `None` when it isn't an XML property list
/// holding one dictionary of scalars (a binary plist, nested values, anything else), so a caller
/// never rewrites a file it couldn't read whole.
pub fn parse_dict(text: &str) -> Option<Vec<(String, Scalar)>> {
    let body = text.split_once("<plist")?.1.split_once('>')?.1;
    let body = body.rsplit_once("</plist>")?.0.trim();
    if body == "<dict/>" {
        return Some(Vec::new());
    }
    let mut rest = body.strip_prefix("<dict>")?.strip_suffix("</dict>")?.trim_start();
    let mut out = Vec::new();
    while !rest.is_empty() {
        let (key, after) = rest.strip_prefix("<key>")?.split_once("</key>")?;
        let (value, after) = scalar(after.trim_start())?;
        out.push((crate::launchd::unescape(key)?, value));
        rest = after.trim_start();
    }
    Some(out)
}

/// The value at the start of `text`, and what follows it.
fn scalar(text: &str) -> Option<(Scalar, &str)> {
    if let Some(after) = text.strip_prefix("<true/>") {
        return Some((Scalar::Bool(true), after));
    }
    if let Some(after) = text.strip_prefix("<false/>") {
        return Some((Scalar::Bool(false), after));
    }
    if let Some(after) = text.strip_prefix("<string/>") {
        return Some((Scalar::String(String::new()), after));
    }
    let tag = text.strip_prefix('<')?.split_once('>')?.0;
    let (inner, after) = text[tag.len() + 2..].split_once(&format!("</{tag}>"))?;
    let value = match tag {
        "string" => Scalar::String(crate::launchd::unescape(inner)?),
        "integer" => Scalar::Integer(inner.trim().parse().ok()?),
        "real" => Scalar::Real(inner.trim().parse().ok()?),
        _ => return None,
    };
    Some((value, after))
}

/// Sets `key` to `value` (added at the end when new).
pub fn set(entries: &mut Vec<(String, Scalar)>, key: &str, value: Scalar) {
    match entries.iter_mut().find(|(k, _)| k == key) {
        Some(e) => e.1 = value,
        None => entries.push((key.to_owned(), value)),
    }
}

/// `entries` as the XML property list `plutil` and CoreFoundation write: keys sorted, one tab of
/// indent.
pub fn render_dict(entries: &[(String, Scalar)]) -> String {
    let mut sorted: Vec<&(String, Scalar)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    for (key, value) in sorted {
        let value = match value {
            Scalar::String(v) => format!("<string>{}</string>", crate::launchd::escape(v)),
            Scalar::Integer(v) => format!("<integer>{v}</integer>"),
            Scalar::Real(v) => format!("<real>{v}</real>"),
            Scalar::Bool(v) => format!("<{v}/>"),
        };
        s.push_str(&format!("\t<key>{}</key>\n\t{value}\n", crate::launchd::escape(key)));
    }
    s.push_str("</dict>\n</plist>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device's `var/run/simulatoraudio/audiosettings.plist` as the iOS 27.2 runtime wrote it.
    const AUDIO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>sim_input_device_uid</key>
	<string>BuiltInMicrophoneDevice</string>
	<key>sim_output_device_uid</key>
	<string>BuiltInSpeakerDevice</string>
	<key>sim_ringer_state</key>
	<integer>1</integer>
	<key>sim_volume</key>
	<integer>60</integer>
</dict>
</plist>
"#;

    #[test]
    fn the_runtimes_audio_settings_are_read_and_written_back_unchanged() {
        let e = parse_dict(AUDIO).expect("parsed");
        assert_eq!(
            e,
            [
                ("sim_input_device_uid".to_owned(), Scalar::String("BuiltInMicrophoneDevice".into())),
                ("sim_output_device_uid".to_owned(), Scalar::String("BuiltInSpeakerDevice".into())),
                ("sim_ringer_state".to_owned(), Scalar::Integer(1)),
                ("sim_volume".to_owned(), Scalar::Integer(60)),
            ]
        );
        assert_eq!(render_dict(&e), AUDIO, "byte for byte what the runtime (CoreFoundation) wrote");
    }

    #[test]
    fn a_value_is_replaced_or_added_and_keys_are_written_sorted() {
        let mut e = parse_dict(AUDIO).expect("parsed");
        set(&mut e, "sim_volume", Scalar::Integer(40));
        set(&mut e, "a_new <key>", Scalar::String("x & y".into()));
        set(&mut e, "gain", Scalar::Real(0.5));
        set(&mut e, "on", Scalar::Bool(true));
        let out = render_dict(&e);
        assert!(out.contains("\t<key>sim_volume</key>\n\t<integer>40</integer>\n"), "{out}");
        assert!(out.contains("<key>a_new &lt;key&gt;</key>\n\t<string>x &amp; y</string>"), "{out}");
        let back = parse_dict(&out).expect("round trip");
        assert_eq!(back[0], ("a_new <key>".to_owned(), Scalar::String("x & y".into())));
        assert!(back.contains(&("gain".to_owned(), Scalar::Real(0.5))) && back.contains(&("on".to_owned(), Scalar::Bool(true))));
        assert_eq!(back.len(), 7);
    }

    #[test]
    fn only_a_flat_xml_dictionary_of_scalars_is_read() {
        assert_eq!(parse_dict("<plist version=\"1.0\"><dict/></plist>"), Some(vec![]));
        assert_eq!(parse_dict("<plist><dict><key>a</key><string/><key>b</key><false/></dict></plist>").map(|e| e.len()), Some(2));
        assert_eq!(parse_dict("bplist00\u{1}\u{2}"), None, "binary");
        assert_eq!(parse_dict("<plist><dict><key>a</key><array/></dict></plist>"), None, "nested");
        assert_eq!(parse_dict("<plist><dict><key>a</key><dict/></dict></plist>"), None, "nested");
        assert_eq!(parse_dict("<plist><dict><key>a</key><integer>x</integer></dict></plist>"), None);
        assert_eq!(parse_dict("<plist><dict><key>a</key></dict></plist>"), None, "a key without a value");
        assert_eq!(parse_dict("<plist><array/></plist>"), None);
    }
}
