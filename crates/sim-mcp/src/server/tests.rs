//! The protocol end to end, through a fake backend: a rig sends lines and reads back what the
//! server says.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::{Receiver, channel};

use sim_api::METHODS;

use super::*;
use crate::backend::fake::Fake;
use crate::rpc::{META_CLIENT_CAPS, META_SERVER_INFO, META_SUBSCRIPTION, META_VERSION};

const WAIT: Duration = Duration::from_secs(10);
const SHORT: Duration = Duration::from_millis(150);

struct Rig {
    server: Server,
    fake: Arc<Fake>,
    rx: Receiver<Value>,
    /// Everything that wasn't the response being waited for.
    notes: RefCell<Vec<Value>>,
    next: Cell<u64>,
}

impl Rig {
    fn new() -> Rig {
        let fake = Arc::new(Fake::with_devices());
        let timing = Timing { watch: Duration::from_millis(20), heartbeat: Duration::from_millis(40) };
        let (tx, rx) = channel::<String>();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |line| drop(tx.lock().expect("tx").send(line)));
        let rx = {
            // Parse on the way out so tests read `Value`s.
            let (vtx, vrx) = channel();
            std::thread::spawn(move || {
                for line in rx {
                    vtx.send(serde_json::from_str::<Value>(&line).expect("the server writes JSON lines")).ok();
                }
            });
            vrx
        };
        Rig { server: Server::with_timing(fake.clone(), sink, timing), fake, rx, notes: RefCell::default(), next: Cell::new(1) }
    }

    fn meta(tasks: bool) -> Value {
        let caps = if tasks { json!({"extensions": {EXT_TASKS: {}}}) } else { json!({}) };
        json!({META_VERSION: MODERN, META_CLIENT_CAPS: caps})
    }

    fn send(&self, message: &Value) {
        self.server.handle_line(&message.to_string());
    }

    /// Sends a request with these params as they are; returns its id.
    fn send_raw(&self, method: &str, params: Value) -> u64 {
        let id = self.next.replace(self.next.get() + 1);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Sends a modern request (this client declares the Tasks extension or not).
    fn send_modern(&self, method: &str, mut params: Value, tasks: bool) -> u64 {
        params["_meta"] = Rig::meta(tasks);
        self.send_raw(method, params)
    }

    /// The next message from the server, response or not.
    fn next_message(&self) -> Option<Value> {
        self.rx.recv_timeout(WAIT).ok()
    }

    /// The response to request `id`; notifications seen on the way go to `notes`.
    fn response(&self, id: u64) -> Value {
        loop {
            let message = self.next_message().expect("a response in time");
            if message["id"] == id {
                return message;
            }
            self.notes.borrow_mut().push(message);
        }
    }

    fn modern(&self, method: &str, params: Value) -> Value {
        let id = self.send_modern(method, params, false);
        self.response(id)
    }

    fn with_tasks(&self, method: &str, params: Value) -> Value {
        let id = self.send_modern(method, params, true);
        self.response(id)
    }

    /// The next notification with this method (earlier ones are kept in `notes`).
    fn notification(&self, method: &str) -> Value {
        loop {
            let found = self.notes.borrow().iter().position(|n| n["method"] == method);
            if let Some(i) = found {
                return self.notes.borrow_mut().remove(i);
            }
            let message = self.next_message().unwrap_or_else(|| panic!("no {method} notification"));
            self.notes.borrow_mut().push(message);
        }
    }

    /// Waits until the backend has been asked for `method` (a watcher's first read is its baseline).
    fn until_read(&self, method: &str) {
        while self.fake.called(method).is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Nothing more arrives within a moment.
    fn quiet(&self) -> bool {
        self.notes.borrow().is_empty() && self.rx.recv_timeout(SHORT).is_err()
    }
}

#[test]
fn discover_says_what_the_server_offers() {
    let rig = Rig::new();
    let response = rig.modern("server/discover", json!({}));
    let result = &response["result"];
    assert_eq!((result["resultType"].clone(), result["supportedVersions"][0].clone()), (json!("complete"), json!(MODERN)));
    assert_eq!(result["supportedVersions"].as_array().map(Vec::len), Some(1 + LEGACY.len()));
    assert_eq!(result["capabilities"]["tools"], json!({}));
    assert_eq!(result["capabilities"]["resources"], json!({"subscribe": true, "listChanged": true}));
    assert_eq!(result["capabilities"]["extensions"][EXT_TASKS], json!({}));
    assert_eq!(result["_meta"][META_SERVER_INFO]["name"], "simee");
    assert!(result["instructions"].as_str().is_some_and(|i| i.contains("devices")));
    assert_eq!((result["ttlMs"].clone(), result["cacheScope"].clone()), (json!(3_600_000), json!("public")));
    // The probe may omit `_meta`.
    let id = rig.send_raw("server/discover", json!({}));
    assert_eq!(rig.response(id)["result"]["resultType"], "complete");
}

#[test]
fn a_request_names_its_version_or_is_refused() {
    let rig = Rig::new();
    let id = rig.send_raw("tools/list", json!({}));
    let error = rig.response(id)["error"].clone();
    assert_eq!(error["code"], rpc::INVALID_PARAMS);
    assert!(error["message"].as_str().is_some_and(|m| m.contains("server/discover") && m.contains("initialize")));
    let id = rig.send_raw("tools/list", json!({"_meta": {META_VERSION: "1900-01-01"}}));
    let error = rig.response(id)["error"].clone();
    assert_eq!(error["code"], rpc::UNSUPPORTED_VERSION);
    assert_eq!(error["data"]["requested"], "1900-01-01");
    assert_eq!(error["data"]["supported"][0], MODERN);
    // Junk is answered with a null id, an unknown method with its name.
    rig.server.handle_line("not json");
    assert_eq!(rig.next_message().map(|m| (m["id"].clone(), m["error"]["code"].clone())), Some((Value::Null, json!(rpc::PARSE_ERROR))));
    let response = rig.modern("nope/nope", json!({}));
    assert_eq!(
        (response["error"]["code"].clone(), response["error"]["message"].clone()),
        (json!(-32601), json!("Method not found: nope/nope"))
    );
}

#[test]
fn tools_list_has_a_tool_for_every_method() {
    let rig = Rig::new();
    let response = rig.modern("tools/list", json!({}));
    let tools = response["result"]["tools"].as_array().expect("tools");
    for (method, ..) in METHODS {
        assert!(tools.iter().any(|t| t["name"] == *method), "no tool for `{method}`");
    }
    assert_eq!(tools.len(), METHODS.len() + 1);
    assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object" && t["description"].is_string()));
    assert_eq!(response["result"]["resultType"], "complete");
    assert_eq!(response["result"]["cacheScope"], "public");
    let again = rig.modern("tools/list", json!({}));
    assert_eq!(again["result"]["tools"], response["result"]["tools"], "a deterministic order");
}

#[test]
fn a_tool_call_is_dispatched_to_its_method() {
    let rig = Rig::new();
    let response = rig.modern("tools/call", json!({"name": "tap", "arguments": {"device": "phone", "x": 10, "y": 20}}));
    assert_eq!(rig.fake.called("tap"), [json!({"device": "phone", "x": 10, "y": 20})]);
    let result = &response["result"];
    assert_eq!((result["content"][0]["text"].clone(), result["resultType"].clone()), (json!("ok"), json!("complete")));
    // The result comes back as text and structured data.
    let devices = rig.modern("tools/call", json!({"name": "devices"}));
    assert_eq!(devices["result"]["structuredContent"][0]["udid"], "phone");
    assert!(devices["result"]["content"][0]["text"].as_str().is_some_and(|t| t.contains("iPhone 17")));
    assert_eq!(rig.fake.called("devices"), [json!({})], "no arguments is an empty object");
    // A screenshot is an image.
    rig.fake.answer("screenshot", Ok(json!({"width": 2, "height": 1, "png": "iVBOR"})));
    let shot = rig.modern("tools/call", json!({"name": "screenshot", "arguments": {"device": "phone"}}));
    assert_eq!(shot["result"]["content"][0], json!({"type": "image", "data": "iVBOR", "mimeType": "image/png"}));
}

#[test]
fn a_failed_method_is_a_tool_error_and_a_bad_call_a_protocol_error() {
    let rig = Rig::new();
    rig.fake.answer("tap", Err("no device matches `ghost`".into()));
    let failed = rig.modern("tools/call", json!({"name": "tap", "arguments": {"device": "ghost"}}));
    assert_eq!(
        (failed["result"]["isError"].clone(), failed["result"]["content"][0]["text"].clone()),
        (json!(true), json!("no device matches `ghost`"))
    );
    let unknown = rig.modern("tools/call", json!({"name": "teleport"}));
    assert_eq!(
        (unknown["error"]["code"].clone(), unknown["error"]["message"].clone()),
        (json!(rpc::INVALID_PARAMS), json!("Unknown tool: teleport"))
    );
    let bad = rig.modern("tools/call", json!({"name": "tap", "arguments": [1, 2]}));
    assert_eq!(bad["error"]["code"], rpc::INVALID_PARAMS);
    assert_eq!(rig.modern("tools/call", json!({}))["error"]["code"], rpc::INVALID_PARAMS);
}

#[test]
fn the_device_view_tool_reads_the_screen() {
    let rig = Rig::new();
    rig.fake.answer("screen", Ok(json!({"width": 1080, "height": 2400})));
    let response = rig.modern("tools/call", json!({"name": "device_view", "arguments": {"device": "phone"}}));
    assert_eq!(response["result"]["structuredContent"], json!({"device": "phone", "screen": {"width": 1080, "height": 2400}}));
    let listed = rig.modern("tools/list", json!({}));
    let view = listed["result"]["tools"].as_array().and_then(|t| t.iter().find(|t| t["name"] == "device_view")).expect("view tool").clone();
    let page = rig.modern("resources/read", json!({"uri": view["_meta"]["ui"]["resourceUri"]}));
    assert_eq!(page["result"]["contents"][0]["mimeType"], "text/html;profile=mcp-app");
}

#[test]
fn resources_are_listed_and_read() {
    let rig = Rig::new();
    rig.fake.answer("info", Ok(json!({"udid": "phone"})));
    let list = rig.modern("resources/list", json!({}));
    let resources = list["result"]["resources"].as_array().expect("resources");
    assert!(
        resources.iter().any(|r| r["uri"] == "simee://devices") && resources.iter().any(|r| r["uri"] == "simee://devices/phone/screenshot")
    );
    assert_eq!((list["result"]["ttlMs"].clone(), list["result"]["cacheScope"].clone()), (json!(0), json!("private")));
    let read = rig.modern("resources/read", json!({"uri": "simee://devices/phone"}));
    assert_eq!(read["result"]["contents"][0]["uri"], "simee://devices/phone");
    assert!(read["result"]["contents"][0]["text"].as_str().is_some_and(|t| t.contains("udid")));
    let templates = rig.modern("resources/templates/list", json!({}));
    assert_eq!(templates["result"]["resourceTemplates"].as_array().map(Vec::len), Some(4));
    let missing = rig.modern("resources/read", json!({"uri": "simee://devices/"}));
    assert_eq!(missing["error"]["code"], rpc::INVALID_PARAMS);
    assert_eq!(missing["error"]["data"]["uri"], "simee://devices/");
    assert_eq!(rig.modern("resources/read", json!({}))["error"]["code"], rpc::INVALID_PARAMS);
}

#[test]
fn a_long_method_is_a_task_for_a_client_that_declares_the_extension() {
    let rig = Rig::new();
    let gate = rig.fake.gate("boot");
    rig.fake.answer("boot", Ok(json!({"boot_s": 12.0})));
    let created = rig.with_tasks("tools/call", json!({"name": "boot", "arguments": {"device": "phone"}}));
    let handle = &created["result"];
    assert_eq!((handle["resultType"].clone(), handle["status"].clone()), (json!("task"), json!("working")));
    let id = handle["taskId"].as_str().expect("task id").to_owned();
    assert!(handle["pollIntervalMs"].is_number() && handle["ttlMs"].is_number());
    // Still working while the boot is.
    let task = rig.modern("tasks/get", json!({"taskId": id}));
    assert_eq!((task["result"]["status"].clone(), task["result"]["resultType"].clone()), (json!("working"), json!("complete")));
    gate.release();
    let done = loop {
        let task = rig.modern("tasks/get", json!({"taskId": id}));
        if task["result"]["status"] == "completed" {
            break task;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(done["result"]["result"]["structuredContent"], json!({"boot_s": 12.0}));
    assert_eq!(rig.fake.called("boot"), [json!({"device": "phone"})]);
    // A method that isn't long is answered inline even for such a client.
    let tap = rig.with_tasks("tools/call", json!({"name": "tap", "arguments": {"device": "phone", "x": 1, "y": 1}}));
    assert_eq!(tap["result"]["resultType"], "complete");
}

#[test]
fn a_task_can_be_cancelled_and_stays_cancelled() {
    let rig = Rig::new();
    let gate = rig.fake.gate("install");
    let created = rig.with_tasks("tools/call", json!({"name": "install", "arguments": {"device": "phone", "path": "/tmp/a.app"}}));
    let id = created["result"]["taskId"].as_str().expect("task id").to_owned();
    let cancel = rig.modern("tasks/cancel", json!({"taskId": id}));
    assert_eq!(cancel["result"]["resultType"], "complete");
    let task = rig.modern("tasks/get", json!({"taskId": id}));
    assert_eq!(task["result"]["status"], "cancelled");
    assert!(task["result"]["statusMessage"].as_str().is_some_and(|m| m.contains("cancelled")));
    gate.release();
    std::thread::sleep(SHORT);
    let after = rig.modern("tasks/get", json!({"taskId": id}));
    assert_eq!(after["result"]["status"], "cancelled");
    assert_eq!(after["result"].get("result"), None, "its result is discarded");
    // Unknown tasks are protocol errors; update acknowledges a known one and has nothing to do.
    for method in ["tasks/get", "tasks/cancel", "tasks/update"] {
        let response = rig.modern(method, json!({"taskId": "nope", "inputResponses": {}}));
        assert_eq!(response["error"]["code"], rpc::INVALID_PARAMS, "{method}");
    }
    assert_eq!(rig.modern("tasks/update", json!({"taskId": id, "inputResponses": {}}))["result"]["resultType"], "complete");
    assert_eq!(rig.modern("tasks/get", json!({}))["error"]["code"], rpc::INVALID_PARAMS);
}

#[test]
fn a_client_without_the_extension_waits_and_hears_progress() {
    let rig = Rig::new();
    let gate = rig.fake.gate("boot");
    rig.fake.answer("boot", Ok(json!({"boot_s": 3})));
    let mut params = json!({"name": "boot", "arguments": {"device": "phone"}});
    params["_meta"] = Rig::meta(false);
    params["_meta"]["progressToken"] = json!("t1");
    let id = rig.send_raw("tools/call", params);
    let progress = rig.notification("notifications/progress");
    assert_eq!(progress["params"]["progressToken"], "t1");
    assert!(progress["params"]["message"].as_str().is_some_and(|m| m.starts_with("boot is still running")));
    gate.release();
    let response = rig.response(id);
    assert_eq!(
        (response["result"]["resultType"].clone(), response["result"]["structuredContent"]["boot_s"].clone()),
        (json!("complete"), json!(3))
    );
    // No token, no progress.
    rig.fake.gates.lock().expect("gates").clear();
    rig.modern("tools/call", json!({"name": "boot", "arguments": {"device": "phone"}}));
    assert!(rig.quiet());
}

#[test]
fn a_cancelled_request_is_not_answered() {
    let rig = Rig::new();
    let gate = rig.fake.gate("boot");
    let id = rig.send_modern("tools/call", json!({"name": "boot", "arguments": {"device": "phone"}}), false);
    // Wait until the call is running, then cancel it.
    while rig.fake.called("boot").is_empty() {
        std::thread::sleep(Duration::from_millis(5));
    }
    rig.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id}}));
    gate.release();
    rig.server.drain(WAIT);
    assert!(rig.quiet(), "nothing is sent for a cancelled request");
    // The server still works.
    assert_eq!(rig.modern("tools/list", json!({}))["result"]["resultType"], "complete");
    // Cancelling something unknown or sending other notifications is harmless.
    rig.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 999}}));
    rig.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {}}));
    rig.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    assert!(rig.quiet());
}

