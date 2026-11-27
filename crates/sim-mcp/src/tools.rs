//! Tools: one per method in `sim-api`'s table, generated from it, plus the one that opens the
//! device view. A method added to the table is a tool the next build.

use serde_json::{Value, json};
use sim_api::{METHODS, input_schema, platforms};

use crate::app;
use crate::backend::Backend;
use crate::rpc::{Ctx, Era, RpcError};

/// The tool that opens the device view (not an API method: the view's data comes from `screen`).
pub const VIEW_TOOL: &str = "device_view";

/// Methods that can run for many seconds: a client that declares the Tasks extension gets a task
/// handle for these, and everything else a progress heartbeat while the call blocks. A method not
/// listed runs inline. (`provision` and `record` aren't API methods, so they aren't tools.)
pub const LONG_RUNNING: &[&str] =
    &["boot", "boot_many", "create", "clone", "install", "settle", "erase", "shutdown", "export", "import", "mem_leaks", "mem_heap"];

/// What a method does to a device or the host, for the annotations a client's approval prompts
/// read. Every method in the table is classified here on purpose (a test fails for one that isn't),
/// since a client may auto-approve what is marked read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    /// Changes nothing, on the device or the host.
    Read,
    /// Sets something to a value; repeating it changes nothing more.
    Set,
    /// Does something each time it is called (input, a boot) without destroying anything.
    Act,
    /// May lose data, run code or change what's running; repeating it can differ.
    Danger,
    /// Destroys or overwrites something; repeating it changes nothing more.
    Destroy,
}

/// The effect of `method`, or `None` for one nobody has classified (it then gets the specification's
/// defaults: it writes, and may destroy).
fn effect(method: &str) -> Option<Effect> {
    use Effect::*;
    Some(match method {
        "prefs_stores" | "prefs_read" | "prefs_watch" => Read,
        "prefs_write" | "prefs_delete" | "prefs_batch" => Destroy,
        // Read a corpse of the app: nothing on the device changes.
        "mem_leaks" | "mem_heap" | "mem_vmmap" | "mem_history" => Read,
        "help" | "identity" | "host" | "form_factors" | "devices" | "runtimes" | "device_types" | "screen" | "screenshot" | "apps"
        | "ca_debug_state" | "logs" | "processes" | "connections" | "traffic" | "containers" | "ls" | "read" | "du" | "crash_reports"
        | "info" | "defaults_read" | "network_state" | "network_flows" | "network_env" | "audio_state" | "settle" | VIEW_TOOL => Read,
        "appearance" | "location" | "status_bar" | "clear_status_bar" | "content_size" | "increase_contrast" | "dynamic_island"
        | "backlight" | "keyboard_language" | "hardware_keyboard" | "ca_debug" | "network_clear" | "audio_volume" | "audio_route"
        | "audio_mute" | "fold" | "carplay" | "profile" | "privacy" => Set,
        "create" | "boot" | "boot_many" | "clone" | "tap" | "swipe" | "touch" | "button" | "type" | "key" | "open_url" | "install"
        | "memory_warning" | "notify" | "biometric" | "rotate" | "rotary" | "watch_gesture" | "remote_swipe" | "xr" | "touchpad"
        | "stylus" | "pointer" | "vehicle" | "audio_capture_stop" | "contrast_check" | "import" => Act,
        // Runs programs, starts apps with an environment, replaces a network profile (inject "boot"
        // reboots the device), delivers a payload, writes a recording or a bundle to a path, drives
        // the console.
        "exec" | "launch" | "push" | "network_shape" | "console" | "audio_capture_start" | "kill" | "export" => Danger,
        "delete" | "erase" | "shutdown" | "rm" | "uninstall" | "terminate" | "reset_keychain" | "clear_container" | "write"
        | "defaults_write" => Destroy,
        _ => return None,
    })
}

