//! Questions the agents are waiting on a person to answer.
//!
//! Each agent call that needs a person becomes one prompt with a small
//! increasing id, carried in `state` until it is answered, cancelled or times
//! out. The agent method itself is parked on the prompt's responder; whatever
//! removes the prompt answers it, so no path leaves BlueZ waiting on a dialog
//! nobody can see. Overskride's could: its wait spins without yielding while
//! a dialog is open, so BlueZ's Cancel never reaches it (overskride 0.6.5,
//! src/bluetooth/agent.rs:178-187 -- read, not run).

use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How long a pairing request waits before the daemon rejects it itself.
/// BlueZ cancels an agent request at 60 s (src/agent.c, REQUEST_TIMEOUT); five
/// seconds more means BlueZ's Cancel normally arrives first and this is only
/// the backstop for a BlueZ that never sends one. Letting BlueZ decide is safe
/// because BlueZ hands its own timeout to the request as an error, a refusal
/// (src/agent.c:338-349).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(65);

/// What `expires` promises the person for a pairing request: BlueZ's own
/// limit, not the backstop above.
pub const SHOWN_TIMEOUT_MS: i64 = 60_000;

/// How long a push waits before the daemon refuses it -- BEFORE obexd's own
/// 60 s (obexd/src/manager.c:43, TIMEOUT), the reverse of a pairing request,
/// because obexd's timeout is an acceptance. Its agent_reply
/// (manager.c:642-645) sets auth_reject only for org.bluez.obex.Error.Rejected
/// and merely calls agent_cancel on NoReply; manager_request_authorization
/// then refuses only `if (!agent || agent->auth_reject)` (manager.c:736-737),
/// and otherwise returns 0 with no folder or name, so opp_chkput saves the
/// file under obexd's root folder with the sender's name
/// (obexd/plugins/opp.c:73-77). Five seconds is room for this reply to cross
/// the bus first.
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(55);

/// A display-only prompt has nothing to answer, so nothing else ends it if
/// the pairing quietly succeeds or fails without a Cancel.
pub const DISPLAY_LIFETIME: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pin,
    Passkey,
    Confirm,
    Authorize,
    Service,
    DisplayPin,
    DisplayPasskey,
    Push,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Pin => "pin",
            Kind::Passkey => "passkey",
            Kind::Confirm => "confirm",
            Kind::Authorize => "authorize",
            Kind::Service => "service",
            Kind::DisplayPin => "displayPin",
            Kind::DisplayPasskey => "displayPasskey",
            Kind::Push => "push",
        }
    }

    pub fn is_display(self) -> bool {
        matches!(self, Kind::DisplayPin | Kind::DisplayPasskey)
    }

    pub fn origin(self) -> Origin {
        if self == Kind::Push {
            Origin::Obex
        } else {
            Origin::Bluez
        }
    }
}

/// Which agent a prompt belongs to, because each agent's `Cancel` cancels
/// only its own requests: obexd giving up on a push says nothing about a
/// pairing in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Bluez,
    Obex,
}

/// What the agent method is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Yes, with the PIN or passkey digits, or the path to save a push to.
    Accept(String),
    Reject,
    Cancel,
}

pub type Responder = async_channel::Sender<Answer>;

/// The device a prompt is about, as the snapshot names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subject {
    pub device: String,
    pub address: String,
    pub name: String,
}

/// The kind-specific fields. `passkey` is the six-digit string for confirm and
/// displayPasskey, and the PIN itself for displayPin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    pub passkey: String,
    pub entered: Option<u16>,
    pub uuid: String,
    pub file: String,
    pub size: Option<u64>,
    /// For a push: the obexd transfer object it authorises.
    pub transfer: String,
}

#[derive(Debug)]
pub struct Prompt {
    pub id: u64,
    pub kind: Kind,
    pub subject: Subject,
    pub details: Details,
    pub expires: i64,
    deadline: Instant,
    responder: Option<Responder>,
}

impl Prompt {
    pub fn json(&self) -> Value {
        let d = &self.details;
        json!({
            "id": self.id,
            "kind": self.kind.as_str(),
            "device": self.subject.device,
            "address": self.subject.address,
            "name": self.subject.name,
            "passkey": d.passkey,
            "entered": d.entered,
            "uuid": d.uuid,
            "file": d.file,
            "size": d.size,
            "expires": self.expires,
        })
    }

    /// Answer the waiting agent call, if there is one. A responder whose call
    /// has gone away is not an error: there is nobody left to tell.
    pub fn answer(self, answer: Answer) {
        if let Some(r) = self.responder {
            let _ = r.try_send(answer);
        }
    }