#[test]
fn listening_acknowledges_first_then_sends_what_changed() {
    let rig = Rig::new();
    rig.fake.answer("info", Ok(json!({"state": "Shutdown"})));
    let id = rig.send_modern(
        "subscriptions/listen",
        json!({"notifications": {"resourceSubscriptions": ["simee://devices/phone", "file:///not-ours"], "resourcesListChanged": true, "toolsListChanged": true}}),
        false,
    );
    let ack = rig.next_message().expect("acknowledgement");
    assert_eq!(ack["method"], "notifications/subscriptions/acknowledged");
    assert_eq!(ack["params"]["_meta"][META_SUBSCRIPTION], id);
    // Only what the server will send: the tools list never changes, and the foreign URI isn't ours.
    assert_eq!(ack["params"]["notifications"], json!({"resourcesListChanged": true, "resourceSubscriptions": ["simee://devices/phone"]}));
    rig.until_read("info");
    rig.until_read("devices");
    rig.fake.answer("info", Ok(json!({"state": "Booted"})));
    let updated = rig.notification("notifications/resources/updated");
    assert_eq!(
        (updated["params"]["uri"].clone(), updated["params"]["_meta"][META_SUBSCRIPTION].clone()),
        (json!("simee://devices/phone"), json!(id))
    );
    rig.fake.answer("devices", Ok(json!([])));
    let listed = rig.notification("notifications/resources/list_changed");
    assert_eq!(listed["params"]["_meta"][META_SUBSCRIPTION], id);
    // Cancelling the listen stops it, and nothing answers the request.
    rig.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id}}));
    std::thread::sleep(SHORT);
    while rig.rx.try_recv().is_ok() {}
    rig.fake.answer("info", Ok(json!({"state": "Shutdown"})));
    assert!(rig.quiet(), "no more notifications");
}