/// Parameters the API takes but a tool doesn't: `screenshot`'s `path` writes a file anywhere on the
/// host, which a client that auto-approves read-only tools must not be able to ask for. The image
/// comes back in the result instead.
const WITHHELD: &[(&str, &str, &str)] = &[
    ("screenshot", "path", "The device's screen as a PNG image, returned in the result"),
    (
        "contrast_check",
        "path",
        "Text and controls below WCAG contrast in a screenshot, from pixels alone: finds text-like regions, estimates each one's foreground and \
         background, and reports those under 4.5:1 (normal text) or 3:1 (large text of about 18 pt, icons and controls) with their box, the \
         two colours and the ratio. The screenshot with the findings boxed comes back as an image (one per appearance with appearance both, \
         which on iOS checks the light and the dark UI, then leaves the device in `restore`; Android can't switch appearance yet). Text over \
         a photo or a busy background has no one background and isn't judged. region is {x, y, width, height} in pixels; scale is pixels per \
         point (default: the device's, else 1)",
    ),
];

fn withheld(method: &str) -> Option<&'static str> {
    WITHHELD.iter().find(|(m, ..)| *m == method).map(|(_, p, _)| *p)
}

/// Whether `name` is a tool.
pub fn exists(name: &str) -> bool {
    name == VIEW_TOOL || METHODS.iter().any(|(m, ..)| *m == name)
}

/// The annotations the MCP spec defines. An unclassified method gets only `readOnlyHint: false`, so
/// a client applies the specification's defaults (it may destroy, and isn't idempotent).
fn annotations(method: &str) -> Value {
    let mut a = json!({"title": method, "openWorldHint": false});
    match effect(method) {
        Some(Effect::Read) => a["readOnlyHint"] = true.into(),
        Some(effect) => {
            a["readOnlyHint"] = false.into();
            a["destructiveHint"] = matches!(effect, Effect::Danger | Effect::Destroy).into();
            a["idempotentHint"] = matches!(effect, Effect::Set | Effect::Destroy).into();
        }
        None => a["readOnlyHint"] = false.into(),
    }
    a
}

fn describe(method: &str, params: &str, doc: &str) -> String {
    let on: Vec<&str> = platforms(method).iter().map(|p| p.label()).collect();
    let mut text = format!("{doc}{}\n\nPlatforms: {}.", if doc.ends_with('.') { "" } else { "." }, on.join(", "));
    if !params.is_empty() {
        text.push_str(&format!(" Parameters: {params}."));
    }
    if LONG_RUNNING.contains(&method) {
        text.push_str(" Can take a while: a client with the tasks extension gets a task to poll.");
    }
    text
}

/// Every tool: the method table in its order, then the view.
pub fn list() -> Vec<Value> {
    let mut tools: Vec<Value> = METHODS
        .iter()
        .map(|(method, params, doc)| {
            let mut schema = input_schema(method).unwrap_or_else(|| json!({"type": "object"}));
            let (mut params, mut doc) = (params.to_string(), doc.to_string());
            if let Some((_, name, text)) = WITHHELD.iter().find(|(m, ..)| m == method) {
                doc = (*text).to_owned();
                if let Some(properties) = schema["properties"].as_object_mut() {
                    properties.remove(*name);
                }
                params = params.split(", ").filter(|t| t != name && !t.starts_with(&format!("{name}?"))).collect::<Vec<_>>().join(", ");
            }
            json!({
                "name": method,
                "title": method,
                "description": describe(method, &params, &doc),
                "inputSchema": schema,
                "annotations": annotations(method),
            })
        })
        .collect();
    tools.push(json!({
        "name": VIEW_TOOL,
        "title": "Device view",
        "description": "Open a live view of a device: its screen, refreshed about once a second, with tap, swipe, type and button \
            controls (an MCP App; a client without app support gets the screen size). Platforms: Android, iOS.",
        "inputSchema": {"type": "object", "properties": {"device": {"type": "string",
            "description": "A device's UDID, name or `booted` (the default)"}}},
        "annotations": {"title": "Device view", "readOnlyHint": true, "openWorldHint": false},
        "_meta": {"ui": {"resourceUri": app::URI}},
    }));
    tools
}

/// The tool's arguments as the method's params: a missing or null `arguments` is none, anything
/// but an object is refused.
pub fn arguments(params: &Value) -> Result<Value, RpcError> {
    match params.get("arguments") {
        None | Some(Value::Null) => Ok(json!({})),
        Some(args @ Value::Object(_)) => Ok(args.clone()),
        Some(_) => Err(RpcError::invalid_params("`arguments` must be an object")),
    }
}

