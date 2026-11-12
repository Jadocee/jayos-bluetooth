//! The two agents: BlueZ's pairing agent on the system bus, obexd's file
//! agent on the session bus.
//!
//! Every method is `async` and parks on a channel until the core answers.
//! That is not style. zbus runs a connection's object server on one executor,
//! and an agent method that blocked its thread waiting for a person would
//! stall it -- BlueZ's `Cancel` would then queue behind the very request it
//! cancels, and a Pair would hang until the person answered a dialog BlueZ had
//! already given up on. tests/daemon.rs holds it: with `wait` below made
//! blocking, its RequestPasskey-then-Cancel case never gets the Canceled
//! answer (run on 2026-10-08).
//!
//! The methods hold no state. They describe the call to the core thread and
//! wait for its answer; the core decides what is shown, what is accepted
//! silently, and when a request times out.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{interface, Connection};

use crate::daemon::Event;
use crate::model::{props_from, Props, Val};
use crate::prompts::{passkey6, Answer, Details, Kind, Origin, Responder};

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

/// obexd's agent errors. There is no `Canceled`, and its absence is the
/// point: obexd refuses a push only on `Rejected` and saves the file on any
/// other answer, `Canceled` and no answer included (see
/// `prompts::PUSH_TIMEOUT` for where in obexd), so every "do not receive"
/// this agent can say is a `Rejected`.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.obex.Error")]
pub enum ObexError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

/// What an agent tells the core.
#[derive(Debug)]
pub enum AgentEvent {
    Request {
        kind: Kind,
        device: String,
        details: Details,
        responder: Responder,
    },
    Display {
        kind: Kind,
        device: String,
        passkey: String,
        entered: Option<u16>,
    },
    Push {
        transfer: String,
        name: Option<String>,
        size: Option<u64>,
        destination: String,
        source: String,
        responder: Responder,
    },
    Cancel(Origin),
    Release(Origin),
}

/// The unique name currently owning a service, so an agent can refuse calls
/// from anybody else on the bus. On the system bus the D-Bus policy already
/// lets only bluetoothd call an Agent1; on the session bus any process of the
/// user's could otherwise put a "receive this file?" card on screen.
#[derive(Debug, Clone, Default)]
pub struct Owner(Arc<Mutex<Option<String>>>);

impl Owner {
    pub fn set(&self, name: Option<String>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = name;
    }

    pub fn get(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Whether the call came from `service`'s owner. A miss asks the bus before
/// refusing, because the cached owner can lag a restart by a signal.
async fn from_owner(conn: &Connection, hdr: &Header<'_>, owner: &Owner, service: &str) -> bool {
    let Some(sender) = hdr.sender() else { return false };
    if owner.get().as_deref() == Some(sender.as_str()) {
        return true;
    }
    let live = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &(service,),
        )
        .await
        .ok()
        .and_then(|m| m.body().deserialize::<String>().ok());
    match live {
        Some(l) => {
            let ok = l == sender.as_str();
            owner.set(Some(l));
            ok
        }
        None => false,
    }
}

async fn wait(rx: async_channel::Receiver<Answer>) -> Answer {
    // A closed channel means the core dropped the prompt without answering --
    // BlueZ went away, or the daemon is exiting -- which is a cancel.
    rx.recv().await.unwrap_or(Answer::Cancel)
}

/// `org.bluez.Agent1`, at `bus::AGENT_PATH` on the system bus.
pub struct PairingAgent {
    core: Sender<Event>,
    owner: Owner,
}

impl PairingAgent {
    pub fn new(core: Sender<Event>, owner: Owner) -> PairingAgent {
        PairingAgent { core, owner }
    }

    fn post(&self, e: AgentEvent) -> bool {
        self.core.send(Event::Agent(e)).is_ok()
    }

    async fn check(&self, conn: &Connection, hdr: &Header<'_>) -> Result<(), AgentError> {
        if from_owner(conn, hdr, &self.owner, crate::bus::BLUEZ).await {
            Ok(())
        } else {
            Err(AgentError::Rejected("only bluetoothd may call this agent".into()))
        }
    }