#[test]
fn a_listener_hears_when_its_task_finishes() {
    let rig = Rig::new();
    let gate = rig.fake.gate("boot");
    let created = rig.with_tasks("tools/call", json!({"name": "boot", "arguments": {"device": "phone"}}));
    let task = created["result"]["taskId"].as_str().expect("task").to_owned();
    let id = rig.send_modern("subscriptions/listen", json!({"notifications": {"taskIds": [task, "unknown"]}}), true);
    let ack = rig.next_message().expect("ack");
    assert_eq!(ack["params"]["notifications"], json!({"taskIds": [task]}));
    gate.release();
    let note = rig.notification("notifications/tasks");
    assert_eq!((note["params"]["taskId"].clone(), note["params"]["status"].clone()), (json!(task), json!("completed")));
    assert_eq!(note["params"]["_meta"][META_SUBSCRIPTION], id);
    assert!(note["params"]["result"].is_object());
}

#[test]
fn listening_to_tasks_needs_the_extension() {
    let rig = Rig::new();
    let response = rig.modern("subscriptions/listen", json!({"notifications": {"taskIds": ["x"]}}));
    assert_eq!(response["error"]["code"], rpc::MISSING_CAPABILITY);
    assert_eq!(response["error"]["data"]["requiredCapabilities"]["extensions"][EXT_TASKS], json!({}));
    assert_eq!(rig.modern("subscriptions/listen", json!({"notifications": 3}))["error"]["code"], rpc::INVALID_PARAMS);
}

