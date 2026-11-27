//! JSON Schemas for the method table's parameters, for front ends that need to describe a method's
//! input (the MCP server's tool definitions).
//!
//! [`METHODS`](crate::METHODS) writes each method's parameters as a short grammar:
//!
//! ```text
//! device, x, y, hold_ms?, display?(main|carplay)      names; `?` optional; (a|b) choices
//! name, platform?(ios|android); iOS: device_type      `;` starts a platform's parameters
//! text? | base64?                                      `|` is "one of these"
//! device, level(0-100)                                 (a-b) is a range; (x) is the default
//! ```
//!
//! [`input_schema`] reads that grammar, so a new method (or a new parameter on one) is described
//! the moment it is in the table. A parameter's type comes from [`param_type`], which knows each
//! parameter name; a name it doesn't know has no `type` (anything goes) until one is added there.

use serde_json::{Map, Value, json};

use crate::METHODS;

/// How a parameter name is typed in a schema.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    Str,
    Num,
    Int,
    Bool,
    /// An object whose values are strings (an environment).
    StrMap,
    /// Any object.
    Obj,
    /// An array of objects.
    Objects,
    /// An array of strings.
    Strings,
}

/// The type of the parameter `name` of `method`: the same for every method that takes it, bar the
/// few listed first (`stylus`'s `button` is the barrel button, `button`'s a name).
fn param_type(method: &str, name: &str) -> Option<Ty> {
    use Ty::*;
    Some(match name {
        "button" if method == "stylus" => Bool,
        "devices" | "args" | "commands" => Strings,
        "device" | "platform" | "form_factor" | "name" | "device_type" | "runtime" | "system_image" | "path" | "url" | "bundle_id"
        | "text" | "base64" | "domain" | "key" | "language" | "uid" | "capture_id" | "program" | "command" | "predicate" | "last"
        | "preset" | "direction" | "gesture" | "button" | "phase" | "action" | "service" | "scope" | "display" | "orientation"
        | "posture" | "mode" | "profile" | "type" | "time" | "data_network" | "wifi_mode" | "cellular_mode" | "operator"
        | "battery_state" | "environment" | "press" | "property" | "pattern" => Str,
        "x" | "y" | "z" | "x1" | "y1" | "x2" | "y2" | "to_x" | "to_y" | "latitude" | "longitude" | "delta" | "angle" | "passthrough"
        | "settle_cpu_pct" | "settle_window_s" | "cpu_pct" | "window_s" | "latency_ms" | "down_kbps" | "up_kbps" | "loss_pct" => Num,
        "hold_ms" | "duration_ms" | "timeout_s" | "settle_timeout_s" | "concurrency" | "first_port" | "usage" | "pid" | "signal"
        | "ticks" | "pressure" | "level" | "wifi_bars" | "cellular_bars" | "battery_level" | "state" | "port_base" | "port_count" => Int,
        "booted" | "on" | "suppressed" | "undo" | "shaped" | "settle" | "settle_foreground" | "audio" | "mute" | "sizes" | "offline"
        | "proxy" | "face" | "foreground" | "eraser" | "folded" | "restart" | "stream" => Bool,
        "env" => StrMap,
        "payload" => Obj,
        "rules" => Objects,
        _ => return None,
    })
}

/// One parameter read from a method's grammar.
struct Param {
    name: String,
    optional: bool,
    /// The choices in `name(a|b)`, as JSON values.
    choices: Vec<Value>,
    /// `name(0-100)`.
    range: Option<(i64, i64)>,
    /// `name?(true)`: the one value in parentheses.
    default: Option<Value>,
    /// Other text in the parentheses, as a note.
    note: Option<String>,
    /// `iOS` or `Android`, from the section it was written in.
    only: Option<&'static str>,
}