/// Runs one tool: the method's outcome (a failure is a tool error, not a protocol error).
pub fn run(backend: &dyn Backend, name: &str, args: &Value) -> Result<Value, String> {
    if let Some(param) = withheld(name).filter(|p| args.get(*p).is_some_and(|v| !v.is_null())) {
        return Err(format!("`{param}` isn't available through MCP: `{name}` returns the image in its result"));
    }
    if name == VIEW_TOOL {
        let device = args.get("device").and_then(Value::as_str).unwrap_or("booted");
        let screen = backend.call("screen", &json!({"device": device}))?;
        return Ok(json!({"device": device, "screen": screen}));
    }
    backend.call(name, args)
}

/// A method's outcome as a `CallToolResult`. A screenshot's PNG becomes an image block instead of
/// base64 in text; everything else is its JSON as text, and as `structuredContent` too (an object,
/// for a client of an older revision).
pub fn result(name: &str, outcome: Result<Value, String>, era: Era) -> Value {
    let value = match outcome {
        Ok(value) => value,
        Err(message) => return json!({"content": [{"type": "text", "text": message}], "isError": true}),
    };
    if name == VIEW_TOOL {
        let (w, h) = (value["screen"]["width"].as_f64().unwrap_or(0.0), value["screen"]["height"].as_f64().unwrap_or(0.0));
        let text = format!(
            "Live view of {} ({w}×{h} px) opened; tap, swipe, type and press buttons in the view.",
            value["device"].as_str().unwrap_or("?")
        );
        return json!({"content": [{"type": "text", "text": text}], "structuredContent": value});
    }
    if let (Some(png), true) = (value.get("png").and_then(Value::as_str), name == "screenshot") {
        let meta = json!({"width": value["width"], "height": value["height"]});
        return json!({"content": [{"type": "image", "data": png, "mimeType": "image/png"},
            {"type": "text", "text": meta.to_string()}], "structuredContent": meta});
    }
    if name == "contrast_check" {
        return contrast_result(value);
    }
    let text = if value.is_null() { "ok".to_owned() } else { value.to_string() };
    let mut result = json!({"content": [{"type": "text", "text": text}]});
    if !value.is_null() {
        result["structuredContent"] = if era == Era::Modern || value.is_object() { value } else { json!({"result": value}) };
    }
    result
}

/// `contrast_check`'s outcome: each annotated PNG (one, or one per appearance) becomes an image block
/// and the findings stay as JSON text and `structuredContent`, so a client never reads a megabyte of
/// base64 as text.
fn contrast_result(mut value: Value) -> Value {
    let mut images = Vec::new();
    let mut lift = |v: &mut Value, label: &str| {
        if let Some(png) = v.as_object_mut().and_then(|o| o.remove("png")).and_then(|p| p.as_str().map(str::to_owned)) {
            images.push(json!({"type": "image", "data": png, "mimeType": "image/png"}));
            v["image"] = json!(images.len() - 1);
            v["image_of"] = json!(label);
        }
    };
    lift(&mut value, "screen");
    if let Some(each) = value.get_mut("appearances").and_then(Value::as_object_mut) {
        for (mode, v) in each.iter_mut() {
            lift(v, mode);
        }
    }
    let mut content = vec![json!({"type": "text", "text": value.to_string()})];
    content.extend(images);
    json!({"content": content, "structuredContent": value})
}

/// What a tool call needs from the request to decide how to run: its name and arguments, checked.
pub fn parse_call(params: &Value) -> Result<(String, Value), RpcError> {
    let name = params.get("name").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("`name` is required"))?;
    if !exists(name) {
        return Err(RpcError::invalid_params(format!("Unknown tool: {name}")));
    }
    Ok((name.to_owned(), arguments(params)?))
}

