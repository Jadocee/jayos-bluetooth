//! The D-Bus plumbing the daemon's threads share: names, calls that report
//! errors the way replies need them, and signal subscriptions decoded into
//! model updates.
//!
//! Every call goes through `call` rather than a generated proxy, because a
//! proxy's `fdo::Error` folds some D-Bus error names into its own variants and
//! a reply must carry the name BlueZ sent -- `org.bluez.Error.Failed` with
//! `br-connection-page-timeout` is the whole diagnosis, and the page maps it
//! to words.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::Sender;
use std::time::Duration;

use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type as MsgType;
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value};
use zbus::{DBusError, MatchRule, Message};

use crate::model::{props_from, Ifaces, Props};

pub const BLUEZ: &str = "org.bluez";
pub const OBEX: &str = "org.bluez.obex";
pub const AGENT_PATH: &str = "/org/jayos/bluetooth/agent";
pub const OBEX_AGENT_PATH: &str = "/org/jayos/bluetooth/obex";
pub const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
pub const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";

/// zbus has one method timeout per connection and none per call
/// (connection/builder.rs), so it is set for the slowest call made: Pair,
/// which waits on a person and then on bonding, and which BlueZ itself ends at
/// 60 s of agent silence.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A failed call, as a reply reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusError {
    pub name: String,
    pub message: String,
}

impl BusError {
    pub fn new(name: &str, message: impl Into<String>) -> BusError {
        BusError { name: name.to_string(), message: message.into() }
    }

    /// The service is not on the bus at all, as opposed to refusing.
    pub fn is_absent(&self) -> bool {
        matches!(
            self.name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
        )
    }
}

impl From<zbus::Error> for BusError {
    fn from(e: zbus::Error) -> BusError {
        match e {
            zbus::Error::MethodError(name, message, _) => {
                BusError { name: name.as_str().to_string(), message: message.unwrap_or_default() }
            }
            zbus::Error::FDO(f) => BusError {
                name: f.name().as_str().to_string(),
                message: f.description().unwrap_or("").to_string(),
            },
            zbus::Error::InputOutput(io) if io.kind() == std::io::ErrorKind::TimedOut => {
                BusError::new("org.freedesktop.DBus.Error.NoReply", "no reply in time")
            }
            other => BusError::new("org.freedesktop.DBus.Error.Failed", other.to_string()),
        }
    }
}

pub fn call<B>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> Result<Message, BusError>
where
    B: serde::Serialize + DynamicType,
{
    Ok(conn.call_method(Some(dest), path, Some(iface), method, body)?)
}

pub fn set_prop(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    prop: &str,
    value: Value<'_>,
) -> Result<(), BusError> {
    call(conn, dest, path, PROPERTIES, "Set", &(iface, prop, value)).map(|_| ())
}

pub fn get_all(conn: &Connection, dest: &str, path: &str, iface: &str) -> Result<Props, BusError> {
    let reply = call(conn, dest, path, PROPERTIES, "GetAll", &(iface,))?;
    let map: HashMap<String, OwnedValue> = reply.body().deserialize()?;
    Ok(props_from(&map))
}

type Managed = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

pub fn managed_objects(
    conn: &Connection,
    dest: &str,
) -> Result<BTreeMap<String, Ifaces>, BusError> {
    let reply = call(conn, dest, "/", OBJECT_MANAGER, "GetManagedObjects", &())?;
    let objects: Managed = reply.body().deserialize()?;
    Ok(objects
        .into_iter()
        .map(|(path, ifaces)| {
            (
                path.as_str().to_string(),
                ifaces.iter().map(|(i, p)| (i.clone(), props_from(p))).collect(),
            )
        })
        .collect())
}

/// The unique name owning `name`, or None when nobody does.
pub fn name_owner(conn: &Connection, name: &str) -> Option<String> {
    let reply = call(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetNameOwner",
        &(name,),
    )
    .ok()?;
    reply.body().deserialize::<String>().ok()
}

/// A signal, decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    Changed { path: String, iface: String, changed: Props, invalidated: Vec<String> },
    Added { path: String, ifaces: Ifaces },
    Removed { path: String, ifaces: Vec<String> },
    Owner { name: String, owner: Option<String> },
}

pub fn decode(msg: &Message) -> Option<Signal> {
    let hdr = msg.header();
    let member = hdr.member()?.as_str().to_string();
    let path = hdr.path().map(|p| p.as_str().to_string()).unwrap_or_default();
    let body = msg.body();
    match member.as_str() {
        "PropertiesChanged" => {
            let (iface, changed, invalidated): (String, HashMap<String, OwnedValue>, Vec<String>) =
                body.deserialize().ok()?;
            Some(Signal::Changed { path, iface, changed: props_from(&changed), invalidated })
        }
        "InterfacesAdded" => {
            let (obj, ifaces): (OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>) =
                body.deserialize().ok()?;
            Some(Signal::Added {
                path: obj.as_str().to_string(),
                ifaces: ifaces.iter().map(|(i, p)| (i.clone(), props_from(p))).collect(),
            })
        }
        "InterfacesRemoved" => {
            let (obj, ifaces): (OwnedObjectPath, Vec<String>) = body.deserialize().ok()?;
            Some(Signal::Removed { path: obj.as_str().to_string(), ifaces })
        }
        "NameOwnerChanged" => {
            let (name, _old, new): (String, String, String) = body.deserialize().ok()?;
            Some(Signal::Owner { name, owner: (!new.is_empty()).then_some(new) })
        }
        _ => None,
    }
}