    /// What a prompt that nobody answered in time is told. A Reject for every
    /// kind: for a push anything else is a yes (see `PUSH_TIMEOUT`).
    pub fn time_out(self) {
        self.answer(Answer::Reject)
    }
}

/// BlueZ carries passkeys as integers, so the leading zeros of `000042` are
/// this function's to put back.
pub fn passkey6(n: u32) -> String {
    format!("{n:06}")
}

/// Turn an `answer` op into what the agent replies with, or say why not.
pub fn answer_for(kind: Kind, accept: bool, value: Option<&str>) -> Result<Answer, String> {
    if !accept {
        return Ok(Answer::Reject);
    }
    match kind {
        Kind::Pin => {
            let v = value.unwrap_or("");
            // Bytes, not characters: BlueZ refuses a PIN longer than 16 bytes
            // (src/agent.c, pincode_reply), which is the legacy-pairing limit.
            if v.is_empty() || v.len() > 16 {
                return Err("a PIN is 1 to 16 characters".into());
            }
            if v.chars().any(char::is_control) {
                return Err("a PIN cannot hold control characters".into());
            }
            Ok(Answer::Accept(v.to_string()))
        }
        Kind::Passkey => {
            let v = value.unwrap_or("");
            if v.is_empty() || v.len() > 6 || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err("a passkey is 1 to 6 digits".into());
            }
            // 000000 is a passkey. Overskride turned a failed parse into 0 and
            // then refused 0, so it could not be entered (src/bluetooth/
            // agent.rs:42-61, read). Six digits cannot exceed 999999.
            let n: u32 = v.parse().map_err(|_| "a passkey is 1 to 6 digits".to_string())?;
            Ok(Answer::Accept(n.to_string()))
        }
        _ => Ok(Answer::Accept(String::new())),
    }
}

/// The open prompts.
#[derive(Debug, Default)]
pub struct Book {
    next: u64,
    list: Vec<Prompt>,
}

impl Book {
    /// Open a prompt that waits for an answer.
    pub fn open(
        &mut self,
        kind: Kind,
        subject: Subject,
        details: Details,
        responder: Responder,
        now: Instant,
        now_ms: i64,
    ) -> u64 {
        // A push card counts down to the daemon's own refusal, which is the
        // last moment it can be answered at all.
        let (deadline, shown) = if kind == Kind::Push {
            (PUSH_TIMEOUT, PUSH_TIMEOUT.as_millis() as i64)
        } else {
            (REQUEST_TIMEOUT, SHOWN_TIMEOUT_MS)
        };
        self.next += 1;
        self.list.push(Prompt {
            id: self.next,
            kind,
            subject,
            details,
            expires: now_ms + shown,
            deadline: now + deadline,
            responder: Some(responder),
        });
        self.next
    }

    /// Show a display-only prompt, or update the one already showing for this
    /// device: BlueZ calls DisplayPasskey again for every key the person types
    /// on the remote keyboard, and a new card per keystroke is noise.
    /// Returns the id and whether it is new.
    pub fn display(
        &mut self,
        kind: Kind,
        subject: Subject,
        passkey: String,
        entered: Option<u16>,
        now: Instant,
        now_ms: i64,
    ) -> (u64, bool) {
        if let Some(p) =
            self.list.iter_mut().find(|p| p.kind == kind && p.subject.device == subject.device)
        {
            p.details.passkey = passkey;
            p.details.entered = entered;
            return (p.id, false);
        }
        self.next += 1;
        self.list.push(Prompt {
            id: self.next,
            kind,
            subject,
            details: Details { passkey, entered, ..Details::default() },
            expires: now_ms + DISPLAY_LIFETIME.as_millis() as i64,
            deadline: now + DISPLAY_LIFETIME,
            responder: None,
        });
        (self.next, true)
    }

    pub fn get(&self, id: u64) -> Option<&Prompt> {
        self.list.iter().find(|p| p.id == id)
    }

    pub fn take(&mut self, id: u64) -> Option<Prompt> {
        let i = self.list.iter().position(|p| p.id == id)?;
        Some(self.list.remove(i))
    }

    fn take_where(&mut self, f: impl Fn(&Prompt) -> bool) -> Vec<Prompt> {
        let (gone, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.list).into_iter().partition(f);
        self.list = kept;
        gone
    }

    /// Everything one agent asked: what its `Cancel` and `Release` end.
    pub fn take_origin(&mut self, origin: Origin) -> Vec<Prompt> {
        self.take_where(|p| p.kind.origin() == origin)
    }

