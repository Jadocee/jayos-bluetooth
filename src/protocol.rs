//! The wire: one JSON object per line, commands in and events out.
//!
//! Parsing is strict about the fields an op needs and silent about everything
//! else. A missing or mistyped field is a `bad-request` reply naming it, never
//! a default -- `{"op":"power","adapter":"/org/bluez/hci0"}` with no `on` must
//! not switch the adapter off -- and an unknown field is ignored, so an older
//! daemon keeps working under a newer page.

use serde_json::{json, Map, Value};

/// What `files` in the `config` op chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Files {
    /// No OBEX agent at all, so obexd refuses every push itself.
    Off,
    /// A prompt for every push.
    Ask,
    /// Accept silently from a device whose `Trusted` is true; ask for the rest.
    Trusted,
}

impl Files {
    pub fn as_str(self) -> &'static str {
        match self {
            Files::Off => "off",
            Files::Ask => "ask",
            Files::Trusted => "trusted",
        }
    }

    pub fn parse(text: &str) -> Option<Files> {
        match text {
            "off" => Some(Files::Off),
            "ask" => Some(Files::Ask),
            "trusted" => Some(Files::Trusted),
            _ => None,
        }
    }
}

/// One command, with the fields its op needs.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Each field is optional: a page that only knows about `files` should not
    /// have to restate `agent` to change it.
    Config {
        agent: Option<bool>,
        files: Option<Files>,
        receive_dir: Option<String>,
    },
    Snapshot,
    Power {
        adapter: String,
        on: bool,
    },
    AdapterName {
        adapter: String,
        name: String,
    },
    Discoverable {
        adapter: String,
        on: bool,
    },
    DiscoverableTimeout {
        adapter: String,
        seconds: u32,
    },
    Pairable {
        adapter: String,
        on: bool,
    },
    Scan {
        adapter: String,
        on: bool,
    },
    Connect {
        device: String,
    },
    Disconnect {
        device: String,
    },
    Pair {
        device: String,
    },
    CancelPair {
        device: String,
    },
    Remove {
        device: String,
    },
    Trust {
        device: String,
        on: bool,
    },
    Block {
        device: String,
        on: bool,
    },
    Wake {
        device: String,
        on: bool,
    },
    Rename {
        device: String,
        name: String,
    },
    Send {
        device: String,
        files: Vec<String>,
    },
    Pick {
        device: String,
    },
    Cancel {
        transfer: String,
    },
    Dismiss {
        transfer: String,
    },
    Answer {
        prompt: u64,
        accept: bool,
        value: Option<String>,
    },
}

/// Every op name the daemon answers, in the order of the protocol table.
pub const OPS: &[&str] = &[
    "config",
    "snapshot",
    "power",
    "adapter-name",
    "discoverable",
    "discoverable-timeout",
    "pairable",
    "scan",
    "connect",
    "disconnect",
    "pair",
    "cancel-pair",
    "remove",
    "trust",
    "block",
    "wake",
    "rename",
    "send",
    "pick",
    "cancel",
    "dismiss",
    "answer",
];

/// A parsed command: the client's `id`, echoed in the reply, and the op.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub id: String,
    pub name: &'static str,
    pub op: Op,
}

/// Why a line was refused, with whatever `id` and `op` could be read from it
/// so the reply still reaches the call that sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub id: String,
    pub op: String,
    pub error: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn line(&self) -> String {
        reply_err(&self.id, &self.op, self.error, &self.message)
    }
}