/// The rules for one service: its property changes under `namespace`, its
/// object manager at `/`, and its name changing hands. `arg0` narrows the
/// property rule to one interface where only that one matters.
pub fn rules(
    service: &str,
    namespace: &str,
    arg0: Option<&str>,
) -> zbus::Result<Vec<MatchRule<'static>>> {
    let mut changed = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .sender(service.to_string())?
        .interface(PROPERTIES)?
        .member("PropertiesChanged")?
        .path_namespace(namespace.to_string())?;
    if let Some(a) = arg0 {
        changed = changed.arg(0, a.to_string())?;
    }
    let objects = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .sender(service.to_string())?
        .interface(OBJECT_MANAGER)?
        .path("/")?
        .build();
    let owner = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg(0, service.to_string())?
        .build();
    Ok(vec![changed.build(), objects, owner])
}

/// The bus driver, which alone sends NameOwnerChanged.
const DRIVER: &str = "org.freedesktop.DBus";

/// Subscribe to `rules` and forward each decoded signal from `service` to
/// `out` through `wrap`. Subscribing happens here, before this returns, so a
/// caller that then fetches the full object tree cannot miss a change made in
/// between. `lost` is sent once when the connection ends.
///
/// Every signal on the connection is read from one stream, in the order the
/// bus delivered them. A stream per rule, each on a thread of its own, lost
/// that order: obexd ends a transfer with PropertiesChanged (Status=complete)
/// and InterfacesRemoved straight after (obexd/client/transfer.c
/// xfer_complete, then gdbus/object.c remove_interface, which flushes the
/// properties first), the two arrived on different threads, and the core
/// sometimes saw the object removed before it saw it complete: a delivered
/// file reported as failed, 16 to 34 files in 300 against tests/daemon.rs's
/// mock obexd, and now and then a send never answered at all. That file's
/// `a_transfer_that_completes_and_is_removed_is_sent_every_time` holds it.
///
/// A signal is taken only from the unique name that owns `service`, and a
/// change of owner only from the bus driver. The bus applies a rule's sender
/// to broadcasts only: a signal addressed to this connection is delivered
/// whoever sent it, and zbus cannot match a well-known sender locally
/// (match_rule/mod.rs, `matches`). A forged PropertiesChanged sent straight
/// to the daemon marked a running send complete, and on the system bus the
/// default policy lets any user send a signal (dbus's system.conf,
/// `<allow send_type="signal"/>`).
pub fn pump<E: Send + 'static>(
    conn: &Connection,
    service: &str,
    rules: Vec<MatchRule<'static>>,
    out: Sender<E>,
    wrap: fn(Signal) -> E,
    lost: Option<E>,
) -> zbus::Result<()> {
    // The stream is drained from before the first AddMatch, by a thread that
    // only queues. zbus stops reading the socket while any stream's queue is
    // full (connection/socket_reader.rs), and the replies the calls below
    // wait for come through that socket.
    let stream = MessageIterator::from(conn);
    let (raw, received) = std::sync::mpsc::channel::<Message>();
    std::thread::Builder::new()
        .name("qs-bluetooth-bus".into())
        .spawn(move || {
            for msg in stream.flatten() {
                if msg.message_type() == MsgType::Signal && raw.send(msg).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    for rule in &rules {
        conn.call_method(
            Some(DRIVER),
            "/org/freedesktop/DBus",
            Some(DRIVER),
            "AddMatch",
            &(rule.to_string(),),
        )?;
    }
    // Asked after the AddMatch calls, so any change of owner after this
    // answer is in the stream. A signal queued before it from an owner that
    // has since gone is dropped, and the change of owner that follows it is
    // what the core acts on.
    let mut owner = name_owner(conn, service);
    let service = service.to_string();
    std::thread::Builder::new()
        .name("qs-bluetooth-signals".into())
        .spawn(move || {
            for msg in received {
                if !rules.iter().any(|r| r.matches(&msg).unwrap_or(false)) {
                    continue;
                }
                let Some(s) = decode(&msg) else { continue };
                let hdr = msg.header();
                let sender = hdr.sender().map(|s| s.as_str());
                match &s {
                    Signal::Owner { name, owner: new } => {
                        if sender != Some(DRIVER) {
                            continue;
                        }
                        if *name == service {
                            owner.clone_from(new);
                        }
                    }
                    _ if sender.is_none() || sender != owner.as_deref() => continue,
                    _ => {}
                }
                if out.send(wrap(s)).is_err() {
                    return;
                }
            }
            if let Some(l) = lost {
                let _ = out.send(l);
            }
        })
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_errors_keep_their_name_and_message() {
        let e = BusError::from(zbus::Error::Failure("x".into()));
        assert_eq!(e.name, "org.freedesktop.DBus.Error.Failed");
        let e = BusError::from(zbus::Error::InputOutput(std::sync::Arc::new(
            std::io::Error::from(std::io::ErrorKind::TimedOut),
        )));
        assert_eq!(e.name, "org.freedesktop.DBus.Error.NoReply");
        assert!(BusError::new("org.freedesktop.DBus.Error.ServiceUnknown", "").is_absent());
        assert!(!BusError::new("org.bluez.Error.Failed", "").is_absent());
    }

    #[test]
    fn the_rules_name_the_service_and_its_namespace() {
        let r = rules(OBEX, "/org/bluez/obex", Some("org.bluez.obex.Transfer1")).unwrap();
        let text: Vec<String> = r.iter().map(|m| m.to_string()).collect();
        assert!(text[0].contains("sender='org.bluez.obex'"), "{}", text[0]);
        assert!(text[0].contains("path_namespace='/org/bluez/obex'"), "{}", text[0]);
        assert!(text[0].contains("arg0='org.bluez.obex.Transfer1'"), "{}", text[0]);
        assert!(text[1].contains("interface='org.freedesktop.DBus.ObjectManager'"), "{}", text[1]);
        assert!(text[2].contains("arg0='org.bluez.obex'"), "{}", text[2]);
    }
}