    /// The display prompts for a device that has finished pairing.
    pub fn take_display_for(&mut self, device: &str) -> Vec<Prompt> {
        self.take_where(|p| p.kind.is_display() && p.subject.device == device)
    }

    /// Prompts past their deadline, and prompts whose agent call has gone
    /// away (the connection it came on closed) and so can never be answered.
    pub fn take_expired(&mut self, now: Instant) -> Vec<Prompt> {
        self.take_where(|p| {
            p.deadline <= now || p.responder.as_ref().is_some_and(|r| r.is_closed())
        })
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.list.iter().map(|p| p.deadline).min()
    }

    pub fn json(&self) -> Vec<Value> {
        self.list.iter().map(Prompt::json).collect()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(dev: &str) -> Subject {
        Subject { device: dev.into(), address: "AA:BB:CC:DD:EE:FF".into(), name: "Phone".into() }
    }

    fn open(
        book: &mut Book,
        kind: Kind,
        dev: &str,
        now: Instant,
    ) -> (u64, async_channel::Receiver<Answer>) {
        let (tx, rx) = async_channel::bounded(1);
        (book.open(kind, subject(dev), Details::default(), tx, now, 1_000), rx)
    }

    #[test]
    fn passkeys_are_zero_padded_to_six_digits() {
        assert_eq!(passkey6(0), "000000");
        assert_eq!(passkey6(12345), "012345");
        assert_eq!(passkey6(999999), "999999");
    }

    #[test]
    fn ids_are_small_increasing_and_never_reused() {
        let mut b = Book::default();
        let now = Instant::now();
        let (a, _ra) = open(&mut b, Kind::Confirm, "/d1", now);
        let (c, _rc) = open(&mut b, Kind::Pin, "/d2", now);
        assert_eq!((a, c), (1, 2));
        b.take(a).unwrap().answer(Answer::Reject);
        let (d, _rd) = open(&mut b, Kind::Authorize, "/d1", now);
        assert_eq!(d, 3);
    }

    #[test]
    fn every_prompt_key_is_present() {
        let mut b = Book::default();
        let (_, _r) = open(&mut b, Kind::Confirm, "/d", Instant::now());
        let v = &b.json()[0];
        for k in [
            "id", "kind", "device", "address", "name", "passkey", "entered", "uuid", "file",
            "size", "expires",
        ] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert_eq!(v["entered"], Value::Null);
        assert_eq!(v["size"], Value::Null);
        assert_eq!(v["expires"], 1_000 + SHOWN_TIMEOUT_MS);
        assert_eq!(v["kind"], "confirm");
    }

    #[test]
    fn cancel_answers_every_request_of_that_agent_and_only_that_agent() {
        let mut b = Book::default();
        let now = Instant::now();
        let (_, r1) = open(&mut b, Kind::Confirm, "/d1", now);
        let (_, r2) = open(&mut b, Kind::Passkey, "/d2", now);
        let (_, r3) = open(&mut b, Kind::Push, "/d3", now);
        b.display(Kind::DisplayPasskey, subject("/d4"), "000123".into(), Some(0), now, 0);
        for p in b.take_origin(Origin::Bluez) {
            p.answer(Answer::Cancel);
        }
        assert_eq!(r1.try_recv(), Ok(Answer::Cancel));
        assert_eq!(r2.try_recv(), Ok(Answer::Cancel));
        assert!(r3.try_recv().is_err(), "the push belongs to obexd's agent");
        assert_eq!(b.len(), 1);
        assert_eq!(b.json()[0]["kind"], "push");
    }

    #[test]
    fn a_later_display_passkey_updates_the_same_prompt_in_place() {
        let mut b = Book::default();
        let now = Instant::now();
        let (id, new) =
            b.display(Kind::DisplayPasskey, subject("/d"), passkey6(42), Some(0), now, 0);
        assert!(new);
        let (again, new) =
            b.display(Kind::DisplayPasskey, subject("/d"), passkey6(42), Some(3), now, 0);
        assert_eq!((again, new), (id, false));
        assert_eq!(b.len(), 1);
        assert_eq!(b.json()[0]["entered"], 3);
        assert_eq!(b.json()[0]["passkey"], "000042");
        let (other, new) =
            b.display(Kind::DisplayPasskey, subject("/e"), passkey6(1), Some(0), now, 0);
        assert!(new && other != id);
    }

    #[test]
    fn a_device_that_paired_takes_its_display_prompts_with_it() {
        let mut b = Book::default();
        let now = Instant::now();
        b.display(Kind::DisplayPin, subject("/d"), "1234".into(), None, now, 0);
        let (_, _r) = open(&mut b, Kind::Confirm, "/d", now);
        assert_eq!(b.take_display_for("/d").len(), 1);
        assert_eq!(b.len(), 1, "the request stays: only Cancel or an answer ends it");
    }

    #[test]
    fn requests_time_out_as_rejections_after_bluez_gives_up() {
        let mut b = Book::default();
        let now = Instant::now();
        let (_, rc) = open(&mut b, Kind::Confirm, "/d", now);
        b.display(Kind::DisplayPasskey, subject("/e"), "000001".into(), Some(0), now, 0);
        assert!(b.take_expired(now + Duration::from_secs(30)).is_empty());
        assert_eq!(b.next_deadline(), Some(now + DISPLAY_LIFETIME));
        let gone = b.take_expired(now + DISPLAY_LIFETIME);
        assert_eq!(gone.len(), 1, "the display prompt lives 60 s");
        assert!(REQUEST_TIMEOUT > Duration::from_secs(60), "BlueZ's own Cancel comes first");
        let gone = b.take_expired(now + REQUEST_TIMEOUT);
        assert_eq!(gone.len(), 1);
        for p in gone {
            p.time_out();
        }
        assert_eq!(rc.try_recv(), Ok(Answer::Reject));
        assert!(b.is_empty());
        assert_eq!(b.next_deadline(), None);
    }

    #[test]
    fn a_push_is_refused_before_obexd_would_take_its_silence_as_a_yes() {
        let mut b = Book::default();
        let now = Instant::now();
        let (_, rp) = open(&mut b, Kind::Push, "/d", now);
        // obexd/src/manager.c:43: its AuthorizePush call times out at 60 s.
        let obexd_gives_up = Duration::from_secs(60);
        assert!(PUSH_TIMEOUT < obexd_gives_up, "{PUSH_TIMEOUT:?}");
        assert_eq!(b.next_deadline(), Some(now + PUSH_TIMEOUT));
        assert_eq!(
            b.json()[0]["expires"],
            1_000 + PUSH_TIMEOUT.as_millis() as i64,
            "the card counts down to the refusal"
        );
        assert!(b.take_expired(now + PUSH_TIMEOUT - Duration::from_millis(1)).is_empty());
        let gone = b.take_expired(now + PUSH_TIMEOUT);
        assert_eq!(gone.len(), 1);
        for p in gone {
            p.time_out();
        }
        assert_eq!(rp.try_recv(), Ok(Answer::Reject), "only a Rejected stops obexd saving it");
    }

    #[test]
    fn a_prompt_whose_call_went_away_is_dropped() {
        let mut b = Book::default();
        let now = Instant::now();
        let (_, r) = open(&mut b, Kind::Confirm, "/d", now);
        drop(r);
        assert_eq!(b.take_expired(now).len(), 1);
    }

    #[test]
    fn answers_are_validated_per_kind() {
        assert_eq!(answer_for(Kind::Confirm, true, None), Ok(Answer::Accept(String::new())));
        assert_eq!(answer_for(Kind::Confirm, false, Some("x")), Ok(Answer::Reject));
        assert_eq!(answer_for(Kind::Pin, true, Some("0000")), Ok(Answer::Accept("0000".into())));
        assert!(answer_for(Kind::Pin, true, Some("")).is_err());
        assert!(answer_for(Kind::Pin, true, None).is_err());
        assert!(answer_for(Kind::Pin, true, Some("12345678901234567")).is_err());
        assert!(answer_for(Kind::Pin, true, Some("12\n4")).is_err());
        assert_eq!(answer_for(Kind::Passkey, true, Some("000000")), Ok(Answer::Accept("0".into())));
        assert_eq!(
            answer_for(Kind::Passkey, true, Some("999999")),
            Ok(Answer::Accept("999999".into()))
        );
        assert_eq!(answer_for(Kind::Passkey, true, Some("42")), Ok(Answer::Accept("42".into())));
        assert!(answer_for(Kind::Passkey, true, Some("1000000")).is_err());
        assert!(answer_for(Kind::Passkey, true, Some("12a")).is_err());
        assert!(answer_for(Kind::Passkey, true, Some("-1")).is_err());
        assert!(answer_for(Kind::Passkey, true, None).is_err());
        assert_eq!(answer_for(Kind::Passkey, false, None), Ok(Answer::Reject));
    }
}