/// Parse one line from a client.
pub fn parse_command(line: &str) -> Result<Command, Refusal> {
    let refuse = |id: &str, op: &str, error: &'static str, message: String| Refusal {
        id: id.to_string(),
        op: op.to_string(),
        error,
        message,
    };
    let value: Value = serde_json::from_str(line)
        .map_err(|e| refuse("", "", "bad-request", format!("not JSON: {e}")))?;
    let Value::Object(obj) = value else {
        return Err(refuse("", "", "bad-request", "a command is a JSON object".into()));
    };
    // A number is accepted as an id because a hand-typed command will use one;
    // anything else has no sensible echo and becomes "".
    let id = match obj.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let op_text = match obj.get("op") {
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(refuse(&id, "", "bad-request", "op must be a string".into())),
        None => return Err(refuse(&id, "", "bad-request", "missing field: op".into())),
    };
    let Some(&name) = OPS.iter().find(|o| **o == op_text) else {
        return Err(refuse(&id, &op_text, "unknown-op", format!("unknown op: {op_text}")));
    };
    let fields = Fields { obj: &obj };
    let op = fields.op(name).map_err(|message| refuse(&id, name, "bad-request", message))?;
    Ok(Command { id, name, op })
}

struct Fields<'a> {
    obj: &'a Map<String, Value>,
}

impl Fields<'_> {
    fn string(&self, key: &str) -> Result<String, String> {
        match self.obj.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(format!("{key} must be a string")),
            None => Err(format!("missing field: {key}")),
        }
    }

    /// A path field. An empty one names nothing and is refused here rather
    /// than reported later as a device that does not exist.
    fn path(&self, key: &str) -> Result<String, String> {
        let s = self.string(key)?;
        if s.is_empty() {
            return Err(format!("{key} must not be empty"));
        }
        Ok(s)
    }

    fn boolean(&self, key: &str) -> Result<bool, String> {
        match self.obj.get(key) {
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("{key} must be true or false")),
            None => Err(format!("missing field: {key}")),
        }
    }

    fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(_) => self.boolean(key).map(Some),
        }
    }

    fn optional_string(&self, key: &str) -> Result<Option<String>, String> {
        match self.obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(_) => self.string(key).map(Some),
        }
    }

    /// A whole number in range. JavaScript has one number type, so `180.0`
    /// is accepted as 180; `180.5` and `-1` are not.
    fn whole(&self, key: &str, max: u64) -> Result<u64, String> {
        let n = match self.obj.get(key) {
            Some(Value::Number(n)) => n,
            Some(_) => return Err(format!("{key} must be a number")),
            None => return Err(format!("missing field: {key}")),
        };
        let v = if let Some(u) = n.as_u64() {
            Some(u)
        } else {
            n.as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= max as f64)
                .map(|f| f as u64)
        };
        match v {
            Some(u) if u <= max => Ok(u),
            _ => Err(format!("{key} must be a whole number from 0 to {max}")),
        }
    }

    fn op(&self, name: &str) -> Result<Op, String> {
        let adapter = || self.path("adapter");
        let device = || self.path("device");
        Ok(match name {
            "config" => {
                let files =
                    match self.optional_string("files")? {
                        None => None,
                        Some(f) => Some(Files::parse(&f).ok_or_else(|| {
                            format!("files must be off, ask or trusted, not {f:?}")
                        })?),
                    };
                Op::Config {
                    agent: self.optional_bool("agent")?,
                    files,
                    receive_dir: self.optional_string("receiveDir")?,
                }
            }
            "snapshot" => Op::Snapshot,
            "power" => Op::Power { adapter: adapter()?, on: self.boolean("on")? },
            "adapter-name" => Op::AdapterName { adapter: adapter()?, name: self.string("name")? },
            "discoverable" => Op::Discoverable { adapter: adapter()?, on: self.boolean("on")? },
            "discoverable-timeout" => Op::DiscoverableTimeout {
                adapter: adapter()?,
                seconds: self.whole("seconds", u32::MAX as u64)? as u32,
            },
            "pairable" => Op::Pairable { adapter: adapter()?, on: self.boolean("on")? },
            "scan" => Op::Scan { adapter: adapter()?, on: self.boolean("on")? },
            "connect" => Op::Connect { device: device()? },
            "disconnect" => Op::Disconnect { device: device()? },
            "pair" => Op::Pair { device: device()? },
            "cancel-pair" => Op::CancelPair { device: device()? },
            "remove" => Op::Remove { device: device()? },
            "trust" => Op::Trust { device: device()?, on: self.boolean("on")? },
            "block" => Op::Block { device: device()?, on: self.boolean("on")? },
            "wake" => Op::Wake { device: device()?, on: self.boolean("on")? },
            "rename" => Op::Rename { device: device()?, name: self.string("name")? },
            "send" => {
                let files = match self.obj.get("files") {
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|v| v.as_str().map(str::to_string).ok_or("files must be strings"))
                        .collect::<Result<Vec<_>, _>>()?,
                    Some(_) => return Err("files must be a list of paths".into()),
                    None => return Err("missing field: files".into()),
                };
                if files.is_empty() {
                    return Err("files must name at least one file".into());
                }
                Op::Send { device: device()?, files }
            }
            "pick" => Op::Pick { device: device()? },
            "cancel" => Op::Cancel { transfer: self.path("transfer")? },
            "dismiss" => Op::Dismiss { transfer: self.path("transfer")? },
            "answer" => Op::Answer {
                prompt: self.whole("prompt", u64::MAX)?,
                accept: self.boolean("accept")?,
                value: self.optional_string("value")?,
            },
            other => return Err(format!("unknown op: {other}")),
        })
    }
}

