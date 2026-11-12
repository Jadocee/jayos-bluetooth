//! The daemon: one core thread that owns all state, and threads that feed it.
//!
//! Everything arrives as an `Event` on one channel -- a client's line, a
//! D-Bus signal, an agent asking a question, an action thread's result -- and
//! only the core thread reads or changes state, so there are no locks around
//! the model and no ordering between two writers to reason about.
//!
//! The core never calls BlueZ, obexd or the portal itself; the only calls it
//! makes are to the bus driver as a bus is taken on (AddMatch and
//! GetNameOwner, in `bus::pump`), which the bus answers itself. Short calls
//! (a property set, discovery, agent registration) go to a worker thread per
//! bus, which keeps them in order; long ones (Pair, Connect, a file transfer,
//! the portal) get a thread each. That is a requirement, not tidiness: Pair
//! does not return until the agent has been answered, the agent's question
//! reaches the person through the core, and a core blocked in Pair could
//! never forward it.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use zbus::blocking::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value as ZValue};

use crate::agent::{AgentEvent, ObexAgent, Owner, PairingAgent};
use crate::bus::{self, BusError, Signal, BLUEZ, OBEX};
use crate::discovery::{Change, Interest};
use crate::files::{self, DirEnv};
use crate::model::{props_from, Model, Props, Val, ADAPTER, DEVICE, TRANSPORT};
use crate::portal::{self, Picked};
use crate::prompts::{self, Answer, Book, Details, Kind, Origin, Responder, Subject};
use crate::protocol::{self, Command, Files, Op};
use crate::throttle::Throttle;
use crate::transfers::{Direction, Outcome, Start, Transfer, Transfers};

const TRANSFER1: &str = "org.bluez.obex.Transfer1";

/// A client's outgoing queue. A client that falls this far behind is not
/// reading, and is dropped rather than allowed to hold memory for ever.
const QUEUE: usize = 256;

/// The longest line a client may send. A `send` naming a few hundred files
/// fits with room to spare; anything longer is not a client of ours.
const MAX_LINE: u64 = 256 * 1024;

/// How long a failed bus connection waits before trying again.
const RECONNECT: Duration = Duration::from_secs(10);

/// How long obexd's absence is believed before the agent is registered again,
/// which D-Bus-activates obexd if it can be.
const OBEX_RETRY: Duration = Duration::from_secs(60);

/// The owner client: the shell that started the daemon, on stdin/stdout.
pub const OWNER: u64 = 0;

/// The ops that mark a device busy until their reply.
const BUSY_OPS: &[&str] =
    &["connect", "disconnect", "pair", "remove", "trust", "block", "wake", "rename"];

/// Everything the core thread is told.
pub enum Event {
    Line {
        client: u64,
        line: String,
    },
    ClientNew {
        client: u64,
        writer: SyncSender<Arc<str>>,
    },
    ClientGone {
        client: u64,
    },
    OwnerEof,
    Agent(AgentEvent),
    SystemUp(Connection),
    SessionUp(Connection),
    SystemLost,
    SessionLost,
    Bluez(Signal),
    Obex(Signal),
    BluezObjects(u64, Option<(String, BTreeMap<String, crate::model::Ifaces>)>),
    Registered {
        origin: Origin,
        on: bool,
        result: Result<Option<String>, BusError>,
    },
    Reply {
        client: u64,
        line: String,
        busy: Option<(String, u64)>,
    },
    /// `obexd` is the unique name that answered SendFile, which is how a
    /// transfer is told apart from one with the same path from another obexd.
    Started {
        batch: u64,
        index: usize,
        transfer: String,
        props: Props,
        obexd: Option<String>,
    },
    FileFailed {
        batch: u64,
        index: usize,
        error: BusError,
    },
    BatchDone {
        batch: u64,
        error: Option<BusError>,
    },
    Picked {
        client: u64,
        id: String,
        device: String,
        result: Result<Picked, BusError>,
    },
    Cancelled {
        client: u64,
        id: String,
        transfer: String,
        result: Result<(), BusError>,
    },
}

type Job = Box<dyn FnOnce(&Connection) + Send>;

struct Bus {
    conn: Connection,
    jobs: Sender<Job>,
}

impl Bus {
    fn new(conn: Connection) -> Bus {
        let (tx, rx) = mpsc::channel::<Job>();
        let c = conn.clone();
        std::thread::spawn(move || {
            for job in rx {
                job(&c);
            }
        });
        Bus { conn, jobs: tx }
    }

    fn run(&self, job: impl FnOnce(&Connection) + Send + 'static) {
        let _ = self.jobs.send(Box::new(job));
    }

    /// A call that may take seconds or minutes, on a thread of its own.
    fn spawn(&self, job: impl FnOnce(&Connection) + Send + 'static) {
        let c = self.conn.clone();
        std::thread::spawn(move || job(&c));
    }
}

/// One agent's registration, reconciled against what the config wants.
#[derive(Debug, Default)]
struct Slot {
    registered: bool,
    pending: bool,
    /// Registration failed or BlueZ released the agent: do not retry until
    /// something changes, or the daemon would loop on a refusal.
    stuck: bool,
    status: &'static str,
    error: String,
}

/// An outgoing `send`, from the request to RemoveSession.
struct Batch {
    client: u64,
    id: String,
    op: &'static str,
    device: String,
    address: String,
    name: String,
    files: Vec<String>,
    transfers: Vec<String>,
    sent: usize,
    /// Files offered to obexd, whether or not they went: the rest of a batch
    /// that stopped early was never tried, and only batch_done can say so.
    tried: usize,
    first_error: Option<BusError>,
    cancelled: Arc<AtomicBool>,
    tx: Sender<()>,
}

/// A transfer obexd has unregistered, with the last it said about it, kept
/// for a SendFile reply that reaches the core after the object has gone.
/// `obexd` is the instance it came from: obexd reuses paths across restarts,
/// so a path alone could be another process's transfer.
struct Grave {
    path: String,
    obexd: Option<String>,
    props: Props,
}

struct Config {
    agent: bool,
    files: Files,
    receive_dir: String,
}

struct Core {
    tx: Sender<Event>,
    clients: BTreeMap<u64, SyncSender<Arc<str>>>,
    config: Config,
    receive_dir: PathBuf,
    system: Option<Bus>,
    session: Option<Bus>,
    bluez_owner: Owner,
    obex_owner: Owner,
    /// The obexd this core has heard of, kept apart from `obex_owner`, which
    /// the agent threads also write: a new owner is judged against what the
    /// core itself last saw.
    obex_instance: Option<String>,
    bluez: bool,
    model: Model,
    /// The fetch of BlueZ's tree whose answer is awaited, by generation, and
    /// the BlueZ signals that arrived meanwhile. The answer comes on a worker
    /// thread and the signals on the pump's, so a change BlueZ signals just
    /// after its reply can reach the core first; dropped, an added device was
    /// missing until bluetoothd restarted. Replaying them on the snapshot is
    /// safe because gdbus sends every pending signal before a reply
    /// (gdbus/object.c:1628-1629, g_dbus_send_message), so those that came
    /// first are already in it and rebuild it unchanged.
    ///
    /// Only the answer to the newest fetch is used, and only while the
    /// bluetoothd it asked is still the owner. With bluetoothd arriving as the
    /// daemon started, two fetches were in flight; the second answered after
    /// the test's bluetoothd had left, and applied, it put back a bluetoothd
    /// that was not there (1 run in about 10 of
    /// `a_device_bluez_adds_just_after_answering_the_fetch_is_kept`).
    generation: u64,
    awaited: Option<u64>,
    held: Vec<Signal>,
    obex_model: Model,
    graveyard: VecDeque<Grave>,
    agent: Slot,
    obex: Slot,
    obex_retry: Option<Instant>,
    prompts: Book,
    transfers: Transfers,
    interest: Interest,
    /// Adapters somebody wants scanning whose discovery this daemon is
    /// holding back because audio is streaming over them. See
    /// `audio_yield`.
    yielded: BTreeSet<String>,
    busy: HashMap<String, Vec<(u64, &'static str)>>,
    busy_seq: u64,
    batches: HashMap<u64, Batch>,
    next_batch: u64,
    throttle: Throttle,
}

/// Run the daemon until stdin closes. Returns the process exit status.
pub fn run() -> u8 {
    let (tx, rx) = mpsc::channel::<Event>();

    // The lock and socket come first: a second daemon must give up before it
    // has touched either bus, or two agents would race for one prompt. It
    // gives up only after `LOCK_GRACE`, since the daemon holding the lock is
    // most often this shell's own from before a reload, on its way out. Stdin
    // is not read while it waits: what the shell writes meanwhile, its
    // `config` first, stays in the pipe and is read once the wait is over.
    // The bound socket is held here, not by the acceptor, so returning from
    // this function is what unlinks it.
    let _bound = match crate::control::Bound::take_in(
        &crate::control::run_dir(),
        crate::control::LOCK_GRACE,
    ) {
        Ok(bound) => {
            match bound.listener.try_clone() {
                Ok(listener) => accept(listener, tx.clone()),
                Err(e) => eprintln!("jayos-bluetooth: no control socket: {e}"),
            }
            Some(bound)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            println!("{}", protocol::fatal("another jayos-bluetooth daemon is running"));
            let _ = std::io::stdout().flush();
            return 1;
        }
        // Anything else -- a runtime directory somebody else owns -- costs the
        // socket clients, not the shell: prompts still reach it on stdout.
        Err(e) => {
            eprintln!("jayos-bluetooth: no control socket: {e}");
            None
        }
    };
    let (owner_out, owner_done) = owner_writer(tx.clone());
    owner_reader(tx.clone());

    let bluez_owner = Owner::default();
    let obex_owner = Owner::default();
    connect_system(tx.clone(), bluez_owner.clone());
    connect_session(tx.clone(), obex_owner.clone());

    let mut core = Core {
        tx: tx.clone(),
        clients: BTreeMap::new(),
        config: Config { agent: true, files: Files::Ask, receive_dir: String::new() },
        receive_dir: files::receive_dir("", &DirEnv::from_process()),
        system: None,
        session: None,
        bluez_owner,
        obex_owner,
        obex_instance: None,
        bluez: false,
        model: Model::default(),
        generation: 0,
        awaited: None,
        held: Vec::new(),
        obex_model: Model::default(),
        graveyard: VecDeque::new(),
        agent: Slot { status: "absent", ..Slot::default() },
        obex: Slot { status: "absent", ..Slot::default() },
        obex_retry: None,
        prompts: Book::default(),
        transfers: Transfers::default(),
        interest: Interest::default(),
        yielded: BTreeSet::new(),
        busy: HashMap::new(),
        busy_seq: 0,
        batches: HashMap::new(),
        next_batch: 0,
        throttle: Throttle::default(),
    };
    core.clients.insert(OWNER, owner_out);
    core.state_now();
    drop(tx);

    loop {
        let now = Instant::now();
        let wait = core
            .next_deadline()
            .map(|d| d.saturating_duration_since(now))
            .unwrap_or(Duration::from_secs(3600));
        match rx.recv_timeout(wait) {
            Ok(Event::OwnerEof) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(ev) => core.handle(ev),
            Err(RecvTimeoutError::Timeout) => {}
        }
        core.tick(Instant::now());
    }
    // What is already queued for the owner is written before exiting -- a
    // shell that closed stdin straight after a command still gets the reply --
    // but only briefly: an owner that has stopped reading would hold the exit
    // for ever, and an orphaned daemon is the one outcome stdin EOF exists to
    // prevent.
    drop(core);
    let _ = owner_done.recv_timeout(Duration::from_secs(1));
    0
}

// ── the threads that feed the core ──────────────────────────────────────────

/// The owner's queue, and a channel that closes once everything queued has
/// been written.
fn owner_writer(tx: Sender<Event>) -> (SyncSender<Arc<str>>, Receiver<()>) {
    let (out, rx) = mpsc::sync_channel::<Arc<str>>(QUEUE);
    let (done_tx, done) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _done = done_tx;
        let stdout = std::io::stdout();
        for line in rx {
            let mut lock = stdout.lock();
            if writeln!(lock, "{line}").and_then(|()| lock.flush()).is_err() {
                // The shell stopped reading: it has gone, and so should we.
                let _ = tx.send(Event::OwnerEof);
                return;
            }
        }
    });
    (out, done)
}