#[test]
fn an_older_client_opens_with_initialize_and_is_served_in_its_shape() {
    let rig = Rig::new();
    let id = rig
        .send_raw("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    let init = rig.response(id);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "simee");
    assert_eq!(init["result"].get("resultType"), None, "an older revision's result has no resultType");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    rig.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    // Requests carry no `_meta` now; results are the older shape.
    let id = rig.send_raw("tools/list", json!({}));
    let tools = rig.response(id);
    assert_eq!(tools["result"]["tools"].as_array().map(Vec::len), Some(METHODS.len() + 1));
    assert_eq!(tools["result"].get("ttlMs"), None);
    let id = rig.send_raw("tools/call", json!({"name": "devices", "arguments": {}}));
    let devices = rig.response(id);
    assert_eq!(devices["result"]["structuredContent"]["result"][0]["udid"], "phone", "structured content is an object");
    // A long method just blocks, with no task, even if capabilities claimed the extension.
    rig.fake.answer("boot", Ok(json!({"boot_s": 1})));
    let id = rig.send_raw("tools/call", json!({"name": "boot", "arguments": {"device": "phone"}}));
    assert_eq!(rig.response(id)["result"]["structuredContent"]["boot_s"], 1);
    // Tasks and listening are the new revision's.
    let id = rig.send_raw("tasks/get", json!({"taskId": "x"}));
    assert_eq!(rig.response(id)["error"]["code"], -32601);
    let id = rig.send_raw("ping", json!({}));
    assert_eq!(rig.response(id)["result"], json!({}));
    // Unknown revisions get the newest legacy one.
    let id = rig.send_raw("initialize", json!({"protocolVersion": "2099-01-01"}));
    assert_eq!(rig.response(id)["result"]["protocolVersion"], LEGACY[0]);
}

#[test]
fn an_older_client_subscribes_to_resources_the_old_way() {
    let rig = Rig::new();
    let id = rig.send_raw("initialize", json!({"protocolVersion": "2025-11-25", "capabilities": {}}));
    rig.response(id);
    rig.fake.answer("info", Ok(json!({"state": "Shutdown"})));
    let id = rig.send_raw("resources/subscribe", json!({"uri": "simee://devices/phone"}));
    assert_eq!(rig.response(id)["result"], json!({}));
    rig.until_read("info");
    rig.fake.answer("info", Ok(json!({"state": "Booted"})));
    let note = rig.notification("notifications/resources/updated");
    assert_eq!(note["params"], json!({"uri": "simee://devices/phone"}));
    let id = rig.send_raw("resources/subscribe", json!({"uri": "file:///x"}));
    assert_eq!(rig.response(id)["error"]["code"], rpc::INVALID_PARAMS);
    let id = rig.send_raw("resources/subscribe", json!({}));
    assert_eq!(rig.response(id)["error"]["code"], rpc::INVALID_PARAMS);
    let id = rig.send_raw("resources/unsubscribe", json!({"uri": "simee://devices/phone"}));
    assert_eq!(rig.response(id)["result"], json!({}));
    std::thread::sleep(SHORT);
    while rig.rx.try_recv().is_ok() {}
    rig.fake.answer("info", Ok(json!({"state": "Shutdown"})));
    assert!(rig.quiet(), "unsubscribed");
    // Modern requests don't have it.
    assert_eq!(rig.modern("resources/subscribe", json!({"uri": "simee://devices"}))["error"]["code"], -32601);
}

#[test]
fn serving_lines_answers_each_and_drains_before_returning() {
    let fake = Arc::new(Fake::with_devices());
    let (tx, rx) = channel::<String>();
    let tx = Mutex::new(tx);
    let sink: Sink = Arc::new(move |line| drop(tx.lock().expect("tx").send(line)));
    let meta = Rig::meta(false);
    let input = format!(
        "{}\n\n{}\n{}\n",
        json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {"_meta": meta}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "devices", "_meta": meta}}),
        "not json",
    );
    crate::serve(fake, std::io::Cursor::new(input), sink).expect("served");
    let lines: Vec<Value> = rx.try_iter().map(|l| serde_json::from_str(&l).expect("json")).collect();
    // Input ended; everything it asked was answered first.
    assert_eq!(lines.len(), 3);
    assert!(lines.iter().any(|l| l["id"] == 1 && l["result"]["supportedVersions"].is_array()));
    assert!(lines.iter().any(|l| l["id"] == 2 && l["result"]["structuredContent"].is_array()));
    assert!(lines.iter().any(|l| l["id"].is_null() && l["error"]["code"] == rpc::PARSE_ERROR));
}