// ── events ──────────────────────────────────────────────────────────────────

/// `{"event":"reply","id":..,"op":..,"ok":true}` plus any extra fields.
pub fn reply_ok(id: &str, op: &str, extra: Map<String, Value>) -> String {
    let mut m = Map::new();
    m.insert("event".into(), json!("reply"));
    m.insert("id".into(), json!(id));
    m.insert("op".into(), json!(op));
    m.insert("ok".into(), json!(true));
    for (k, v) in extra {
        m.insert(k, v);
    }
    Value::Object(m).to_string()
}

/// A failed reply: the D-Bus error name, or one of the daemon's own kebab
/// names, and the message that goes with it.
pub fn reply_err(id: &str, op: &str, error: &str, message: &str) -> String {
    reply_err_with(id, op, error, message, Map::new())
}

/// A failed reply that still reports what was done -- a `send` that got two
/// files of three across says which.
pub fn reply_err_with(
    id: &str,
    op: &str,
    error: &str,
    message: &str,
    extra: Map<String, Value>,
) -> String {
    let mut m = Map::new();
    m.insert("event".into(), json!("reply"));
    m.insert("id".into(), json!(id));
    m.insert("op".into(), json!(op));
    m.insert("ok".into(), json!(false));
    m.insert("error".into(), json!(error));
    m.insert("message".into(), json!(message));
    for (k, v) in extra {
        m.insert(k, v);
    }
    Value::Object(m).to_string()
}

