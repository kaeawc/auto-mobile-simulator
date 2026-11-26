//! logcat filter specs (`tag:priority ... *:priority`, what `-logcat` and the guest's
//! `androidboot.logcat` take) as logcat reads them: a tag's own rule wins, then `*`'s, and with
//! no `*` every priority shows.
//!
//! emu reads a few tags from the guest's log for itself (the display's rotation,
//! [`crate::rotation::ROTATION_TAGS`]): [`with_tags`] widens the guest's filter by them, and
//! [`Filter::passes`] keeps the lines only emu wanted out of what the user asked to see.

/// A logcat filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    /// Each tag's priority, in the order given (the last rule for a tag wins).
    tags: Vec<(String, char)>,
    /// `*`'s priority; `V` when the spec has none.
    default: char,
}

/// logcat's priorities, most verbose first; `S` (silent) shows nothing.
const PRIORITIES: &str = "VDIWEFS";

fn rank(p: char) -> usize {
    PRIORITIES.find(p.to_ascii_uppercase()).unwrap_or(0)
}

impl Filter {
    /// Parses `spec` (whitespace- or comma-separated `tag[:priority]`; a bare tag is `V`).
    /// Unknown priorities read as `V`, as logcat would show the tag then.
    pub fn parse(spec: &str) -> Filter {
        let mut f = Filter { tags: Vec::new(), default: 'V' };
        for rule in spec.split([' ', ',', '\t']).filter(|r| !r.is_empty()) {
            let (tag, p) = rule.rsplit_once(':').unwrap_or((rule, "V"));
            let p = p.chars().next().map(|c| c.to_ascii_uppercase()).filter(|c| PRIORITIES.contains(*c)).unwrap_or('V');
            if tag == "*" {
                f.default = p;
            } else {
                f.tags.retain(|(t, _)| t != tag);
                f.tags.push((tag.to_owned(), p));
            }
        }
        f
    }

    /// The least priority `tag` shows at (`S`: not at all).
    pub fn priority(&self, tag: &str) -> char {
        self.tags.iter().find(|(t, _)| t == tag).map_or(self.default, |(_, p)| *p)
    }

    /// Whether a message of `tag` at `priority` shows.
    pub fn shows(&self, tag: &str, priority: char) -> bool {
        let least = self.priority(tag);
        least != 'S' && rank(priority) >= rank(least)
    }

    /// Whether logcat line `line` (`threadtime`: `date time pid tid P tag: message`) shows;
    /// a line in any other shape does (it's passed on as it came).
    pub fn passes(&self, line: &str) -> bool {
        match parse_line(line) {
            Some((priority, tag, _)) => self.shows(tag, priority),
            None => true,
        }
    }

    /// The filter as a spec logcat takes.
    pub fn spec(&self) -> String {
        let mut rules: Vec<String> = self.tags.iter().map(|(t, p)| format!("{t}:{p}")).collect();
        rules.push(format!("*:{}", self.default));
        rules.join(" ")
    }
}

/// `user`'s filter widened so `tags` show at least at their priority: what the guest is told to
/// log, so emu sees those lines whatever the user asked for.
pub fn with_tags(user: &Filter, tags: &[(&str, char)]) -> Filter {
    let mut f = user.clone();
    for &(tag, p) in tags {
        if !f.shows(tag, p) {
            f.tags.retain(|(t, _)| t != tag);
            f.tags.push((tag.to_owned(), p));
        }
    }
    f
}

/// A `threadtime` line's priority, tag and message (without the line's end).
pub fn parse_line(line: &str) -> Option<(char, &str, &str)> {
    let mut rest = line.trim_start();
    // Date, time, pid, tid.
    for _ in 0..4 {
        let end = rest.find(char::is_whitespace)?;
        rest = rest[end..].trim_start();
    }
    let mut chars = rest.chars();
    let priority = chars.next().filter(|c| PRIORITIES.contains(*c))?;
    if !chars.next().is_some_and(char::is_whitespace) {
        return None;
    }
    let rest = chars.as_str().trim_start();
    let colon = rest.find(':')?;
    let message = rest[colon + 1..].strip_prefix(' ').unwrap_or(&rest[colon + 1..]).trim_end_matches(['\r', '\n']);
    Some((priority, rest[..colon].trim_end(), message))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WM: &str = "11-26 09:58:57.210   642   680 I WindowManager: Display id=0 rotation changed to 3 from 0";
    const APP: &str = "11-26 09:58:57.211  4242  4242 D MyApp   : hello: world";

    #[test]
    fn filters_read_as_logcat_reads_them() {
        let f = Filter::parse("MyApp:D *:S");
        assert!(f.shows("MyApp", 'D') && f.shows("MyApp", 'E') && !f.shows("MyApp", 'V'));
        assert!(!f.shows("WindowManager", 'I'), "silent by default");
        let all = Filter::parse("");
        assert!(all.shows("Anything", 'V'), "no spec shows everything");
        let some = Filter::parse("Noisy:S,Quiet:W");
        assert!(some.shows("Other", 'V'), "no `*`: the rest at V");
        assert!(!some.shows("Noisy", 'F') && some.shows("Quiet", 'E') && !some.shows("Quiet", 'I'));
        assert_eq!(Filter::parse("A B:x C:e").spec(), "A:V B:V C:E *:V");
        assert_eq!(Filter::parse("A:W A:D").priority("A"), 'D', "the last rule wins");
        assert_eq!(Filter::parse("*:S").spec(), "*:S");
    }

    #[test]
    fn the_guest_logs_what_emu_reads_and_the_user_sees_only_theirs() {
        let tags = [("WindowManager", 'I'), ("ActivityTaskManager", 'I')];
        // emu's default (no -logcat): silent but for the rotation tags.
        let silent = Filter::parse("*:S");
        let guest = with_tags(&silent, &tags);
        assert_eq!(guest.spec(), "WindowManager:I ActivityTaskManager:I *:S");
        assert!(guest.passes(WM) && !guest.passes(APP));
        // A user's filter keeps its own rules; the rotation tags are added only where it would
        // hide them, and the user's output drops what was added for emu.
        let user = Filter::parse("MyApp:D WindowManager:E *:S");
        let guest = with_tags(&user, &tags);
        assert_eq!(guest.spec(), "MyApp:D WindowManager:I ActivityTaskManager:I *:S");
        assert!(guest.passes(WM) && !user.passes(WM) && user.passes(APP));
        // Everything already includes them: nothing changes.
        let verbose = Filter::parse("*:V");
        assert_eq!(with_tags(&verbose, &tags), verbose);
        assert_eq!(with_tags(&Filter::parse("*:I"), &tags).spec(), "*:I");
    }

    #[test]
    fn lines_in_other_shapes_pass() {
        let f = Filter::parse("*:S");
        assert_eq!(parse_line(WM), Some(('I', "WindowManager", "Display id=0 rotation changed to 3 from 0")));
        assert_eq!(parse_line(APP), Some(('D', "MyApp", "hello: world")));
        assert_eq!(parse_line(&format!("{WM}\r\n")).map(|l| l.2), Some("Display id=0 rotation changed to 3 from 0"));
        for odd in ["--------- beginning of main", "", "W/Tag( 12): brief format", "1 2 3 4 Q Tag: x"] {
            assert!(f.passes(odd), "{odd}");
        }
    }
}