/// Splits `s` at every `sep` that isn't inside brackets, braces, parentheses or quotes.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut parts, mut depth, mut quoted, mut start) = (Vec::new(), 0i32, false, 0);
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' | '[' | '{' if !quoted => depth += 1,
            ')' | ']' | '}' if !quoted => depth -= 1,
            c if c == sep && depth == 0 && !quoted => {
                parts.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// A choice or default written in the grammar: `true`, a number, `"quoted"` or a bare word.
fn literal(s: &str) -> Value {
    let s = s.trim();
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => match s.parse::<i64>() {
            Ok(n) => Value::from(n),
            Err(_) => Value::String(s.trim_matches('"').to_owned()),
        },
    }
}

/// Whether `s` is a plain choice: a word, a number or a quoted word (`landscape-right`, `"boot"`).
fn is_choice(s: &str) -> bool {
    let s = s.trim().trim_matches('"');
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// One alternative of an item: `name`, `name?`, `name(a|b)`, `name?(x)`. Literals such as
/// `"default"` and anything that isn't a name are skipped.
fn parse_atom(atom: &str, only: Option<&'static str>) -> Option<Param> {
    let atom = atom.trim();
    let name_end = atom.find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')).unwrap_or(atom.len());
    let name = &atom[..name_end];
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return None;
    }
    let mut rest = atom[name_end..].trim_start();
    let optional = rest.starts_with('?');
    rest = rest.trim_start_matches('?').trim_start();
    let mut param = Param { name: name.to_owned(), optional, choices: Vec::new(), range: None, default: None, note: None, only };
    if let Some(inner) = rest.strip_prefix('(').and_then(|r| r.rfind(')').map(|end| &r[..end])) {
        // `(a|b|c; iOS)`: the choices, then a note on where they apply.
        let (body, tail) = inner.split_once(';').map_or((inner, None), |(b, t)| (b, Some(t.trim())));
        let alts: Vec<&str> = body.split('|').collect();
        let range = body.split_once('-').and_then(|(a, b)| Some((a.trim().parse::<i64>().ok()?, b.trim().parse::<i64>().ok()?)));
        if let Some(range) = range {
            param.range = Some(range);
        } else if alts.len() > 1 && alts.iter().all(|a| is_choice(a)) {
            param.choices = alts.iter().map(|a| literal(a)).collect();
        } else if alts.len() == 1 && is_choice(body) && !body.trim().starts_with('.') {
            // `timeout_s?(600)` is a default; `name(slim)`, with no `?`, is the one choice there is.
            if optional {
                param.default = Some(literal(body));
            } else {
                param.choices = vec![literal(body)];
            }
        } else if !body.trim().is_empty() && !body.trim_start().starts_with(['[', '{']) {
            param.note = Some(inner.trim().to_owned());
        }
        if let Some(tail) = tail.filter(|_| param.note.is_none() && !param.choices.is_empty()) {
            param.note = Some(tail.to_owned());
        }
    }
    Some(param)
}

/// Every parameter of a method's grammar, with whether it is required, and the groups of
/// alternatives of which the method wants one (`ticks | delta`; JSON Schema can't say so without
/// `anyOf` at the top, which tool clients refuse, so a group's members are each optional in the
/// schema and say which others they go with).
fn parse(spec: &str) -> (Vec<(Param, bool)>, Vec<Vec<String>>) {
    let mut out: Vec<(Param, bool)> = Vec::new();
    let mut groups: Vec<Vec<String>> = Vec::new();
    for (i, section) in split_top(spec, ';').into_iter().enumerate() {
        let section = section.trim();
        let (only, section) = if let Some(rest) = section.strip_prefix("iOS:") {
            (Some("iOS"), rest)
        } else if let Some(rest) = section.strip_prefix("Android:") {
            (Some("Android"), rest)
        } else {
            (None, section)
        };
        for item in split_top(section, ',') {
            let alts = split_top(item, '|');
            let parsed: Vec<Param> = alts.iter().filter_map(|atom| parse_atom(atom, only)).collect();
            // Alternatives are names to choose between; a quoted word beside one name is a value it may take.
            let one_of = parsed.len() > 1;
            if one_of && i == 0 && only.is_none() && parsed.iter().all(|p| !p.optional) {
                groups.push(parsed.iter().map(|p| p.name.clone()).collect());
            }
            for param in parsed {
                // Required: written without `?` or a default, alone, in the section every device reads.
                let required = !param.optional && param.default.is_none() && !one_of && i == 0 && only.is_none();
                match out.iter_mut().find(|(p, _)| p.name == param.name) {
                    Some((_, was_required)) => *was_required &= required,
                    None => out.push((param, required)),
                }
            }
        }
    }
    (out, groups)
}

/// The groups of alternatives of which `method` wants one of each (`console`'s `command | commands`).
#[cfg(test)]
pub(crate) fn one_of(method: &str) -> Vec<Vec<String>> {
    METHODS.iter().find(|(name, ..)| *name == method).map(|(_, spec, _)| parse(spec).1).unwrap_or_default()
}

/// Parameters a method takes that its grammar can't write as a list of names (`xr`'s actions each
/// read different ones).
const IRREGULAR: &[(&str, &[&str])] = &[("xr", &["x", "y", "z", "environment", "passthrough", "mode", "phase", "press", "gesture"])];

/// Methods whose grammar writes alternative forms with shared names (`touchpad`: a touch is `x, y`,
/// a swipe adds `to_x, to_y`), which a per-name reading can't tell apart: the parameters every
/// form needs.
const REQUIRED: &[(&str, &[&str])] =
    &[("touchpad", &["device", "x", "y"]), ("stylus", &["device", "x", "y"]), ("pointer", &["device", "x", "y"])];

/// The JSON Schema (an object schema) of `method`'s parameters, or `None` for a name that isn't in
/// [`METHODS`]. Choices, ranges and defaults become `enum`, `minimum`/`maximum` and `default`;
/// parameters only one platform reads say so in their `description`.
pub fn input_schema(method: &str) -> Option<Value> {
    let (_, spec, _) = METHODS.iter().find(|(name, ..)| *name == method)?;
    let mut properties = Map::new();
    let mut required = Vec::new();
    let (mut params, groups) = parse(spec);
    for name in IRREGULAR.iter().filter(|(m, _)| *m == method).flat_map(|(_, names)| names.iter()) {
        if !params.iter().any(|(p, _)| p.name == *name) {
            let param =
                Param { name: (*name).to_owned(), optional: true, choices: vec![], range: None, default: None, note: None, only: None };
            params.push((param, false));
        }
    }
    let fixed = REQUIRED.iter().find(|(m, _)| *m == method).map(|(_, names)| *names);
    for (param, mut is_required) in params {
        if let Some(names) = fixed {
            is_required = names.contains(&param.name.as_str());
        }
        if is_required {
            required.push(Value::String(param.name.clone()));
        }
        let mut prop = property(method, &param);
        if let Some(group) = groups.iter().find(|g| g.contains(&param.name)) {
            let names: Vec<String> = group.iter().map(|n| format!("`{n}`")).collect();
            let note = format!("give one of {}", names.join(", "));
            prop["description"] = match prop["description"].as_str() {
                Some(text) => format!("{text}; {note}").into(),
                None => note.into(),
            };
        }
        properties.insert(param.name.clone(), prop);
    }
    let mut schema = json!({"type": "object", "properties": properties});
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    Some(schema)
}

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// The schema of one parameter.
fn property(method: &str, param: &Param) -> Value {
    let ty = param_type(method, &param.name);
    let mut prop = object(match ty {
        Some(Ty::Str) => json!({"type": "string"}),
        Some(Ty::Num) => json!({"type": "number"}),
        Some(Ty::Int) => json!({"type": "integer"}),
        Some(Ty::Bool) => json!({"type": "boolean"}),
        Some(Ty::StrMap) => json!({"type": "object", "additionalProperties": {"type": "string"}}),
        Some(Ty::Obj) => json!({"type": "object"}),
        Some(Ty::Objects) => json!({"type": "array", "items": {"type": "object"}}),
        Some(Ty::Strings) => json!({"type": "array", "items": {"type": "string"}}),
        None => json!({}),
    });
    if !param.choices.is_empty() {
        // Choices that mix types (`false`, `"boot"`) say so by leaving the type out.
        let uniform = |f: fn(&Value) -> bool| param.choices.iter().all(f);
        if !(uniform(Value::is_string) || uniform(Value::is_boolean) || uniform(Value::is_number)) {
            prop.remove("type");
        }
        prop.insert("enum".into(), Value::Array(param.choices.clone()));
    }
    if let Some((min, max)) = param.range {
        prop.insert("minimum".into(), min.into());
        prop.insert("maximum".into(), max.into());
    }
    if let Some(default) = &param.default {
        // A default of the parameter's own type; `profile?(slim)` is a string, `first_port?(5554)` a number.
        let fits = match ty {
            Some(Ty::Bool) => default.is_boolean(),
            Some(Ty::Num | Ty::Int) => default.is_number(),
            Some(Ty::Str) => default.is_string(),
            _ => true,
        };
        if fits {
            prop.insert("default".into(), default.clone());
        }
    }
    let mut notes = Vec::new();
    if param.name == "device" {
        notes.push("A device's UDID, name or `booted`".to_owned());
    }
    if let Some(only) = param.only {
        notes.push(format!("{only} only"));
    }
    if let Some(note) = &param.note {
        notes.push(note.clone());
    }
    if !notes.is_empty() {
        prop.insert("description".into(), notes.join("; ").into());
    }
    Value::Object(prop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(method: &str) -> Value {
        input_schema(method).unwrap_or_else(|| panic!("{method} is a method"))
    }

    #[test]
    fn required_optional_and_typed() {
        let tap = schema("tap");
        assert_eq!(tap["required"], json!(["device", "x", "y"]));
        assert_eq!(tap["properties"]["x"]["type"], "number");
        assert_eq!(tap["properties"]["hold_ms"]["type"], "integer");
        assert_eq!(tap["properties"]["display"]["enum"], json!(["main", "outer", "inner", "carplay"]));
        assert_eq!(tap["properties"]["device"]["description"], "A device's UDID, name or `booted`");
        assert_eq!(schema("devices").get("required"), None, "every parameter is optional");
        assert_eq!(schema("help"), json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn platform_sections_are_optional_and_say_where_they_apply() {
        let create = schema("create");
        assert_eq!(create["required"], json!(["name"]));
        assert_eq!(create["properties"]["device_type"]["description"], "iOS only");
        assert_eq!(create["properties"]["system_image"]["description"], "Android only");
        let boot = schema("boot");
        assert_eq!(boot["required"], json!(["device"]));
        assert_eq!(boot["properties"]["env"]["type"], "object");
        assert_eq!(boot["properties"]["settle_foreground"]["default"], true);
        assert_eq!(boot["properties"]["profile"]["default"], "slim");
        // `profile`'s `name(slim)` has no `?`: the iOS dispatcher requires it, and `slim` is its one value.
        let profile = schema("profile");
        assert_eq!(profile["required"], json!(["device", "name"]));
        assert_eq!((profile["properties"]["name"]["enum"].clone(), profile["properties"]["name"].get("default")), (json!(["slim"]), None));
        assert_eq!(boot["properties"]["audio"]["default"], false);
        assert_eq!(schema("boot_many")["properties"]["first_port"]["default"], 5554);
        assert_eq!(schema("boot_many")["properties"]["devices"], json!({"type": "array", "items": {"type": "string"}}));
    }

    #[test]
    fn alternatives_are_all_optional() {
        let write = schema("write");
        assert_eq!(write["required"], json!(["device", "path"]));
        assert!(write["properties"]["text"].is_object() && write["properties"]["base64"].is_object());
        assert_eq!(schema("console")["required"], json!(["device"]));
        assert!(schema("console")["properties"]["commands"].is_object());
        // `audio_route`'s `uid | "default"` is the uid alone, and required: the quoted word is a value, not a name.
        assert_eq!(schema("audio_route")["properties"].as_object().map(|p| p.len()), Some(3));
        assert_eq!(schema("audio_route")["required"], json!(["device", "scope", "uid"]));
        let rotary = schema("rotary");
        assert_eq!(rotary["required"], json!(["device"]));
        assert_eq!(rotary["properties"]["delta"]["description"], "Digital Crown units; iOS; give one of `ticks`, `delta`");
        assert_eq!(one_of("fold"), [["folded", "angle", "posture"]]);
        assert_eq!(one_of("console"), [["command", "commands"]]);
        // `text? | base64?` are both optional, and a lone name or a quoted word is no group.
        assert!(one_of("write").is_empty() && one_of("audio_route").is_empty() && one_of("tap").is_empty());
    }

    #[test]
    fn choices_ranges_and_defaults() {
        let button = schema("button");
        let choices = button["properties"]["button"]["enum"].as_array().expect("choices");
        assert!(choices.contains(&json!("home")) && choices.contains(&json!("play-pause")) && choices.contains(&json!("back")));
        assert_eq!(schema("audio_volume")["properties"]["level"], json!({"type": "integer", "minimum": 0, "maximum": 100}));
        assert_eq!(schema("fold")["properties"]["angle"]["maximum"], 180);
        let rotate = schema("rotate");
        assert_eq!(rotate["properties"]["orientation"]["enum"].as_array().map(Vec::len), Some(4));
        assert_eq!(rotate["properties"]["orientation"]["description"], "iOS");
        // Choices of mixed types carry no `type`.
        let inject = &schema("network_shape")["properties"]["inject"];
        assert_eq!(inject["enum"], json!([false, true, "launchd", "boot"]));
        assert_eq!(inject.get("type"), None);
        assert_eq!(schema("network_shape")["properties"]["rules"]["type"], "array");
    }

    #[test]
    fn irregular_grammars_still_list_their_names() {
        let xr = schema("xr");
        assert_eq!(xr["required"], json!(["device", "action"]));
        for name in ["x", "z", "environment", "passthrough", "mode", "phase", "press", "gesture"] {
            assert!(xr["properties"][name].is_object(), "{name}");
        }
        assert_eq!(schema("touchpad")["properties"]["to_x"]["type"], "number");
        // A touch is `x, y`; only a swipe adds `to_x, to_y`.
        assert_eq!(schema("touchpad")["required"], json!(["device", "x", "y"]));
        // `ticks | delta`: an Apple Watch takes either, an Android watch only `ticks`, so neither is
        // required alone; each says it goes with the other.
        assert_eq!(schema("pointer")["required"], json!(["device", "x", "y"]));
        assert_eq!(schema("rotary")["required"], json!(["device"]));
        assert_eq!(one_of("rotary"), [["ticks", "delta"]]);
        assert_eq!(schema("rotary")["properties"]["ticks"]["description"], "give one of `ticks`, `delta`");
        let stylus = schema("stylus");
        assert_eq!(stylus["required"], json!(["device", "x", "y"]));
        assert_eq!(
            (stylus["properties"]["button"]["type"].clone(), stylus["properties"]["pressure"]["type"].clone()),
            (json!("boolean"), json!("integer"))
        );
        assert_eq!(schema("button")["properties"]["button"]["type"], "string");
    }

    #[test]
    fn every_method_has_a_schema_and_every_parameter_a_type() {
        let mut untyped = Vec::new();
        for (method, ..) in METHODS {
            let s = schema(method);
            assert_eq!(s["type"], "object", "{method}");
            for (name, prop) in s["properties"].as_object().expect("properties") {
                // Choices and ranges type themselves; a parameter with neither needs a line in
                // `param_type` so agents see what to send.
                if prop.get("type").is_none()
                    && prop.get("enum").is_none()
                    && !matches!(name.as_str(), "value" | "size" | "option" | "area_id")
                {
                    untyped.push(format!("{method}.{name}"));
                }
            }
        }
        assert!(untyped.is_empty(), "add these parameters to `param_type` in schema.rs: {untyped:?}");
        assert_eq!(input_schema("nope"), None);
    }
}