/// The line a second daemon prints before it exits.
pub fn fatal(message: &str) -> String {
    json!({"event": "fatal", "message": message}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(line: &str) -> Command {
        parse_command(line).unwrap_or_else(|r| panic!("{line} refused: {r:?}"))
    }

    fn refused(line: &str) -> Refusal {
        match parse_command(line) {
            Ok(c) => panic!("{line} parsed as {c:?}"),
            Err(r) => r,
        }
    }

    #[test]
    fn every_op_in_the_table_parses_with_its_fields() {
        let cases = [
            (
                r#"{"op":"config","agent":true,"files":"trusted","receiveDir":"~/In"}"#,
                Op::Config {
                    agent: Some(true),
                    files: Some(Files::Trusted),
                    receive_dir: Some("~/In".into()),
                },
            ),
            (r#"{"op":"snapshot"}"#, Op::Snapshot),
            (
                r#"{"op":"power","adapter":"/a","on":false}"#,
                Op::Power { adapter: "/a".into(), on: false },
            ),
            (
                r#"{"op":"adapter-name","adapter":"/a","name":"box"}"#,
                Op::AdapterName { adapter: "/a".into(), name: "box".into() },
            ),
            (
                r#"{"op":"discoverable","adapter":"/a","on":true}"#,
                Op::Discoverable { adapter: "/a".into(), on: true },
            ),
            (
                r#"{"op":"discoverable-timeout","adapter":"/a","seconds":180}"#,
                Op::DiscoverableTimeout { adapter: "/a".into(), seconds: 180 },
            ),
            (
                r#"{"op":"pairable","adapter":"/a","on":true}"#,
                Op::Pairable { adapter: "/a".into(), on: true },
            ),
            (
                r#"{"op":"scan","adapter":"/a","on":true}"#,
                Op::Scan { adapter: "/a".into(), on: true },
            ),
            (r#"{"op":"connect","device":"/d"}"#, Op::Connect { device: "/d".into() }),
            (r#"{"op":"disconnect","device":"/d"}"#, Op::Disconnect { device: "/d".into() }),
            (r#"{"op":"pair","device":"/d"}"#, Op::Pair { device: "/d".into() }),
            (r#"{"op":"cancel-pair","device":"/d"}"#, Op::CancelPair { device: "/d".into() }),
            (r#"{"op":"remove","device":"/d"}"#, Op::Remove { device: "/d".into() }),
            (
                r#"{"op":"trust","device":"/d","on":true}"#,
                Op::Trust { device: "/d".into(), on: true },
            ),
            (
                r#"{"op":"block","device":"/d","on":false}"#,
                Op::Block { device: "/d".into(), on: false },
            ),
            (
                r#"{"op":"wake","device":"/d","on":true}"#,
                Op::Wake { device: "/d".into(), on: true },
            ),
            (
                r#"{"op":"rename","device":"/d","name":""}"#,
                Op::Rename { device: "/d".into(), name: String::new() },
            ),
            (
                r#"{"op":"send","device":"/d","files":["/a","/b"]}"#,
                Op::Send { device: "/d".into(), files: vec!["/a".into(), "/b".into()] },
            ),
            (r#"{"op":"pick","device":"/d"}"#, Op::Pick { device: "/d".into() }),
            (r#"{"op":"cancel","transfer":"/t"}"#, Op::Cancel { transfer: "/t".into() }),
            (r#"{"op":"dismiss","transfer":"/t"}"#, Op::Dismiss { transfer: "/t".into() }),
            (
                r#"{"op":"answer","prompt":3,"accept":true,"value":"000000"}"#,
                Op::Answer { prompt: 3, accept: true, value: Some("000000".into()) },
            ),
        ];
        assert_eq!(cases.len(), OPS.len(), "a case per op");
        for (line, want) in cases {
            assert_eq!(ok(line).op, want, "{line}");
        }
    }

    #[test]
    fn the_id_is_echoed_and_a_number_is_accepted_as_one() {
        assert_eq!(ok(r#"{"id":"7","op":"snapshot"}"#).id, "7");
        assert_eq!(ok(r#"{"id":12,"op":"snapshot"}"#).id, "12");
        assert_eq!(ok(r#"{"op":"snapshot"}"#).id, "");
        assert_eq!(ok(r#"{"id":{"x":1},"op":"snapshot"}"#).id, "");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let c = ok(r#"{"id":"1","op":"power","adapter":"/a","on":true,"colour":"blue"}"#);
        assert_eq!(c.op, Op::Power { adapter: "/a".into(), on: true });
    }

    #[test]
    fn a_missing_field_is_refused_and_named_never_defaulted() {
        let r = refused(r#"{"id":"9","op":"power","adapter":"/a"}"#);
        assert_eq!((r.id.as_str(), r.op.as_str(), r.error), ("9", "power", "bad-request"));
        assert!(r.message.contains("on"), "{}", r.message);
        let r = refused(r#"{"op":"connect"}"#);
        assert!(r.message.contains("device"));
        let r = refused(r#"{"op":"send","device":"/d"}"#);
        assert!(r.message.contains("files"));
        let r = refused(r#"{"op":"answer","prompt":1}"#);
        assert!(r.message.contains("accept"));
    }

    #[test]
    fn a_wrong_type_is_refused() {
        for line in [
            r#"{"op":"power","adapter":"/a","on":"yes"}"#,
            r#"{"op":"power","adapter":5,"on":true}"#,
            r#"{"op":"discoverable-timeout","adapter":"/a","seconds":"180"}"#,
            r#"{"op":"discoverable-timeout","adapter":"/a","seconds":-1}"#,
            r#"{"op":"discoverable-timeout","adapter":"/a","seconds":1.5}"#,
            r#"{"op":"discoverable-timeout","adapter":"/a","seconds":4294967296}"#,
            r#"{"op":"send","device":"/d","files":"/a"}"#,
            r#"{"op":"send","device":"/d","files":[1]}"#,
            r#"{"op":"send","device":"/d","files":[]}"#,
            r#"{"op":"config","files":"sometimes"}"#,
            r#"{"op":"config","agent":"on"}"#,
            r#"{"op":"answer","prompt":"3","accept":true}"#,
            r#"{"op":"connect","device":""}"#,
            r#"{"op":7}"#,
        ] {
            assert_eq!(refused(line).error, "bad-request", "{line}");
        }
    }

    #[test]
    fn a_whole_float_is_a_number_javascript_meant() {
        let c = ok(r#"{"op":"discoverable-timeout","adapter":"/a","seconds":180.0}"#);
        assert_eq!(c.op, Op::DiscoverableTimeout { adapter: "/a".into(), seconds: 180 });
    }

    #[test]
    fn config_fields_are_each_optional() {
        assert_eq!(
            ok(r#"{"op":"config","files":"off"}"#).op,
            Op::Config { agent: None, files: Some(Files::Off), receive_dir: None }
        );
        assert_eq!(
            ok(r#"{"op":"config"}"#).op,
            Op::Config { agent: None, files: None, receive_dir: None }
        );
    }

    #[test]
    fn an_unknown_op_is_its_own_error() {
        let r = refused(r#"{"id":"x","op":"teleport"}"#);
        assert_eq!((r.id.as_str(), r.op.as_str(), r.error), ("x", "teleport", "unknown-op"));
    }

    #[test]
    fn garbage_is_a_bad_request_and_still_a_reply() {
        for line in ["", "nope", "[1,2]", "\"op\"", r#"{"id":"1"}"#] {
            let r = refused(line);
            assert_eq!(r.error, "bad-request", "{line}");
            let v: Value = serde_json::from_str(&r.line()).unwrap();
            assert_eq!(v["event"], "reply");
            assert_eq!(v["ok"], false);
        }
    }

    #[test]
    fn replies_carry_every_field_a_client_reads() {
        let v: Value = serde_json::from_str(&reply_err(
            "7",
            "connect",
            "org.bluez.Error.Failed",
            "br-connection-page-timeout",
        ))
        .unwrap();
        assert_eq!(
            v,
            json!({"event":"reply","id":"7","op":"connect","ok":false,
            "error":"org.bluez.Error.Failed","message":"br-connection-page-timeout"})
        );
        let mut extra = Map::new();
        extra.insert("connected".into(), json!(true));
        let v: Value = serde_json::from_str(&reply_ok("8", "pair", extra)).unwrap();
        assert_eq!(v, json!({"event":"reply","id":"8","op":"pair","ok":true,"connected":true}));
        let v: Value =
            serde_json::from_str(&fatal("another jayos-bluetooth daemon is running")).unwrap();
        assert_eq!(v["event"], "fatal");
    }
}
