use std::collections::BTreeMap;

/// Parses the `key=value` / `key = value` files the SDK writes. Comments (`#`, `;`) and blank
/// lines are skipped; later keys win.
pub fn parse_ini(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with(';'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_styles() {
        let m = parse_ini("# c\nhw.ramSize=2G\nhw.lcd.width = 1080\n\nbad line\nk=a=b\n");
        assert_eq!(m.get("hw.ramSize").map(String::as_str), Some("2G"));
        assert_eq!(m.get("hw.lcd.width").map(String::as_str), Some("1080"));
        assert_eq!(m.get("k").map(String::as_str), Some("a=b"));
        assert_eq!(m.len(), 3);
    }
}