/// Read lines from `input` until EOF, posting each as a command from `client`.
fn read_lines(input: impl Read, client: u64, tx: &Sender<Event>) {
    let mut reader = BufReader::new(input);
    loop {
        let mut buf = Vec::new();
        // `take` bounds the read: a client that never sends a newline would
        // otherwise grow this buffer until the machine noticed.
        match (&mut reader).take(MAX_LINE).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if buf.last() != Some(&b'\n') && buf.len() as u64 >= MAX_LINE {
            return;
        }
        let line = String::from_utf8_lossy(&buf).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if tx.send(Event::Line { client, line }).is_err() {
            return;
        }
    }
}

fn owner_reader(tx: Sender<Event>) {
    std::thread::spawn(move || {
        read_lines(std::io::stdin().lock(), OWNER, &tx);
        let _ = tx.send(Event::OwnerEof);
    });
}

fn accept(listener: UnixListener, tx: Sender<Event>) {
    let next = Arc::new(AtomicU64::new(OWNER + 1));
    std::thread::spawn(move || {
        // SAFETY: getuid cannot fail.
        let ours = unsafe { libc::getuid() };
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if crate::control::peer_uid(&stream) != Some(ours) {
                continue;
            }
            let client = next.fetch_add(1, Ordering::Relaxed);
            let Ok(write_half) = stream.try_clone() else { continue };
            let (out, rx) = mpsc::sync_channel::<Arc<str>>(QUEUE);
            // Registered before the reader starts, so its first line cannot
            // reach the core ahead of the client it came from.
            if tx.send(Event::ClientNew { client, writer: out }).is_err() {
                return;
            }
            std::thread::spawn(move || client_writer(write_half, rx));
            let tx = tx.clone();
            std::thread::spawn(move || {
                read_lines(&stream, client, &tx);
                let _ = stream.shutdown(std::net::Shutdown::Both);
                let _ = tx.send(Event::ClientGone { client });
            });
        }
    });
}

fn client_writer(mut stream: UnixStream, rx: Receiver<Arc<str>>) {
    // A client that stops reading must not hold this thread for ever.
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    for line in rx {
        if writeln!(stream, "{line}").and_then(|()| stream.flush()).is_err() {
            break;
        }
    }
    // The queue closed (the core dropped this client) or the write failed:
    // either way the reader must stop too.
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Connect to a bus on a thread of its own, retrying until it answers, and
/// post the connection to the core. A machine without a system bus is not an
/// error worth exiting over: the state says `bluez:false` until one appears.
fn connect_bus(
    name: &'static str,
    tx: Sender<Event>,
    build: impl Fn() -> zbus::Result<Connection> + Send + 'static,
    up: fn(Connection) -> Event,
) {
    std::thread::spawn(move || {
        let mut said = false;
        loop {
            match build() {
                Ok(conn) => {
                    let _ = tx.send(up(conn));
                    return;
                }
                Err(e) if !said => {
                    eprintln!("jayos-bluetooth: {name} bus: {e}");
                    said = true;
                }
                Err(_) => {}
            }
            std::thread::sleep(RECONNECT);
        }
    });
}

/// The system bus, with the pairing agent served on it.
fn connect_system(tx: Sender<Event>, owner: Owner) {
    let core = tx.clone();
    connect_bus(
        "system",
        tx,
        move || {
            zbus::blocking::connection::Builder::system()?
                .serve_at(bus::AGENT_PATH, PairingAgent::new(core.clone(), owner.clone()))?
                .method_timeout(bus::CALL_TIMEOUT)
                .build()
        },
        Event::SystemUp,
    );
}

/// The session bus, with obexd's file agent served on it.
fn connect_session(tx: Sender<Event>, owner: Owner) {
    let core = tx.clone();
    connect_bus(
        "session",
        tx,
        move || {
            zbus::blocking::connection::Builder::session()?
                .serve_at(bus::OBEX_AGENT_PATH, ObexAgent::new(core.clone(), owner.clone()))?
                .method_timeout(bus::CALL_TIMEOUT)
                .build()
        },
        Event::SessionUp,
    );
}

fn object_path(p: &str) -> Result<ObjectPath<'_>, BusError> {
    ObjectPath::try_from(p).map_err(|e| BusError::new("bad-request", e.to_string()))
}

fn base_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// The file a notice about a whole batch names: the one file, or none, which
/// the shell words as "a file".
fn only_file(files: &[String]) -> String {
    match files {
        [f] => base_name(f),
        _ => String::new(),
    }
}