    async fn ask(
        &self,
        kind: Kind,
        device: OwnedObjectPath,
        details: Details,
    ) -> Result<String, AgentError> {
        let (tx, rx) = async_channel::bounded(1);
        if !self.post(AgentEvent::Request {
            kind,
            device: device.as_str().to_string(),
            details,
            responder: tx,
        }) {
            return Err(AgentError::Canceled("the daemon is exiting".into()));
        }
        match wait(rx).await {
            Answer::Accept(v) => Ok(v),
            Answer::Reject => Err(AgentError::Rejected("rejected".into())),
            Answer::Cancel => Err(AgentError::Canceled("cancelled".into())),
        }
    }
}

#[interface(name = "org.bluez.Agent1")]
impl PairingAgent {
    async fn release(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> Result<(), AgentError> {
        self.check(conn, &hdr).await?;
        self.post(AgentEvent::Release(Origin::Bluez));
        Ok(())
    }

    async fn request_pin_code(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        device: OwnedObjectPath,
    ) -> Result<String, AgentError> {
        self.check(conn, &hdr).await?;
        self.ask(Kind::Pin, device, Details::default()).await
    }

    async fn display_pin_code(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        device: OwnedObjectPath,
        pincode: String,
    ) -> Result<(), AgentError> {
        self.check(conn, &hdr).await?;
        // Answered at once: BlueZ only wants the PIN shown, and holding the
        // reply until somebody pressed OK, as overskride does
        // (src/bluetooth/agent.rs:63-73), delays the pairing behind a dialog.
        self.post(AgentEvent::Display {
            kind: Kind::DisplayPin,
            device: device.as_str().to_string(),
            passkey: pincode,
            entered: None,
        });
        Ok(())
    }

    async fn request_passkey(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        device: OwnedObjectPath,
    ) -> Result<u32, AgentError> {
        self.check(conn, &hdr).await?;
        let digits = self.ask(Kind::Passkey, device, Details::default()).await?;
        digits.parse().map_err(|_| AgentError::Rejected("not a passkey".into()))
    }

    async fn display_passkey(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        device: OwnedObjectPath,
        passkey: u32,
        entered: u16,
    ) -> Result<(), AgentError> {
        self.check(conn, &hdr).await?;
        self.post(AgentEvent::Display {
            kind: Kind::DisplayPasskey,
            device: device.as_str().to_string(),
            passkey: passkey6(passkey),
            entered: Some(entered),
        });
        Ok(())
    }

    async fn request_confirmation(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> Result<(), AgentError> {
        self.check(conn, &hdr).await?;
        let details = Details { passkey: passkey6(passkey), ..Details::default() };
        self.ask(Kind::Confirm, device, details).await.map(|_| ())
    }

    async fn request_authorization(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        device: OwnedObjectPath,
    ) -> Result<(), AgentError> {
        self.check(conn, &hdr).await?;
        self.ask(Kind::Authorize, device, Details::default()).await.map(|_| ())
    }

    async fn authorize_service(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        device: OwnedObjectPath,
        uuid: String,
    ) -> Result<(), AgentError> {
        self.check(conn, &hdr).await?;
        let details = Details { uuid, ..Details::default() };
        self.ask(Kind::Service, device, details).await.map(|_| ())
    }

    async fn cancel(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> Result<(), AgentError> {
        self.check(conn, &hdr).await?;
        self.post(AgentEvent::Cancel(Origin::Bluez));
        Ok(())
    }
}

/// `org.bluez.obex.Agent1`, at `bus::OBEX_AGENT_PATH` on the session bus.
pub struct ObexAgent {
    core: Sender<Event>,
    owner: Owner,
}

impl ObexAgent {
    pub fn new(core: Sender<Event>, owner: Owner) -> ObexAgent {
        ObexAgent { core, owner }
    }

    fn post(&self, e: AgentEvent) -> bool {
        self.core.send(Event::Agent(e)).is_ok()
    }

    async fn check(&self, conn: &Connection, hdr: &Header<'_>) -> Result<(), ObexError> {
        if from_owner(conn, hdr, &self.owner, crate::bus::OBEX).await {
            Ok(())
        } else {
            Err(ObexError::Rejected("only obexd may call this agent".into()))
        }
    }
}

async fn get_all(conn: &Connection, path: &str, iface: &str) -> Option<Props> {
    let reply = conn
        .call_method(
            Some(crate::bus::OBEX),
            path,
            Some(crate::bus::PROPERTIES),
            "GetAll",
            &(iface,),
        )
        .await
        .ok()?;
    let map: HashMap<String, OwnedValue> = reply.body().deserialize().ok()?;
    Some(props_from(&map))
}

fn text(p: &Props, k: &str) -> Option<String> {
    match p.get(k) {
        Some(Val::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

#[interface(name = "org.bluez.obex.Agent1")]
impl ObexAgent {
    async fn release(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> Result<(), ObexError> {
        self.check(conn, &hdr).await?;
        self.post(AgentEvent::Release(Origin::Obex));
        Ok(())
    }

    /// Where to save an incoming file, or a refusal. `Name` and `Size` are
    /// optional in obexd's Transfer1 and overskride `expect()`s both
    /// (src/obex/obex.rs:260-263); a missing one here is just a missing one.
    async fn authorize_push(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        transfer: OwnedObjectPath,
    ) -> Result<String, ObexError> {
        self.check(conn, &hdr).await?;
        // obexd keeps serving its main loop while it waits for this reply
        // (obexd/src/manager.c, manager_request_authorization), so asking it
        // about the transfer from inside the call does not deadlock.
        let Some(t) = get_all(conn, transfer.as_str(), "org.bluez.obex.Transfer1").await else {
            return Err(ObexError::Rejected("the transfer could not be read".into()));
        };
        let session = text(&t, "Session").unwrap_or_default();
        let s = if session.is_empty() {
            None
        } else {
            get_all(conn, &session, "org.bluez.obex.Session1").await
        };
        let s = s.unwrap_or_default();
        let size = match t.get("Size") {
            Some(Val::Int(n)) if *n >= 0 => Some(*n as u64),
            _ => None,
        };
        let (tx, rx) = async_channel::bounded(1);
        let event = AgentEvent::Push {
            transfer: transfer.as_str().to_string(),
            name: text(&t, "Name"),
            size,
            destination: text(&s, "Destination").unwrap_or_default(),
            source: text(&s, "Source").unwrap_or_default(),
            responder: tx,
        };
        if !self.post(event) {
            return Err(ObexError::Rejected("the daemon is exiting".into()));
        }
        // A Cancel -- a card that timed out, Incoming files switched off
        // while it was open, a receive folder that cannot be used, obexd's
        // own Cancel -- is a refusal here, or obexd saves the file anyway.
        match wait(rx).await {
            Answer::Accept(path) => Ok(path),
            Answer::Reject => Err(ObexError::Rejected("not authorised".into())),
            Answer::Cancel => Err(ObexError::Rejected("cancelled".into())),
        }
    }

    async fn cancel(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> Result<(), ObexError> {
        self.check(conn, &hdr).await?;
        self.post(AgentEvent::Cancel(Origin::Obex));
        Ok(())
    }
}