/// Whether this call should be a task for this client.
pub fn as_task(name: &str, ctx: &Ctx) -> bool {
    ctx.modern() && ctx.has_extension(crate::rpc::EXT_TASKS) && LONG_RUNNING.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::Fake;

    #[test]
    fn there_is_a_tool_for_every_method_in_the_table() {
        let tools = list();
        for (method, params, doc) in METHODS {
            let tool = tools.iter().find(|t| t["name"] == *method).unwrap_or_else(|| panic!("no tool for {method}"));
            let description = tool["description"].as_str().expect("description");
            if withheld(method).is_none() {
                assert!(description.starts_with(doc), "{method}");
                assert!(params.is_empty() || description.contains(params), "{method} lists its parameters");
            }
            assert!(description.contains("Platforms: "), "{method} says where it works");
            assert_eq!(tool["inputSchema"]["type"], "object", "{method}");
        }
        assert_eq!(tools.len(), METHODS.len() + 1, "the table plus the view");
        assert_eq!(tools.last().map(|t| t["name"].clone()), Some(json!(VIEW_TOOL)));
        // Tool names are what the spec allows, and unique.
        let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.iter().all(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')));
        names.sort();
        names.dedup();
        assert_eq!(names.len(), tools.len());
    }

    #[test]
    fn descriptions_say_which_platforms_a_method_works_on() {
        let tools = list();
        let find = |m: &str| tools.iter().find(|t| t["name"] == m).and_then(|t| t["description"].as_str()).expect(m).to_owned();
        assert!(find("tap").contains("Platforms: Android, iOS."));
        assert!(find("xr").contains("Platforms: Android."));
        assert!(!find("xr").contains("iOS."));
        assert!(find("fold").contains("Platforms: Android, iOS."), "the iPhone Duo folds too");
        assert!(find("status_bar").contains("Platforms: iOS."));
        assert!(find("boot").contains("task"));
        assert!(!find("tap").contains("task"));
    }

    #[test]
    fn every_method_is_classified_on_purpose() {
        // A method that isn't in `effect` would get the specification's defaults: add it there.
        let unclassified: Vec<&str> = METHODS.iter().map(|(m, ..)| *m).filter(|m| effect(m).is_none()).collect();
        assert!(unclassified.is_empty(), "classify these in `effect` in tools.rs: {unclassified:?}");
        assert!(effect(VIEW_TOOL).is_some());
        for name in LONG_RUNNING {
            assert!(METHODS.iter().any(|(m, ..)| m == name), "{name} is not a method");
        }
    }

    #[test]
    fn annotations_follow_what_a_method_does() {
        let tools = list();
        let ann = |m: &str| tools.iter().find(|t| t["name"] == m).map(|t| t["annotations"].clone()).expect(m);
        assert_eq!(ann("devices")["readOnlyHint"], true);
        assert_eq!(ann("devices").get("destructiveHint"), None);
        assert_eq!((ann("delete")["destructiveHint"].clone(), ann("delete")["idempotentHint"].clone()), (json!(true), json!(true)));
        assert_eq!((ann("tap")["readOnlyHint"].clone(), ann("tap")["destructiveHint"].clone()), (json!(false), json!(false)));
        assert_eq!(ann("appearance")["idempotentHint"], true);
        assert_eq!(ann("tap")["openWorldHint"], false);
        // What runs code, replaces state or overwrites files is marked destructive, not harmless.
        for risky in ["exec", "write", "defaults_write", "network_shape", "push", "launch", "console", "kill", "audio_capture_start"] {
            assert_eq!((ann(risky)["readOnlyHint"].clone(), ann(risky)["destructiveHint"].clone()), (json!(false), json!(true)), "{risky}");
        }
        // Read-only means it changes nothing: not a capture that stops and writes a recording.
        assert_eq!(ann("audio_capture_stop")["readOnlyHint"], false);
        assert_eq!(ann("screenshot")["readOnlyHint"], true);
        // A method nobody classified gets the specification's defaults, not a claim of safety.
        let unknown = annotations("a_new_method");
        assert_eq!(unknown["readOnlyHint"], false);
        assert_eq!((unknown.get("destructiveHint"), unknown.get("idempotentHint")), (None, None));
        // Every read-only tool is one of the listings and reads, by name.
        let read_only: Vec<&str> = METHODS.iter().map(|(m, ..)| *m).filter(|m| effect(m) == Some(Effect::Read)).collect();
        assert!(read_only.iter().all(|m| !m.starts_with("audio_capture") && !["write", "rm", "exec", "kill"].contains(m)), "{read_only:?}");
    }

    #[test]
    fn screenshot_does_not_take_a_host_path() {
        let tools = list();
        let shot = tools.iter().find(|t| t["name"] == "screenshot").expect("screenshot");
        assert_eq!(shot["inputSchema"]["properties"].get("path"), None);
        assert!(!shot["description"].as_str().expect("description").contains("path"));
        assert!(shot["inputSchema"]["properties"]["device"].is_object());
        let fake = Fake::default();
        let err = run(&fake, "screenshot", &json!({"device": "d", "path": "/home/me/.zshrc"})).expect_err("refused");
        assert!(err.contains("`path` isn't available through MCP"), "{err}");
        assert!(fake.called("screenshot").is_empty(), "never reaches the API");
        assert!(run(&fake, "screenshot", &json!({"device": "d", "path": null})).is_ok());
        // Other methods keep their parameters.
        assert!(
            tools.iter().find(|t| t["name"] == "audio_capture_start").is_some_and(|t| t["inputSchema"]["properties"].get("path").is_some())
        );
    }

    #[test]
    fn the_docs_name_every_tool_and_resource() {
        // docs/mcp.md lives at the repository root, outside the published crate: in the
        // workspace a missing doc fails, from an unpacked .crate the test has nothing to check.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        if !root.join("crates/sim-mcp/Cargo.toml").is_file() {
            eprintln!("skipped: not in the simee workspace, so docs/mcp.md isn't here");
            return;
        }
        let docs = std::fs::read_to_string(root.join("docs/mcp.md")).expect("docs/mcp.md");
        for tool in list() {
            let name = tool["name"].as_str().expect("name");
            assert!(docs.contains(&format!("`{name}`")), "docs/mcp.md doesn't name the tool `{name}`: add it to the tool table");
        }
        for uri in [
            crate::resources::DEVICES,
            crate::resources::RUNTIMES,
            crate::resources::DEVICE_TYPES,
            crate::resources::FORM_FACTORS,
            app::URI,
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(crate::resources::templates().as_array().into_iter().flatten().filter_map(|t| t["uriTemplate"].as_str().map(str::to_owned)))
        {
            assert!(docs.contains(&format!("`{uri}`")), "docs/mcp.md doesn't name the resource `{uri}`");
        }
        for method in LONG_RUNNING {
            assert!(docs.contains(&format!("`{method}`")), "{method}");
        }
    }

    #[test]
    fn the_view_tool_points_at_the_app() {
        let tools = list();
        let view = tools.last().expect("view");
        assert_eq!(view["_meta"]["ui"]["resourceUri"], app::URI);
        assert!(tools.iter().filter(|t| t.get("_meta").is_some()).count() == 1);
    }

    #[test]
    fn arguments_are_objects() {
        assert_eq!(arguments(&json!({"name": "tap"})), Ok(json!({})));
        assert_eq!(arguments(&json!({"arguments": null})), Ok(json!({})));
        assert_eq!(arguments(&json!({"arguments": {"x": 1}})), Ok(json!({"x": 1})));
        assert_eq!(arguments(&json!({"arguments": [1]})).expect_err("array").code, crate::rpc::INVALID_PARAMS);
        assert_eq!(parse_call(&json!({"name": "nope"})).expect_err("unknown").message, "Unknown tool: nope");
        assert!(parse_call(&json!({})).is_err());
        assert_eq!(parse_call(&json!({"name": "tap", "arguments": {"x": 1}})), Ok(("tap".to_owned(), json!({"x": 1}))));
    }

    #[test]
    fn results_are_content_and_structure() {
        let ok = result("devices", Ok(json!([{"udid": "a"}])), Era::Modern);
        assert_eq!(ok["content"][0]["text"], r#"[{"udid":"a"}]"#);
        assert_eq!(ok["structuredContent"], json!([{"udid": "a"}]));
        // An older revision's structuredContent is an object.
        let old = result("devices", Ok(json!([1])), Era::Legacy);
        assert_eq!(old["structuredContent"], json!({"result": [1]}));
        assert_eq!(result("info", Ok(json!({"a": 1})), Era::Legacy)["structuredContent"], json!({"a": 1}));
        let none = result("tap", Ok(Value::Null), Era::Modern);
        assert_eq!((none["content"][0]["text"].clone(), none.get("structuredContent")), (json!("ok"), None));
        let failed = result("tap", Err("no device matches `x`".into()), Era::Modern);
        assert_eq!((failed["isError"].clone(), failed["content"][0]["text"].clone()), (json!(true), json!("no device matches `x`")));
        let shot = result("screenshot", Ok(json!({"width": 2, "height": 1, "png": "iVBOR"})), Era::Modern);
        assert_eq!(shot["content"][0], json!({"type": "image", "data": "iVBOR", "mimeType": "image/png"}));
        assert_eq!(shot["structuredContent"], json!({"width": 2, "height": 1}));
        // A contrast check's PNGs become image blocks and leave the text a few lines of findings.
        let check = result(
            "contrast_check",
            Ok(json!({"failing": 1, "appearances": {"light": {"png": "AAA", "findings": []}, "dark": {"png": "BBB", "findings": []}}})),
            Era::Modern,
        );
        // `image` is a result's place among the image blocks (JSON objects have no order to lean on).
        let at = |mode: &str| check["structuredContent"]["appearances"][mode]["image"].as_u64().expect("image") as usize + 1;
        assert_eq!(check["content"][at("light")], json!({"type": "image", "data": "AAA", "mimeType": "image/png"}));
        assert_eq!(check["content"][at("dark")]["data"], "BBB");
        assert_ne!(at("light"), at("dark"));
        assert!(!check["content"][0]["text"].as_str().expect("text").contains("AAA"));
        let single = result("contrast_check", Ok(json!({"png": "CCC", "findings": []})), Era::Modern);
        assert_eq!(
            (single["content"][1]["data"].clone(), single["structuredContent"]["image_of"].clone()),
            (json!("CCC"), json!("screen"))
        );
        // Written to a path, a screenshot is plain data.
        let path = result("screenshot", Ok(json!({"path": "/tmp/x.png", "width": 2, "height": 1})), Era::Modern);
        assert_eq!(path["structuredContent"]["path"], "/tmp/x.png");
    }

    #[test]
    fn running_a_tool_calls_the_method_or_reads_the_screen_for_the_view() {
        let fake = Fake::default();
        fake.answer("screen", Ok(json!({"width": 1080, "height": 2400})));
        assert_eq!(run(&fake, "tap", &json!({"device": "d", "x": 1, "y": 2})), Ok(Value::Null));
        assert_eq!(fake.called("tap"), [json!({"device": "d", "x": 1, "y": 2})]);
        let view = run(&fake, VIEW_TOOL, &json!({})).expect("view");
        assert_eq!(view, json!({"device": "booted", "screen": {"width": 1080, "height": 2400}}));
        assert_eq!(fake.called("screen"), [json!({"device": "booted"})]);
        let shown = result(VIEW_TOOL, Ok(view), Era::Modern);
        assert!(shown["content"][0]["text"].as_str().is_some_and(|t| t.contains("1080")));
        assert_eq!(shown["structuredContent"]["device"], "booted");
        fake.answer("screen", Err("booted: no such device".into()));
        assert!(run(&fake, VIEW_TOOL, &json!({"device": "x"})).is_err());
    }

    #[test]
    fn only_long_methods_for_task_clients_become_tasks() {
        let caps = json!({"extensions": {crate::rpc::EXT_TASKS: {}}});
        let tasks = Ctx { era: Era::Modern, caps, progress_token: None };
        let plain = Ctx { era: Era::Modern, caps: json!({}), progress_token: None };
        let legacy = Ctx { era: Era::Legacy, caps: tasks.caps.clone(), progress_token: None };
        assert!(as_task("boot", &tasks) && as_task("install", &tasks));
        assert!(!as_task("tap", &tasks) && !as_task("boot", &plain) && !as_task("boot", &legacy));
    }
}