/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF` -> `AA:BB:CC:DD:EE:FF`, for a
/// device the agent is asked about before the model has heard of it.
fn address_from_path(path: &str) -> String {
    path.rsplit('/')
        .next()
        .and_then(|s| s.strip_prefix("dev_"))
        .map(|s| s.replace('_', ":"))
        .unwrap_or_default()
}

// ── the core ────────────────────────────────────────────────────────────────

impl Core {
    fn next_deadline(&self) -> Option<Instant> {
        [
            self.prompts.next_deadline(),
            self.transfers.next_deadline(),
            self.throttle.deadline(),
            self.obex_retry,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn tick(&mut self, now: Instant) {
        let expired = self.prompts.take_expired(now);
        if !expired.is_empty() {
            for p in expired {
                p.time_out();
            }
            self.state_now();
        }
        if self.transfers.expire(now) {
            self.changed();
        }
        if self.obex_retry.is_some_and(|t| now >= t) {
            self.obex_retry = None;
            self.obex.stuck = false;
            self.reconcile_obex();
        }
        if self.throttle.due(now) {
            self.broadcast(self.state_line());
        }
    }

    // ── output ──

    fn state_line(&self) -> String {
        let (adapters, devices) = if self.bluez {
            let scanning = |p: &str| self.interest.wanted(p);
            let paused = |p: &str| self.yielded.contains(p);
            let busy = |p: &str| {
                self.busy
                    .get(p)
                    .and_then(|v| v.last())
                    .map(|(_, op)| op.to_string())
                    .unwrap_or_default()
            };
            (self.model.adapters_json(scanning, paused), self.model.devices_json(busy))
        } else {
            (Vec::new(), Vec::new())
        };
        json!({
            "event": "state",
            "version": 1,
            "bluez": self.bluez,
            "agent": self.agent.status,
            "agentError": self.agent.error,
            "obex": self.obex.status,
            "obexError": self.obex.error,
            "files": self.config.files.as_str(),
            "receiveDir": self.receive_dir.to_string_lossy(),
            "adapters": adapters,
            "devices": devices,
            "transfers": self.transfers.json(),
            "prompts": self.prompts.json(),
        })
        .to_string()
    }

    fn send_to(&mut self, client: u64, line: &str) {
        let Some(w) = self.clients.get(&client) else { return };
        match w.try_send(Arc::from(line)) {
            Ok(()) => {}
            // The owner is never dropped for being slow: losing a state line is
            // survivable, losing the shell's connection to its prompts is not.
            Err(TrySendError::Full(_)) if client == OWNER => {
                eprintln!("jayos-bluetooth: the shell is not reading")
            }
            Err(_) => self.drop_client(client),
        }
    }

    fn broadcast(&mut self, line: String) {
        let line: Arc<str> = Arc::from(line);
        let mut gone = Vec::new();
        for (&c, w) in &self.clients {
            match w.try_send(line.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) if c == OWNER => {
                    eprintln!("jayos-bluetooth: the shell is not reading")
                }
                Err(_) => gone.push(c),
            }
        }
        for c in gone {
            self.drop_client(c);
        }
    }

    /// Something in `state` changed; send it when the throttle allows.
    fn changed(&mut self) {
        if self.throttle.request(Instant::now()) {
            self.broadcast(self.state_line());
        }
    }

    /// Send `state` now: a prompt appeared or went, which must not wait.
    fn state_now(&mut self) {
        self.throttle.sent(Instant::now());
        self.broadcast(self.state_line());
    }

    fn reply(&mut self, client: u64, line: String) {
        self.send_to(client, &line);
    }

    fn reply_ok(&mut self, client: u64, id: &str, op: &str) {
        self.reply(client, protocol::reply_ok(id, op, Map::new()));
    }

    fn reply_err(&mut self, client: u64, id: &str, op: &str, error: &str, message: &str) {
        self.reply(client, protocol::reply_err(id, op, error, message));
    }

    /// A `notice` about a file and the device it went to or came from, with
    /// whatever else its kind carries in `extra`.
    fn notice(&mut self, kind: &str, file: &str, device: &str, name: &str, extra: Value) {
        let mut m = Map::new();
        m.insert("event".into(), json!("notice"));
        m.insert("kind".into(), json!(kind));
        m.insert("file".into(), json!(file));
        m.insert("device".into(), json!(device));
        m.insert("name".into(), json!(name));
        if let Value::Object(e) = extra {
            m.extend(e);
        }
        self.broadcast(Value::Object(m).to_string());
    }

    fn drop_client(&mut self, client: u64) {
        if client == OWNER || self.clients.remove(&client).is_none() {
            return;
        }
        for adapter in self.interest.drop_client(client) {
            if !self.yielded.remove(&adapter) {
                self.stop_discovery(adapter);
            }
        }
        self.changed();
    }

    // ── events ──

    fn handle(&mut self, ev: Event) {
        match ev {
            // A socket client already dropped for not reading still has a
            // reader thread posting its lines until the writer shuts the
            // socket. Run, its `scan on` took discovery interest that its
            // ClientGone could never give back, since that client was no
            // longer registered to be dropped.
            Event::Line { client, .. }
                if client != OWNER && !self.clients.contains_key(&client) => {}
            Event::Line { client, line } => self.command(client, &line),
            Event::ClientNew { client, writer } => {
                self.clients.insert(client, writer);
                let line = self.state_line();
                self.send_to(client, &line);
            }
            Event::ClientGone { client } => self.drop_client(client),
            Event::OwnerEof => {}
            Event::Agent(a) => self.agent_event(a),
            Event::SystemUp(conn) => self.system_up(conn),
            Event::SessionUp(conn) => self.session_up(conn),
            Event::SystemLost => {
                self.system = None;
                self.forget_fetches();
                self.bluez_gone();
                connect_system(self.tx.clone(), self.bluez_owner.clone());
            }
            Event::SessionLost => {
                self.session = None;
                self.obex_gone();
                connect_session(self.tx.clone(), self.obex_owner.clone());
            }
            Event::Bluez(s) => self.bluez_signal(s),
            Event::Obex(s) => self.obex_signal(s),
            Event::BluezObjects(generation, found) => self.bluez_objects(generation, found),
            Event::Registered { origin, on, result } => self.registered(origin, on, result),
            Event::Reply { client, line, busy } => {
                if let Some((device, seq)) = busy {
                    self.busy_end(&device, seq);
                }
                self.reply(client, line);
                self.changed();
            }
            Event::Started { batch, index, transfer, props, obexd } => {
                self.started(batch, index, transfer, props, obexd)
            }
            Event::FileFailed { batch, index, error } => self.file_failed(batch, index, error),
            Event::BatchDone { batch, error } => self.batch_done(batch, error),
            Event::Picked { client, id, device, result } => self.picked(client, id, device, result),
            Event::Cancelled { client, id, transfer, result } => {
                self.cancelled(client, &id, &transfer, result)
            }
        }
    }

    // ── the buses ──

    fn system_up(&mut self, conn: Connection) {
        let rules = match bus::rules(BLUEZ, "/org/bluez", None) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("jayos-bluetooth: {e}");
                return;
            }
        };
        if let Err(e) =
            bus::pump(&conn, BLUEZ, rules, self.tx.clone(), Event::Bluez, Some(Event::SystemLost))
        {
            eprintln!("jayos-bluetooth: system bus signals: {e}");
            return;
        }
        self.system = Some(Bus::new(conn));
        // After the subscriptions, so nothing changes unseen between the fetch
        // and the first signal.
        self.refetch_bluez();
    }

    fn session_up(&mut self, conn: Connection) {
        let rules = match bus::rules(OBEX, "/org/bluez/obex", Some(TRANSFER1)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("jayos-bluetooth: {e}");
                return;
            }
        };
        if let Err(e) =
            bus::pump(&conn, OBEX, rules, self.tx.clone(), Event::Obex, Some(Event::SessionLost))
        {
            eprintln!("jayos-bluetooth: session bus signals: {e}");
            return;
        }
        self.session = Some(Bus::new(conn));
        self.reconcile_obex();
        self.changed();
    }

    fn refetch_bluez(&mut self) {
        let Some(b) = &self.system else { return };
        self.generation += 1;
        let generation = self.generation;
        self.awaited = Some(generation);
        let tx = self.tx.clone();
        b.run(move |c| {
            // No owner means no bluetoothd, and the fetch is not attempted.
            let found = bus::name_owner(c, BLUEZ)
                .and_then(|owner| Some(owner).zip(bus::managed_objects(c, BLUEZ).ok()));
            let _ = tx.send(Event::BluezObjects(generation, found));
        });
    }

    /// Whatever is outstanding describes a bluetoothd, or a connection, that
    /// is no longer there.
    fn forget_fetches(&mut self) {
        self.generation += 1;
        self.awaited = None;
        self.held.clear();
    }

    fn bluez_objects(
        &mut self,
        generation: u64,
        found: Option<(String, BTreeMap<String, crate::model::Ifaces>)>,
    ) {
        if self.awaited != Some(generation) {
            return;
        }
        self.awaited = None;
        let held = std::mem::take(&mut self.held);
        let Some((owner, objects)) = found else {
            self.bluez_gone();
            return;
        };
        self.bluez_owner.set(Some(owner));
        self.bluez = true;
        self.model.replace(objects);
        for s in held {
            self.bluez_apply(s);
        }
        self.agent.registered = false;
        self.agent.pending = false;
        self.agent.stuck = false;
        self.reconcile_agent();
        for adapter in self.interest.adapters() {
            if self.model.bool(&adapter, ADAPTER, "Powered") == Some(true) {
                self.start_discovery(adapter, None);
            }
        }
        self.changed();
    }

    fn bluez_gone(&mut self) {
        // A state still folding goes out first. A graceful stop disconnects
        // bluetoothd's devices and powers its adapters off while it still owns
        // the name; folded into the state that says there is no BlueZ, those
        // departures reached no client, and the island never said them.
        if self.throttle.deadline().is_some() {
            self.state_now();
        }
        let had_prompts = !self.prompts.is_empty();
        self.bluez = false;
        self.bluez_owner.set(None);
        self.model.clear();
        // Nothing streams through a bluetoothd that is gone, and the next
        // one's transports decide afresh in bluez_objects.
        self.yielded.clear();
        self.agent.registered = false;
        self.agent.pending = false;
        self.agent.stuck = false;
        self.agent.error.clear();
        self.agent.status = if self.config.agent { "absent" } else { "off" };
        for p in self.prompts.take_origin(Origin::Bluez) {
            p.answer(Answer::Cancel);
        }
        if had_prompts {
            self.state_now();
        } else {
            self.changed();
        }
    }

    fn obex_gone(&mut self) {
        self.obex_owner.set(None);
        self.obex_instance = None;
        self.forget_obexd();
        self.obex.registered = false;
        self.obex.pending = false;
        self.obex.error.clear();
        self.obex.status = if self.obex_wanted() { "absent" } else { "off" };
        for p in self.prompts.take_origin(Origin::Obex) {
            p.answer(Answer::Cancel);
        }
        let now_ms = crate::now_ms();
        let now = Instant::now();
        let mut ended = Vec::new();
        for t in self.transfers.iter_mut() {
            if let Some(o) = t.fail("obexd stopped", now_ms, now) {
                ended.push((t.id.clone(), o));
            }
        }
        for (id, o) in ended {
            self.outcome(&id, o);
        }
        if self.obex_wanted() && self.session.is_some() {
            self.obex_retry = Some(Instant::now() + Duration::from_secs(5));
        }
        self.state_now();
    }

    /// What one obexd said about its transfers means nothing to the next:
    /// obexd numbers sessions and transfers from counters that start at 0 in
    /// each process (obexd/client/session.c:48, client/transfer.c:40), so a
    /// restarted obexd's first transfer has the path of the last one's first,
    /// and a `complete` kept from before the restart finished it at once --
    /// reported sent while obexd was still sending it.
    ///
    /// The graveyard is not cleared with the rest: its entries name the obexd
    /// they came from, and `started` reads only that one's. Cleared, a
    /// transfer that completed just before its obexd left -- its SendFile
    /// reply still on the way to the core -- had nothing left to say so, and
    /// its send waited for ever.
    fn forget_obexd(&mut self) {
        self.obex_model.clear();
    }

    fn bluez_signal(&mut self, s: Signal) {
        match s {
            // Everything held, and any fetch outstanding, belongs to the owner
            // before this one: one stream delivers its signals ahead of this.
            Signal::Owner { name, owner } if name == BLUEZ => {
                self.forget_fetches();
                match owner {
                    Some(_) => self.refetch_bluez(),
                    None => self.bluez_gone(),
                }
            }
            Signal::Owner { .. } => {}
            s if self.awaited.is_some() => self.held.push(s),
            s => self.bluez_apply(s),
        }
    }

    fn bluez_apply(&mut self, s: Signal) {
        match s {
            Signal::Owner { .. } => {}
            _ if !self.bluez => {}
            Signal::Changed { path, iface, changed, invalidated } => {
                let was_powered = self.model.bool(&path, ADAPTER, "Powered");
                let paired = iface == DEVICE && changed.get("Paired") == Some(&Val::Bool(true));
                let powered_on =
                    iface == ADAPTER && changed.get("Powered") == Some(&Val::Bool(true));
                let stream = iface == TRANSPORT && changed.contains_key("State");
                if !self.model.changed(&path, &iface, changed, &invalidated) {
                    return;
                }
                if stream {
                    self.audio_yield();
                }
                if powered_on && was_powered != Some(true) && self.interest.wanted(&path) {
                    self.start_discovery(path.clone(), None);
                }
                if paired && !self.prompts.take_display_for(&path).is_empty() {
                    self.state_now();
                    return;
                }
                self.changed();
            }
            Signal::Added { path, ifaces } => {
                let adapter = ifaces.contains_key(ADAPTER) && !self.model.has(&path, ADAPTER);
                let stream = ifaces.contains_key(TRANSPORT);
                self.model.add(&path, ifaces);
                if stream {
                    self.audio_yield();
                }
                // An adapter that comes back already powered -- re-enumerated
                // after a resume, a dongle plugged in again -- sends no
                // Powered change, so the arm above never sees it. Interest is
                // kept by path through its absence.
                if adapter
                    && self.model.bool(&path, ADAPTER, "Powered") == Some(true)
                    && self.interest.wanted(&path)
                {
                    self.start_discovery(path.clone(), None);
                }
                self.changed();
            }
            Signal::Removed { path, ifaces } => {
                let stream = ifaces.iter().any(|i| i == TRANSPORT);
                self.model.remove(&path, &ifaces);
                if stream {
                    self.audio_yield();
                }
                self.changed();
            }
        }
    }

    fn obex_signal(&mut self, s: Signal) {
        match s {
            Signal::Owner { name, owner } if name == OBEX => match owner {
                Some(o) => {
                    // Handed straight to another process, with no moment
                    // unowned in between. obexd cannot do that -- it asks for
                    // its name with DO_NOT_QUEUE (gdbus/mainloop.c:325-326) --
                    // but anything else owning the name can, and the new
                    // owner has neither the old one's transfers nor an agent.
                    if self.obex_instance.as_ref().is_some_and(|known| *known != o) {
                        self.obex_gone();
                    }
                    self.obex_instance = Some(o.clone());
                    self.obex_owner.set(Some(o));
                    if !self.obex.registered && !self.obex.pending {
                        self.obex.stuck = false;
                        self.reconcile_obex();
                        self.changed();
                    }
                }
                // Whatever `obex` says: it reads "off" whenever Incoming files
                // is off, sending still works then, and only this ends a send
                // whose obexd has died -- obexd sends no InterfacesRemoved as
                // it goes. Skipped, the send was never answered.
                None => self.obex_gone(),
            },
            Signal::Owner { .. } => {}
            Signal::Changed { path, iface, changed, invalidated } => {
                let applied = changed.clone();
                self.obex_model.changed(&path, &iface, changed, &invalidated);
                if iface != TRANSFER1 {
                    return;
                }
                let now_ms = crate::now_ms();
                let outcome = self
                    .transfers
                    .get_mut(&path)
                    .and_then(|t| t.apply(&applied, now_ms, Instant::now()));
                if let Some(o) = outcome {
                    self.outcome(&path, o);
                }
                if self.transfers.get(&path).is_some() {
                    self.changed();
                }
            }
            Signal::Added { path, ifaces } => self.obex_model.add(&path, ifaces),
            Signal::Removed { path, ifaces } => {
                if ifaces.iter().any(|i| i == TRANSFER1) {
                    if let Some(p) = self.obex_model.props(&path, TRANSFER1) {
                        // Kept a little while: a short transfer can finish and
                        // vanish before SendFile's reply reaches the core.
                        self.graveyard.push_back(Grave {
                            path: path.clone(),
                            obexd: self.obex_instance.clone(),
                            props: p.clone(),
                        });
                        while self.graveyard.len() > 32 {
                            self.graveyard.pop_front();
                        }
                    }
                    let now_ms = crate::now_ms();
                    let o = self.transfers.get_mut(&path).and_then(|t| {
                        t.fail("the transfer ended without completing", now_ms, Instant::now())
                    });
                    if let Some(o) = o {
                        self.outcome(&path, o);
                        self.changed();
                    }
                }
                self.obex_model.remove(&path, &ifaces);
            }
        }
    }

    // ── agents ──

    fn obex_wanted(&self) -> bool {
        self.config.agent && self.config.files != Files::Off
    }

    fn reconcile_agent(&mut self) {
        let want = self.config.agent;
        if !self.bluez {
            self.agent.status = if want { "absent" } else { "off" };
            return;
        }
        if self.agent.pending {
            return;
        }
        if want == self.agent.registered {
            if !want {
                self.agent.status = "off";
                self.agent.error.clear();
            } else if !self.agent.stuck {
                self.agent.status = "ready";
            }
            return;
        }
        if want && self.agent.stuck {
            return;
        }
        let Some(b) = &self.system else { return };
        self.agent.pending = true;
        let tx = self.tx.clone();
        b.run(move |c| {
            let path = ObjectPath::from_static_str_unchecked(bus::AGENT_PATH);
            let result = if want {
                let reg = bus::call(
                    c,
                    BLUEZ,
                    "/org/bluez",
                    "org.bluez.AgentManager1",
                    "RegisterAgent",
                    &(&path, "KeyboardDisplay"),
                );
                // AlreadyExists answers a sender that has an agent already:
                // this connection's own, from before a lost signal.
                match reg {
                    Err(e) if e.name != "org.bluez.Error.AlreadyExists" => Err(e),
                    _ => bus::call(
                        c,
                        BLUEZ,
                        "/org/bluez",
                        "org.bluez.AgentManager1",
                        "RequestDefaultAgent",
                        &(&path,),
                    )
                    .map(|_| None),
                }
            } else {
                bus::call(
                    c,
                    BLUEZ,
                    "/org/bluez",
                    "org.bluez.AgentManager1",
                    "UnregisterAgent",
                    &(&path,),
                )
                .map(|_| None)
            };
            let _ = tx.send(Event::Registered { origin: Origin::Bluez, on: want, result });
        });
    }

    fn reconcile_obex(&mut self) {
        let want = self.obex_wanted();
        let Some(b) = &self.session else {
            self.obex.status = if want { "absent" } else { "off" };
            return;
        };
        if self.obex.pending {
            return;
        }
        if want == self.obex.registered {
            if !want {
                self.obex.status = "off";
                self.obex.error.clear();
            }
            return;
        }
        if want && self.obex.stuck {
            return;
        }
        if !want {
            for p in self.prompts.take_origin(Origin::Obex) {
                p.answer(Answer::Cancel);
            }
        }
        self.obex.pending = true;
        let tx = self.tx.clone();
        b.run(move |c| {
            let path = ObjectPath::from_static_str_unchecked(bus::OBEX_AGENT_PATH);
            let method = if want { "RegisterAgent" } else { "UnregisterAgent" };
            // Registering is also what starts obexd: org.bluez.obex is
            // D-Bus-activated, and nothing else in the session asks for it.
            let result = bus::call(
                c,
                OBEX,
                "/org/bluez/obex",
                "org.bluez.obex.AgentManager1",
                method,
                &(&path,),
            )
            .map(|_| if want { bus::name_owner(c, OBEX) } else { None });
            let _ = tx.send(Event::Registered { origin: Origin::Obex, on: want, result });
        });
    }

    fn registered(&mut self, origin: Origin, on: bool, result: Result<Option<String>, BusError>) {
        match origin {
            Origin::Bluez => {
                self.agent.pending = false;
                match result {
                    Ok(_) => {
                        self.agent.registered = on;
                        self.agent.error.clear();
                        self.agent.status = if on { "ready" } else { "off" };
                    }
                    Err(e) if !on => {
                        // An agent BlueZ no longer has is unregistered either way.
                        self.agent.registered = false;
                        self.agent.status = "off";
                        eprintln!("jayos-bluetooth: UnregisterAgent: {} {}", e.name, e.message);
                    }
                    Err(e) => {
                        self.agent.stuck = true;
                        if e.is_absent() {
                            self.agent.status = "absent";
                        } else {
                            self.agent.status = "error";
                            self.agent.error = format!("{}: {}", e.name, e.message);
                        }
                    }
                }
                self.reconcile_agent();
            }
            Origin::Obex => {
                self.obex.pending = false;
                match result {
                    Ok(owner) => {
                        self.obex.registered = on;
                        self.obex.error.clear();
                        self.obex.status = if on { "ready" } else { "off" };
                        if owner.is_some() {
                            // An obexd running before this daemon started
                            // announced itself to nobody; this is where the
                            // core first learns which process it is.
                            if self.obex_instance.is_none() {
                                self.obex_instance = owner.clone();
                            }
                            self.obex_owner.set(owner);
                        }
                    }
                    Err(e) if !on => {
                        self.obex.registered = false;
                        self.obex.status = "off";
                        eprintln!(
                            "jayos-bluetooth: obex UnregisterAgent: {} {}",
                            e.name, e.message
                        );
                    }
                    Err(e) => {
                        self.obex.stuck = true;
                        if e.is_absent() {
                            self.obex.status = "absent";
                            self.obex.error.clear();
                            self.obex_retry = Some(Instant::now() + OBEX_RETRY);
                        } else if e.name == "org.bluez.obex.Error.AlreadyExists" {
                            // obexd takes one agent for the whole session.
                            self.obex.status = "busy";
                            self.obex.error = e.message;
                        } else {
                            self.obex.status = "error";
                            self.obex.error = format!("{}: {}", e.name, e.message);
                        }
                    }
                }
                self.reconcile_obex();
            }
        }
        self.changed();
    }

    fn subject(&self, device: &str) -> Subject {
        let address = self
            .model
            .str(device, DEVICE, "Address")
            .map(str::to_string)
            .unwrap_or_else(|| address_from_path(device));
        let name = self.model.str(device, DEVICE, "Alias").unwrap_or("").to_string();
        Subject { device: device.to_string(), address, name }
    }

    fn agent_event(&mut self, a: AgentEvent) {
        let now = Instant::now();
        let now_ms = crate::now_ms();
        match a {
            AgentEvent::Request { kind, device, details, responder } => {
                if !self.config.agent {
                    let _ = responder.try_send(Answer::Reject);
                    return;
                }
                let subject = self.subject(&device);
                self.prompts.open(kind, subject, details, responder, now, now_ms);
                self.state_now();
            }
            AgentEvent::Display { kind, device, passkey, entered } => {
                let subject = self.subject(&device);
                let (_, new) = self.prompts.display(kind, subject, passkey, entered, now, now_ms);
                if new {
                    self.state_now();
                } else {
                    self.changed();
                }
            }
            AgentEvent::Push { transfer, name, size, destination, source, responder } => {
                self.push(transfer, name, size, destination, source, responder);
            }
            AgentEvent::Cancel(origin) => {
                for p in self.prompts.take_origin(origin) {
                    p.answer(Answer::Cancel);
                }
                self.state_now();
            }
            AgentEvent::Release(origin) => {
                for p in self.prompts.take_origin(origin) {
                    p.answer(Answer::Cancel);
                }
                let slot = if origin == Origin::Bluez { &mut self.agent } else { &mut self.obex };
                slot.registered = false;
                slot.stuck = true;
                slot.status = "off";
                self.state_now();
            }
        }
    }

    // ── receiving ──

    /// Where an accepted push is saved, made unique against the folder and
    /// against pushes still arriving.
    fn push_target(&self, file: &str) -> Result<String, String> {
        let dir = &self.receive_dir;
        files::ensure_dir(dir).map_err(|e| format!("cannot use {}: {e}", dir.display()))?;
        let reserved = self.transfers.reserved_paths();
        // symlink_metadata, not exists(): a dangling symlink at the name is
        // taken too, or obexd would write through it to wherever it points.
        let path = files::unique_path(dir, file, |p| {
            p.symlink_metadata().is_ok() || reserved.iter().any(|r| Path::new(r) == p)
        });
        path.to_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{} is not a name D-Bus can carry", path.display()))
    }

    fn push(
        &mut self,
        transfer: String,
        name: Option<String>,
        size: Option<u64>,
        destination: String,
        source: String,
        responder: Responder,
    ) {
        if !self.obex_wanted() {
            let _ = responder.try_send(Answer::Reject);
            return;
        }
        let device = self
            .model
            .device_by_address(&destination, (!source.is_empty()).then_some(source.as_str()));
        let subject = match &device {
            Some(d) => self.subject(d),
            None => {
                Subject { device: String::new(), address: destination.clone(), name: String::new() }
            }
        };
        let trusted =
            device.as_deref().and_then(|d| self.model.bool(d, DEVICE, "Trusted")) == Some(true);
        let file = files::sanitise(name.as_deref().unwrap_or(""));
        if self.config.files == Files::Trusted && trusted {
            match self.push_target(&file) {
                Ok(path) => {
                    self.receive(&transfer, &subject, &file, &path, size);
                    let _ = responder.try_send(Answer::Accept(path));
                    self.changed();
                }
                Err(message) => {
                    let _ = responder.try_send(Answer::Cancel);
                    let extra = json!({"direction": "in", "message": message});
                    self.notice("failed", &file, &subject.device, &subject.name, extra);
                }
            }
            return;
        }
        let details = Details { file, size, transfer, ..Details::default() };
        self.prompts.open(Kind::Push, subject, details, responder, Instant::now(), crate::now_ms());
        self.state_now();
    }

    fn receive(
        &mut self,
        transfer: &str,
        subject: &Subject,
        file: &str,
        path: &str,
        size: Option<u64>,
    ) {
        let start = Start {
            id: transfer.to_string(),
            device: subject.device.clone(),
            address: subject.address.clone(),
            device_name: subject.name.clone(),
            file: file.to_string(),
            path: path.to_string(),
            size,
        };
        // Inserted before obexd is answered, so its first progress signal
        // finds the transfer already listed.
        self.transfers.insert(Transfer::new(Direction::In, start, None, crate::now_ms()));
    }

    // ── transfers ending ──

    fn outcome(&mut self, id: &str, o: Outcome) {
        let Some(t) = self.transfers.get(id) else { return };
        let (dir, file, path, device, name, batch) = (
            t.direction,
            t.file.clone(),
            t.path.clone(),
            t.device.clone(),
            t.device_name.clone(),
            t.batch,
        );
        match (&o, dir) {
            (Outcome::Complete, Direction::In) => {
                self.notice("received", &file, &device, &name, json!({"path": path}))
            }
            (Outcome::Complete, Direction::Out) => {
                self.notice("sent", &file, &device, &name, Value::Null)
            }
            (Outcome::Failed(message), d) => {
                let extra = json!({"direction": d.as_str(), "message": message});
                self.notice("failed", &file, &device, &name, extra)
            }
            // Cancelled by this daemon on somebody's request: not news to them.
            (Outcome::Cancelled, _) => {}
        }
        if let Some(b) = batch.and_then(|b| self.batches.get_mut(&b)) {
            match o {
                Outcome::Complete => b.sent += 1,
                Outcome::Failed(m) => {
                    b.first_error.get_or_insert(BusError::new("org.bluez.obex.Error.Failed", m));
                }
                Outcome::Cancelled => {
                    b.cancelled.store(true, Ordering::SeqCst);
                }
            }
            let _ = b.tx.send(());
        }
    }

    // ── commands ──

    fn command(&mut self, client: u64, line: &str) {
        let cmd = match protocol::parse_command(line) {
            Ok(c) => c,
            Err(r) => {
                self.reply(client, r.line());
                return;
            }
        };
        let Command { id, name, op } = cmd;
        match op {
            Op::Config { agent, files, receive_dir } => {
                let agent_was = self.config.agent;
                if let Some(a) = agent {
                    self.config.agent = a;
                }
                if let Some(f) = files {
                    self.config.files = f;
                }
                if let Some(d) = receive_dir {
                    self.config.receive_dir = d;
                }
                self.receive_dir =
                    files::receive_dir(&self.config.receive_dir, &DirEnv::from_process());
                if self.config.agent != agent_was {
                    self.agent.stuck = false;
                    self.obex.stuck = false;
                    if !self.config.agent {
                        for p in self.prompts.take_origin(Origin::Bluez) {
                            p.answer(Answer::Cancel);
                        }
                    }
                }
                self.reconcile_agent();
                self.reconcile_obex();
                self.reply_ok(client, &id, name);
                self.state_now();
            }
            Op::Snapshot => {
                let line = self.state_line();
                self.send_to(client, &line);
                self.reply_ok(client, &id, name);
            }
            Op::Power { adapter, on } => {
                self.adapter_set(client, id, name, adapter, "Powered", ZValue::from(on))
            }
            Op::AdapterName { adapter, name: alias } => {
                self.adapter_set(client, id, name, adapter, "Alias", ZValue::from(alias))
            }
            Op::Discoverable { adapter, on } => {
                self.adapter_set(client, id, name, adapter, "Discoverable", ZValue::from(on))
            }
            Op::DiscoverableTimeout { adapter, seconds } => self.adapter_set(
                client,
                id,
                name,
                adapter,
                "DiscoverableTimeout",
                ZValue::from(seconds),
            ),
            Op::Pairable { adapter, on } => {
                self.adapter_set(client, id, name, adapter, "Pairable", ZValue::from(on))
            }
            Op::Scan { adapter, on } => self.scan(client, id, adapter, on),
            Op::Connect { device }
            | Op::Pair { device }
            | Op::Disconnect { device }
            | Op::Remove { device }
            | Op::CancelPair { device } => self.device_call(client, id, name, device),
            Op::Trust { device, on } => {
                self.device_set(client, id, name, device, "Trusted", ZValue::from(on))
            }
            Op::Block { device, on } => {
                self.device_set(client, id, name, device, "Blocked", ZValue::from(on))
            }
            Op::Wake { device, on } => {
                self.device_set(client, id, name, device, "WakeAllowed", ZValue::from(on))
            }
            Op::Rename { device, name: alias } => {
                self.device_set(client, id, name, device, "Alias", ZValue::from(alias))
            }
            Op::Send { device, files } => self.send(client, id, name, device, files),
            Op::Pick { device } => self.pick(client, id, device),
            Op::Cancel { transfer } => self.cancel(client, id, transfer),
            Op::Dismiss { transfer } => match self.transfers.dismiss(&transfer) {
                Ok(()) => {
                    self.reply_ok(client, &id, name);
                    self.changed();
                }
                Err((e, m)) => self.reply_err(client, &id, name, e, m),
            },
            Op::Answer { prompt, accept, value } => self.answer(client, id, prompt, accept, value),
        }
    }

    /// Checks shared by every BlueZ op: BlueZ is there and so is the object.
    fn bluez_problem(&self, path: &str, iface: &str) -> Option<(&'static str, &'static str)> {
        if !self.bluez || self.system.is_none() {
            return Some(("bluez-absent", "bluetoothd is not running"));
        }
        if !self.model.has(path, iface) {
            return Some(if iface == ADAPTER {
                ("no-such-adapter", "no adapter has that path")
            } else {
                ("no-such-device", "no device has that path")
            });
        }
        None
    }

    fn bluez_target(&mut self, client: u64, id: &str, op: &str, path: &str, iface: &str) -> bool {
        match self.bluez_problem(path, iface) {
            Some((e, m)) => {
                self.reply_err(client, id, op, e, m);
                false
            }
            None => true,
        }
    }

    fn adapter_set(
        &mut self,
        client: u64,
        id: String,
        op: &'static str,
        adapter: String,
        prop: &'static str,
        value: ZValue<'static>,
    ) {
        if !self.bluez_target(client, &id, op, &adapter, ADAPTER) {
            return;
        }
        let tx = self.tx.clone();
        if let Some(b) = &self.system {
            b.run(move |c| {
                let line = match bus::set_prop(c, BLUEZ, &adapter, ADAPTER, prop, value) {
                    Ok(()) => protocol::reply_ok(&id, op, Map::new()),
                    Err(e) => protocol::reply_err(&id, op, &e.name, &e.message),
                };
                let _ = tx.send(Event::Reply { client, line, busy: None });
            });
        }
    }

    fn busy_begin(&mut self, device: &str, op: &'static str) -> Option<(String, u64)> {
        if !BUSY_OPS.contains(&op) {
            return None;
        }
        self.busy_seq += 1;
        self.busy.entry(device.to_string()).or_default().push((self.busy_seq, op));
        self.changed();
        Some((device.to_string(), self.busy_seq))
    }

    fn busy_end(&mut self, device: &str, seq: u64) {
        if let Some(v) = self.busy.get_mut(device) {
            v.retain(|(s, _)| *s != seq);
            if v.is_empty() {
                self.busy.remove(device);
            }
        }
    }

    fn device_set(
        &mut self,
        client: u64,
        id: String,
        op: &'static str,
        device: String,
        prop: &'static str,
        value: ZValue<'static>,
    ) {
        if !self.bluez_target(client, &id, op, &device, DEVICE) {
            return;
        }
        let busy = self.busy_begin(&device, op);
        let tx = self.tx.clone();
        if let Some(b) = &self.system {
            b.run(move |c| {
                let line = match bus::set_prop(c, BLUEZ, &device, DEVICE, prop, value) {
                    Ok(()) => protocol::reply_ok(&id, op, Map::new()),
                    Err(e) => protocol::reply_err(&id, op, &e.name, &e.message),
                };
                let _ = tx.send(Event::Reply { client, line, busy });
            });
        }
    }

    /// Connect, disconnect, pair, cancel-pair and remove: device methods that
    /// can take seconds, each on a thread of its own.
    fn device_call(&mut self, client: u64, id: String, op: &'static str, device: String) {
        if !self.bluez_target(client, &id, op, &device, DEVICE) {
            return;
        }
        let paired = self.model.bool(&device, DEVICE, "Paired") == Some(true);
        let adapter = self.model.device_adapter(&device);
        let busy = self.busy_begin(&device, op);
        let tx = self.tx.clone();
        let Some(b) = &self.system else { return };
        b.spawn(move |c| {
            let line = match op {
                // An unpaired device cannot be connected to; pairing first is
                // what the person meant.
                "pair" => pair_then_connect(c, &id, op, &device),
                "connect" if !paired => pair_then_connect(c, &id, op, &device),
                _ => {
                    let r = match op {
                        "connect" => bus::call(c, BLUEZ, &device, DEVICE, "Connect", &()),
                        "disconnect" => bus::call(c, BLUEZ, &device, DEVICE, "Disconnect", &()),
                        "cancel-pair" => bus::call(c, BLUEZ, &device, DEVICE, "CancelPairing", &()),
                        _ => object_path(&device).and_then(|p| {
                            bus::call(c, BLUEZ, &adapter, ADAPTER, "RemoveDevice", &(p,))
                        }),
                    };
                    match r {
                        Ok(_) => protocol::reply_ok(&id, op, Map::new()),
                        Err(e) => protocol::reply_err(&id, op, &e.name, &e.message),
                    }
                }
            };
            let _ = tx.send(Event::Reply { client, line, busy });
        });
    }

    fn start_discovery(&mut self, adapter: String, reply: Option<(u64, String)>) {
        // Held back while audio streams over this adapter, and started by
        // audio_yield when the stream ends. The client is told it has its
        // scan: the interest is kept, and the state says why nothing is
        // being found (`audioPaused`).
        if self.model.streaming_on(&adapter) {
            self.yielded.insert(adapter);
            if let Some((client, id)) = reply {
                self.reply_ok(client, &id, "scan");
            }
            return;
        }
        self.yielded.remove(&adapter);
        let tx = self.tx.clone();
        let Some(b) = &self.system else { return };
        b.run(move |c| {
            let r = bus::call(c, BLUEZ, &adapter, ADAPTER, "StartDiscovery", &());
            if let Some((client, id)) = reply {
                let line = match r {
                    // Already running for this connection: what was asked for.
                    Err(e) if e.name != "org.bluez.Error.InProgress" => {
                        protocol::reply_err(&id, "scan", &e.name, &e.message)
                    }
                    _ => protocol::reply_ok(&id, "scan", Map::new()),
                };
                let _ = tx.send(Event::Reply { client, line, busy: None });
            }
        });
    }

    fn stop_discovery(&mut self, adapter: String) {
        if let Some(b) = &self.system {
            b.run(move |c| {
                // An error here is a discovery that had already stopped -- the
                // adapter went off -- which is the state being asked for.
                let _ = bus::call(c, BLUEZ, &adapter, ADAPTER, "StopDiscovery", &());
            });
        }
    }

    /// DISCOVERY YIELDS TO AUDIO. A scan is an inquiry on BR/EDR and an LE
    /// scan beside it, and BlueZ runs both at full duty for as long as it is
    /// discovering (src/adapter.c's discovery, and the kernel's interleaved
    /// discovery in net/bluetooth/hci_sync.c). The radio is one radio: every
    /// inquiry window is time an A2DP link is not being served, and a
    /// headset's buffer, a few hundred milliseconds of LDAC at most, runs dry
    /// -- heard as audio that lags and cuts out, the owner's report of
    /// 2026-11-12 over a WH-1000XM5. The page scans for as long as it is open
    /// and the network popup's Bluetooth tab for as long as it is shown, so
    /// the case is common: opening either to connect the headset, then
    /// listening with it open.
    ///
    /// So while a transport under an adapter is acquired -- audio streaming,
    /// or about to -- this daemon stops its discovery there, keeps every
    /// client's interest, and starts discovery again when the last stream
    /// ends. Android does the like for A2DP (read, not run). Only this
    /// daemon's own discovery: BlueZ keeps one session per D-Bus connection,
    /// and another program's scan is that program's.
    ///
    /// Run over every adapter with interest whenever a transport comes, goes
    /// or changes State, which is cheap -- a handful of paths -- and keeps
    /// one rule for every way a stream can start or end.
    fn audio_yield(&mut self) {
        let mut moved = false;
        for adapter in self.interest.adapters() {
            let streaming = self.model.streaming_on(&adapter);
            let powered = self.model.bool(&adapter, ADAPTER, "Powered") == Some(true);
            if streaming && !self.yielded.contains(&adapter) {
                self.yielded.insert(adapter.clone());
                if powered {
                    self.stop_discovery(adapter);
                }
                moved = true;
            } else if !streaming && self.yielded.contains(&adapter) {
                self.yielded.remove(&adapter);
                if powered {
                    self.start_discovery(adapter, None);
                }
                moved = true;
            }
        }
        if moved {
            self.changed();
        }
    }

    fn scan(&mut self, client: u64, id: String, adapter: String, on: bool) {
        // Giving interest back needs no BlueZ. Refused while bluetoothd or
        // the adapter was away, an `off` left the interest in place, and
        // discovery started again for it when they came back -- for a client
        // that had stopped looking, the network popup on its Wi-Fi tab.
        if !on {
            if let Change::Stop(a) = self.interest.set(client, &adapter, false) {
                // A discovery held back for audio was never started, so
                // there is nothing to stop.
                if !self.yielded.remove(&a) && self.bluez_problem(&a, ADAPTER).is_none() {
                    self.stop_discovery(a);
                }
            }
            self.reply_ok(client, &id, "scan");
            self.changed();
            return;
        }
        if !self.bluez_target(client, &id, "scan", &adapter, ADAPTER) {
            return;
        }
        match self.interest.set(client, &adapter, on) {
            Change::Start(a) => {
                if self.model.bool(&a, ADAPTER, "Powered") == Some(true) {
                    self.start_discovery(a, Some((client, id)));
                } else {
                    // Kept, and started when the adapter powers on.
                    self.reply_ok(client, &id, "scan");
                }
            }
            Change::Stop(a) => {
                if !self.yielded.remove(&a) {
                    self.stop_discovery(a);
                }
                self.reply_ok(client, &id, "scan");
            }
            Change::Nothing => self.reply_ok(client, &id, "scan"),
        }
        self.changed();
    }

    fn answer(
        &mut self,
        client: u64,
        id: String,
        prompt: u64,
        accept: bool,
        value: Option<String>,
    ) {
        let Some(p) = self.prompts.get(prompt) else {
            self.reply_err(client, &id, "answer", "no-such-prompt", "no prompt has that id");
            return;
        };
        let kind = p.kind;
        if kind.is_display() {
            // Nothing is waiting on a display prompt; answering it hides it.
            self.prompts.take(prompt);
        } else if kind == Kind::Push {
            let Some(p) = self.prompts.take(prompt) else { return };
            if !accept {
                p.answer(Answer::Reject);
            } else {
                match self.push_target(&p.details.file) {
                    Ok(path) => {
                        let (transfer, subject, file, size) = (
                            p.details.transfer.clone(),
                            p.subject.clone(),
                            p.details.file.clone(),
                            p.details.size,
                        );
                        self.receive(&transfer, &subject, &file, &path, size);
                        p.answer(Answer::Accept(path));
                    }
                    Err(message) => {
                        let (file, device, name) = (
                            p.details.file.clone(),
                            p.subject.device.clone(),
                            p.subject.name.clone(),
                        );
                        p.answer(Answer::Cancel);
                        // A notice as well as the reply, as the trusted path
                        // sends: the reply only reaches the card, and the
                        // card goes with the prompt in the very next state.
                        let extra = json!({"direction": "in", "message": message});
                        self.notice("failed", &file, &device, &name, extra);
                        self.reply_err(client, &id, "answer", "receive-dir", &message);
                        self.state_now();
                        return;
                    }
                }
            }
        } else {
            match prompts::answer_for(kind, accept, value.as_deref()) {
                Ok(a) => {
                    if let Some(p) = self.prompts.take(prompt) {
                        p.answer(a);
                    }
                }
                Err(message) => {
                    self.reply_err(client, &id, "answer", "bad-request", &message);
                    return;
                }
            }
        }
        self.reply_ok(client, &id, "answer");
        self.state_now();
    }

    // ── sending ──

    /// A `send` or `pick` that ended before any file was offered to the
    /// device: the reply, and a `failed` notice to every client. The notice
    /// is not a duplicate. The settings page closes the panel the moment it
    /// asks for a pick -- the chooser would open underneath it otherwise --
    /// so the client the reply goes to has usually gone, and a notice is what
    /// the shell turns into a notification.
    #[allow(clippy::too_many_arguments)]
    fn send_refused(
        &mut self,
        client: u64,
        id: &str,
        op: &str,
        device: &str,
        file: &str,
        error: &str,
        message: &str,
    ) {
        self.reply_err(client, id, op, error, message);
        let name = self.model.str(device, DEVICE, "Alias").unwrap_or("").to_string();
        let extra = json!({"direction": "out", "message": message});
        self.notice("failed", file, device, &name, extra);
    }

    fn send(
        &mut self,
        client: u64,
        id: String,
        op: &'static str,
        device: String,
        files: Vec<String>,
    ) {
        if let Some((e, m)) = self.bluez_problem(&device, DEVICE) {
            self.send_refused(client, &id, op, &device, &only_file(&files), e, m);
            return;
        }
        if self.session.is_none() {
            let message = "there is no session bus to reach obexd on";
            self.send_refused(client, &id, op, &device, &only_file(&files), "obex-absent", message);
            return;
        }
        // Every file is checked before CreateSession: a typo in the third
        // name should not cost a connection to the phone and two transfers.
        for f in &files {
            let p = Path::new(f);
            // is_file before open: opening a fifo would block this thread
            // until something wrote to it.
            let readable = p.is_absolute() && p.is_file() && std::fs::File::open(p).is_ok();
            if !readable {
                let message = format!("cannot read {f}");
                self.send_refused(
                    client,
                    &id,
                    op,
                    &device,
                    &base_name(f),
                    "file-unreadable",
                    &message,
                );
                return;
            }
        }
        let address = self.model.str(&device, DEVICE, "Address").unwrap_or("").to_string();
        let name = self.model.str(&device, DEVICE, "Alias").unwrap_or("").to_string();
        let source = self.model.send_source(&device);
        self.next_batch += 1;
        let batch = self.next_batch;
        let (btx, brx) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.batches.insert(
            batch,
            Batch {
                client,
                id,
                op,
                device,
                address: address.clone(),
                name,
                files: files.clone(),
                transfers: Vec::new(),
                sent: 0,
                tried: 0,
                first_error: None,
                cancelled: cancelled.clone(),
                tx: btx,
            },
        );
        let tx = self.tx.clone();
        if let Some(b) = &self.session {
            b.spawn(move |c| run_batch(c, batch, &address, source, &files, &brx, &cancelled, &tx));
        }
    }

    fn started(
        &mut self,
        batch: u64,
        index: usize,
        transfer: String,
        props: Props,
        obexd: Option<String>,
    ) {
        let Some(b) = self.batches.get_mut(&batch) else { return };
        b.tried += 1;
        b.transfers.push(transfer.clone());
        let path = b.files.get(index).cloned().unwrap_or_default();
        let start = Start {
            id: transfer.clone(),
            device: b.device.clone(),
            address: b.address.clone(),
            device_name: b.name.clone(),
            file: base_name(&path),
            path,
            size: None,
        };
        let mut t = Transfer::new(Direction::Out, start, Some(batch), crate::now_ms());
        let now = Instant::now();
        let now_ms = crate::now_ms();
        let mut outcome = t.apply(&props, now_ms, now);
        // The obexd that answered may have left since: its NameOwnerChanged
        // and SendFile's reply arrive on different threads, and obex_gone has
        // then already failed every transfer it knew -- which this one was not
        // yet. Nothing would end it after, so it ends here: from what that
        // obexd said before it went, or as stopped.
        let gone =
            obexd.as_ref().is_some_and(|o| self.obex_instance.as_deref() != Some(o.as_str()));
        let instance = obexd.or_else(|| self.obex_instance.clone());
        // Whatever obexd said about this transfer before SendFile's reply got
        // here: the subscription was made first, so it is in the model -- or,
        // for a transfer short enough to be gone already, in the graveyard,
        // under the obexd it came from.
        let current =
            if gone { None } else { self.obex_model.props(&transfer, TRANSFER1).cloned() };
        let earlier = current.or_else(|| {
            self.graveyard
                .iter()
                .rev()
                .find(|g| g.path == transfer && g.obexd == instance)
                .map(|g| g.props.clone())
        });
        if let Some(p) = earlier {
            outcome = outcome.or_else(|| t.apply(&p, now_ms, now));
        }
        if gone {
            outcome = outcome.or_else(|| t.fail("obexd stopped", now_ms, now));
        }
        self.transfers.insert(t);
        if let Some(o) = outcome {
            self.outcome(&transfer, o);
        }
        self.changed();
    }

    fn file_failed(&mut self, batch: u64, index: usize, error: BusError) {
        let Some(b) = self.batches.get_mut(&batch) else { return };
        b.tried += 1;
        let file = b.files.get(index).map(|f| base_name(f)).unwrap_or_default();
        let (device, name) = (b.device.clone(), b.name.clone());
        // A batch has said what went wrong once already when the transfer
        // that lost the link failed: a second notice for its SendFile is
        // noise, and the reply counts every file that did not go.
        let first = b.first_error.is_none();
        b.first_error.get_or_insert(error.clone());
        if first {
            let extra = json!({"direction": "out", "message": error.message});
            self.notice("failed", &file, &device, &name, extra);
        }
    }

    fn batch_done(&mut self, batch: u64, error: Option<BusError>) {
        let Some(b) = self.batches.remove(&batch) else { return };
        let total = b.files.len();
        let mut extra = Map::new();
        extra.insert("transfers".into(), json!(b.transfers));
        extra.insert("sent".into(), json!(b.sent));
        extra.insert("failed".into(), json!(total - b.sent));
        let line = if let Some(e) = error {
            // CreateSession to a service that is not there: obexd is missing.
            let name = if e.is_absent() { "obex-absent" } else { e.name.as_str() };
            // No transfer began, so nothing else says so (see send_refused).
            let why = json!({"direction": "out", "message": e.message});
            self.notice("failed", &only_file(&b.files), &b.device, &b.name, why);
            protocol::reply_err_with(&b.id, b.op, name, &e.message, extra)
        } else if b.cancelled.load(Ordering::SeqCst) {
            protocol::reply_err_with(
                &b.id,
                b.op,
                "org.bluez.obex.Error.Canceled",
                "the transfer was cancelled",
                extra,
            )
        } else if b.sent < total {
            // The files a stopped batch never offered. Its one notice so far
            // names the file that failed, and the reply that counts the rest
            // goes to a client that has usually gone -- the page closes the
            // settings panel when it asks for a pick -- so without this the
            // person learns of one file and not the others.
            let untried = total.saturating_sub(b.tried);
            if untried > 0 {
                let what = if untried == 1 {
                    "1 more file".to_string()
                } else {
                    format!("{untried} more files")
                };
                let why =
                    json!({"direction": "out", "message": "not tried once the send had stopped"});
                self.notice("failed", &what, &b.device, &b.name, why);
            }
            let first =
                b.first_error.unwrap_or_else(|| BusError::new("org.bluez.obex.Error.Failed", ""));
            let mut message = format!("{} of {total} files were not sent", total - b.sent);
            if !first.message.is_empty() {
                message = format!("{message}: {}", first.message);
            }
            protocol::reply_err_with(&b.id, b.op, &first.name, &message, extra)
        } else {
            protocol::reply_ok(&b.id, b.op, extra)
        };
        self.reply(b.client, line);
    }

    fn pick(&mut self, client: u64, id: String, device: String) {
        if let Some((e, m)) = self.bluez_problem(&device, DEVICE) {
            self.send_refused(client, &id, "pick", &device, "", e, m);
            return;
        }
        let name = self.model.str(&device, DEVICE, "Alias").unwrap_or("").to_string();
        let Some(b) = &self.session else {
            let m = "there is no session bus";
            self.send_refused(client, &id, "pick", &device, "", "portal-unavailable", m);
            return;
        };
        static TOKENS: AtomicU64 = AtomicU64::new(0);
        let token = format!(
            "qs_bluetooth_{}_{}",
            std::process::id(),
            TOKENS.fetch_add(1, Ordering::Relaxed)
        );
        let tx = self.tx.clone();
        b.spawn(move |c| {
            let result = portal::pick(c, &format!("Send to {name}"), &token);
            let _ = tx.send(Event::Picked { client, id, device, result });
        });
    }

    fn picked(
        &mut self,
        client: u64,
        id: String,
        device: String,
        result: Result<Picked, BusError>,
    ) {
        match result {
            Ok(Picked::Cancelled) => {
                let mut extra = Map::new();
                extra.insert("cancelled".into(), json!(true));
                self.reply(client, protocol::reply_ok(&id, "pick", extra));
            }
            Ok(Picked::Files(paths)) if paths.is_empty() => {
                let mut extra = Map::new();
                extra.insert("cancelled".into(), json!(true));
                self.reply(client, protocol::reply_ok(&id, "pick", extra));
            }
            Ok(Picked::Files(paths)) => {
                let mut files = Vec::new();
                for p in paths {
                    match p.to_str() {
                        Some(s) => files.push(s.to_string()),
                        None => {
                            let m = format!("{} is not a name obexd can be given", p.display());
                            let file = p.file_name().map(|f| f.to_string_lossy().into_owned());
                            let file = file.unwrap_or_default();
                            self.send_refused(
                                client,
                                &id,
                                "pick",
                                &device,
                                &file,
                                "file-unreadable",
                                &m,
                            );
                            return;
                        }
                    }
                }
                self.send(client, id, "pick", device, files);
            }
            // The person closing the chooser is Picked::Cancelled above, and
            // says nothing; this is the chooser failing, or never answering.
            Err(e) if portal::is_unavailable(&e) => self.send_refused(
                client,
                &id,
                "pick",
                &device,
                "",
                "portal-unavailable",
                &e.message,
            ),
            Err(e) => self.send_refused(client, &id, "pick", &device, "", &e.name, &e.message),
        }
    }

    fn cancel(&mut self, client: u64, id: String, transfer: String) {
        let Some(t) = self.transfers.get(&transfer) else {
            self.reply_err(client, &id, "cancel", "no-such-transfer", "no transfer has that id");
            return;
        };
        if t.is_finished() {
            self.reply_err(
                client,
                &id,
                "cancel",
                "bad-request",
                "the transfer has already finished",
            );
            return;
        }
        let Some(b) = &self.session else {
            self.reply_err(client, &id, "cancel", "obex-absent", "there is no session bus");
            return;
        };
        let tx = self.tx.clone();
        let target = transfer.clone();
        b.run(move |c| {
            let result = bus::call(c, OBEX, &target, TRANSFER1, "Cancel", &()).map(|_| ());
            let _ = tx.send(Event::Cancelled { client, id, transfer: target, result });
        });
        if let Some(t) = self.transfers.get_mut(&transfer) {
            t.cancelling = true;
        }
    }

    fn cancelled(&mut self, client: u64, id: &str, transfer: &str, result: Result<(), BusError>) {
        match result {
            Ok(()) => {
                let o = self
                    .transfers
                    .get_mut(transfer)
                    .and_then(|t| t.cancel(crate::now_ms(), Instant::now()));
                if let Some(o) = o {
                    self.outcome(transfer, o);
                }
                self.reply_ok(client, id, "cancel");
                self.changed();
            }
            Err(e) => {
                if let Some(t) = self.transfers.get_mut(transfer) {
                    t.cancelling = false;
                }
                self.reply_err(client, id, "cancel", &e.name, &e.message)
            }
        }
    }
}

/// Pair, trust, connect. The reply is ok iff Pair succeeded; whether the
/// connection that follows worked is reported beside it.
fn pair_then_connect(c: &Connection, id: &str, op: &str, device: &str) -> String {
    if let Err(e) = bus::call(c, BLUEZ, device, DEVICE, "Pair", &()) {
        return protocol::reply_err(id, op, &e.name, &e.message);
    }
    // Trusted so the device can reconnect without asking. Overskride's README
    // lists auto-trust on pair as not working; the network popup trusts a
    // paired device it connects for the same reason (NetworkPopup.qml,
    // btConnect).
    if let Err(e) = bus::set_prop(c, BLUEZ, device, DEVICE, "Trusted", ZValue::from(true)) {
        eprintln!("jayos-bluetooth: trusting {device}: {} {}", e.name, e.message);
    }
    let mut extra = Map::new();
    match bus::call(c, BLUEZ, device, DEVICE, "Connect", &()) {
        Ok(_) => {
            extra.insert("connected".into(), json!(true));
        }
        Err(e) => {
            extra.insert("connected".into(), json!(false));
            extra.insert("connectError".into(), json!(e.name));
            extra.insert("connectMessage".into(), json!(e.message));
        }
    }
    protocol::reply_ok(id, op, extra)
}

/// One `send`: a session, the files in order, and the session removed at the
/// end whatever happened. Overskride never removes it: RemoveSession is
/// defined in its src/obex/obex_utils.rs:36-47 and called nowhere.
#[allow(clippy::too_many_arguments)]
fn run_batch(
    c: &Connection,
    batch: u64,
    address: &str,
    source: Option<String>,
    files: &[String],
    finished: &Receiver<()>,
    cancelled: &AtomicBool,
    tx: &Sender<Event>,
) {
    let mut args: HashMap<&str, ZValue> = HashMap::new();
    args.insert("Target", ZValue::from("opp"));
    if let Some(s) = source {
        args.insert("Source", ZValue::from(s));
    }
    let session: OwnedObjectPath = match bus::call(
        c,
        OBEX,
        "/org/bluez/obex",
        "org.bluez.obex.Client1",
        "CreateSession",
        &(address, args),
    )
    .and_then(|m| m.body().deserialize().map_err(BusError::from))
    {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(Event::BatchDone { batch, error: Some(e) });
            return;
        }
    };
    for (index, file) in files.iter().enumerate() {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        let reply = bus::call(
            c,
            OBEX,
            session.as_str(),
            "org.bluez.obex.ObjectPush1",
            "SendFile",
            &(file.as_str(),),
        )
        .and_then(|m| {
            let obexd = m.header().sender().map(|s| s.as_str().to_string());
            m.body()
                .deserialize::<(OwnedObjectPath, HashMap<String, OwnedValue>)>()
                .map(|(transfer, props)| (transfer, props, obexd))
                .map_err(BusError::from)
        });
        match reply {
            Ok((transfer, props, obexd)) => {
                let _ = tx.send(Event::Started {
                    batch,
                    index,
                    transfer: transfer.as_str().to_string(),
                    props: props_from(&props),
                    obexd,
                });
                // The core says when this transfer has ended; a closed channel
                // means the daemon is going and there is nobody to report to.
                if finished.recv().is_err() {
                    return;
                }
            }
            // Stop at the first: every file is checked before
            // CreateSession, so a SendFile that fails is the session failing.
            // When the link drops obexd answers "Session not connected"
            // (client/session.c:860-864) and then unregisters the session
            // (session_disconnected), and each file after would fail the
            // same way, a notification apiece. batch_done counts them unsent.
            Err(error) => {
                let _ = tx.send(Event::FileFailed { batch, index, error });
                break;
            }
        }
    }
    let _ = bus::call(
        c,
        OBEX,
        "/org/bluez/obex",
        "org.bluez.obex.Client1",
        "RemoveSession",
        &(&session,),
    );
    let _ = tx.send(Event::BatchDone { batch, error: None });
}
