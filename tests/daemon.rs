//! The daemon end to end, against a mock BlueZ and a mock obexd on private
//! buses.
//!
//! There is no Bluetooth adapter and no system bus where this runs, so two
//! dbus-daemons are started from a permissive configuration, one standing in
//! for the system bus and one for the session bus, and the real binary is
//! pointed at them through the standard environment variables. The mocks
//! implement just enough of BlueZ's and obexd's interfaces to be the other
//! side of every call the daemon makes, and to make the calls BlueZ and obexd
//! make back -- the agent methods.
//!
//! Without dbus-daemon the bus-backed tests print `skip: dbus-daemon not
//! found` to stderr and return; the three that need no bus still run.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use jayos_bluetooth::control::LOCK_GRACE;
use serde_json::{json, Value};
use zbus::blocking;
use zbus::message::Header;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value as ZValue};
use zbus::{fdo, interface, Connection, ObjectServer};

const WAIT: Duration = Duration::from_secs(10);
const ADAPTER: &str = "/org/bluez/hci0";
const PHONE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01";
const HEADSET: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_02";
const BARE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_03";
const AGENT: &str = "/org/jayos/bluetooth/agent";

// ── the scratch directory and the buses ─────────────────────────────────────

static SCRATCH: AtomicU32 = AtomicU32::new(0);

/// A directory under /tmp with a short name: a unix socket path is limited to
/// 108 bytes, and the scratchpad's own paths are longer than that.
fn scratch() -> PathBuf {
    let dir = PathBuf::from(format!(
        "/tmp/qsbt.{}.{}",
        std::process::id(),
        SCRATCH.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn dbus_daemon() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("dbus-daemon")).find(|p| p.is_file())
}

struct Buses {
    dir: PathBuf,
    system: String,
    session: String,
    children: Vec<Child>,
}

impl Buses {
    fn start() -> Option<Buses> {
        let Some(exe) = dbus_daemon() else {
            eprintln!("skip: dbus-daemon not found");
            return None;
        };
        let dir = scratch();
        let mut children = Vec::new();
        let mut addrs = Vec::new();
        for name in ["system", "session"] {
            let conf = dir.join(format!("{name}.conf"));
            std::fs::write(
                &conf,
                format!(
                    r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>{name}</type>
  <listen>unix:path={}/{name}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                    dir.display()
                ),
            )
            .unwrap();
            let mut child = Command::new(&exe)
                .arg(format!("--config-file={}", conf.display()))
                .arg("--nofork")
                .arg("--print-address=1")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut line = String::new();
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
            assert!(!line.trim().is_empty(), "dbus-daemon printed no address");
            addrs.push(line.trim().to_string());
            children.push(child);
        }
        Some(Buses { dir, session: addrs.pop().unwrap(), system: addrs.pop().unwrap(), children })
    }
}

impl Drop for Buses {
    fn drop(&mut self) {
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ── the daemon, and clients of it ───────────────────────────────────────────

/// Where a daemon runs: its buses and its home.
#[derive(Clone)]
struct Env {
    dir: PathBuf,
    system: String,
    session: String,
}

impl Env {
    fn of(b: &Buses) -> Env {
        Env { dir: b.dir.clone(), system: b.system.clone(), session: b.session.clone() }
    }

    /// No buses at all: what a machine without D-Bus looks like.
    fn unreachable(dir: &Path) -> Env {
        Env {
            dir: dir.to_path_buf(),
            system: "unix:path=/nonexistent/system".into(),
            session: "unix:path=/nonexistent/session".into(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_jayos-bluetooth"));
        c.args(args)
            .env("DBUS_SYSTEM_BUS_ADDRESS", &self.system)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.session)
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("HOME", self.dir.join("home"))
            .env("XDG_CONFIG_HOME", self.dir.join("home/.config"))
            .env_remove("XDG_DOWNLOAD_DIR")
            .env_remove("QS_RUN_BLUETOOTH");
        c
    }

    fn recv(&self) -> PathBuf {
        self.dir.join("recv")
    }
}

/// A running `daemon` or `watch`, with its events read on a thread.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    events: mpsc::Receiver<Value>,
    backlog: Vec<Value>,
    /// The newest `state`: a snapshot is the whole truth, so a condition it
    /// already meets needs no newer one.
    last: Option<Value>,
    stderr: Arc<Mutex<String>>,
}

impl Proc {
    fn spawn(env: &Env, verb: &str) -> Proc {
        let mut child = env
            .command(&[verb])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, events) = mpsc::channel();
        let out = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                let v: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
                if tx.send(v).is_err() {
                    return;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let err = child.stderr.take().unwrap();
        let sink = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if let Ok(mut s) = sink.lock() {
                    s.push_str(&line);
                    s.push('\n');
                }
            }
        });
        let stdin = child.stdin.take();
        Proc { child, stdin, events, backlog: Vec::new(), last: None, stderr }
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    /// The next event matching `pred`, looking at what was already read
    /// first. A `state` is a whole snapshot, so only the newest matters and
    /// states are never kept in the backlog.
    fn wait(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        self.wait_within(what, WAIT, pred)
    }

    fn wait_within(
        &mut self,
        what: &str,
        within: Duration,
        pred: impl Fn(&Value) -> bool,
    ) -> Value {
        if let Some(i) = self.backlog.iter().position(&pred) {
            return self.backlog.remove(i);
        }
        if let Some(last) = self.last.as_ref().filter(|v| pred(v)) {
            return last.clone();
        }
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(v) => {
                    let matched = pred(&v);
                    if v["event"] == "state" {
                        self.last = Some(v.clone());
                    } else if !matched {
                        self.backlog.push(v.clone());
                    }
                    if matched {
                        return v;
                    }
                }
                Err(_) => {
                    let err = self.stderr.lock().map(|s| s.clone()).unwrap_or_default();
                    let last: String = self
                        .last
                        .as_ref()
                        .map(Value::to_string)
                        .unwrap_or_default()
                        .chars()
                        .take(1500)
                        .collect();
                    panic!("timed out waiting for {what}; last state {last}; stderr:\n{err}");
                }
            }
        }
    }

    fn state(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        self.wait(what, |v| v["event"] == "state" && pred(v))
    }

    fn reply(&mut self, id: &str) -> Value {
        self.wait(&format!("reply {id}"), |v| v["event"] == "reply" && v["id"] == id)
    }

    fn ask(&mut self, cmd: Value) -> Value {
        let id = cmd["id"].as_str().expect("an id").to_string();
        self.send(cmd);
        self.reply(&id)
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn exit(&mut self, within: Duration) -> Option<i32> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s.code();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn device<'a>(state: &'a Value, path: &str) -> &'a Value {
    state["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["path"] == path)
        .unwrap_or_else(|| panic!("no {path} in {state}"))
}

fn prompt_of(state: &Value, kind: &str) -> Option<Value> {
    state["prompts"].as_array()?.iter().find(|p| p["kind"] == kind).cloned()
}

// ── the mocks ───────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn push(&self, s: impl Into<String>) {
        self.0.lock().unwrap().push(s.into());
    }

    fn has(&self, s: &str) -> bool {
        self.0.lock().unwrap().iter().any(|l| l == s)
    }

    fn count(&self, s: &str) -> usize {
        self.0.lock().unwrap().iter().filter(|l| *l == s).count()
    }

    fn wait(&self, s: &str) -> bool {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if self.has(s) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// Who registered as the agent: (unique name, object path).
type AgentSlot = Arc<Mutex<Option<(String, String)>>>;

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum BluezError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    AuthenticationRejected(String),
    AuthenticationCanceled(String),
}

async fn sleep(d: Duration) {
    // A thread sleeping on the executor would stall every other call on it.
    let (tx, rx) = async_channel::bounded::<()>(1);
    std::thread::spawn(move || {
        std::thread::sleep(d);
        let _ = tx.send_blocking(());
    });
    let _ = rx.recv().await;
}

struct MockAgentManager {
    log: Log,
    agent: AgentSlot,
}

#[interface(name = "org.bluez.AgentManager1")]
impl MockAgentManager {
    fn register_agent(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        agent: OwnedObjectPath,
        capability: String,
    ) {
        self.log.push(format!("RegisterAgent {} {capability}", agent.as_str()));
        *self.agent.lock().unwrap() =
            Some((hdr.sender().unwrap().to_string(), agent.as_str().to_string()));
    }

    fn unregister_agent(&self, agent: OwnedObjectPath) {
        self.log.push(format!("UnregisterAgent {}", agent.as_str()));
        *self.agent.lock().unwrap() = None;
    }

    fn request_default_agent(&self, agent: OwnedObjectPath) {
        self.log.push(format!("RequestDefaultAgent {}", agent.as_str()));
    }
}

struct MockAdapter {
    log: Log,
    address: String,
    alias: String,
    powered: bool,
    discoverable: bool,
    discoverable_timeout: u32,
    pairable: bool,
}

#[interface(name = "org.bluez.Adapter1")]
impl MockAdapter {
    fn start_discovery(&self) {
        self.log.push("StartDiscovery");
    }

    fn stop_discovery(&self) {
        self.log.push("StopDiscovery");
    }

    fn remove_device(&self, device: OwnedObjectPath) {
        self.log.push(format!("RemoveDevice {}", device.as_str()));
    }

    #[zbus(property, name = "Address")]
    fn address(&self) -> String {
        self.address.clone()
    }

    #[zbus(property, name = "Name")]
    fn name(&self) -> String {
        "archbox".into()
    }

    #[zbus(property, name = "Alias")]
    fn alias(&self) -> String {
        self.alias.clone()
    }

    #[zbus(property, name = "Alias")]
    fn set_alias(&mut self, v: String) {
        self.log.push(format!("Set Alias {v}"));
        self.alias = v;
    }

    #[zbus(property, name = "Powered")]
    fn powered(&self) -> bool {
        self.powered
    }

    #[zbus(property, name = "Powered")]
    fn set_powered(&mut self, v: bool) {
        self.log.push(format!("Set Powered {v}"));
        self.powered = v;
    }

    #[zbus(property, name = "PowerState")]
    fn power_state(&self) -> String {
        if self.powered { "on" } else { "off" }.into()
    }

    #[zbus(property, name = "Discoverable")]
    fn discoverable(&self) -> bool {
        self.discoverable
    }

    #[zbus(property, name = "Discoverable")]
    fn set_discoverable(&mut self, v: bool) {
        self.log.push(format!("Set Discoverable {v}"));
        self.discoverable = v;
    }

    #[zbus(property, name = "DiscoverableTimeout")]
    fn discoverable_timeout(&self) -> u32 {
        self.discoverable_timeout
    }

    #[zbus(property, name = "DiscoverableTimeout")]
    fn set_discoverable_timeout(&mut self, v: u32) {
        self.log.push(format!("Set DiscoverableTimeout {v}"));
        self.discoverable_timeout = v;
    }

    #[zbus(property, name = "Pairable")]
    fn pairable(&self) -> bool {
        self.pairable
    }

    #[zbus(property, name = "Pairable")]
    fn set_pairable(&mut self, v: bool) {
        self.log.push(format!("Set Pairable {v}"));
        self.pairable = v;
    }

    #[zbus(property, name = "PairableTimeout")]
    fn pairable_timeout(&self) -> u32 {
        0
    }

    #[zbus(property, name = "Discovering")]
    fn discovering(&self) -> bool {
        false
    }
}

#[derive(Clone)]
struct Dev {
    address: String,
    name: String,
    alias: String,
    icon: String,
    paired: bool,
    trusted: bool,
    blocked: bool,
    connected: bool,
    wake: bool,
    rssi: i16,
    uuids: Vec<String>,
    passkey: u32,
    connect_error: Option<String>,
}

struct MockDevice {
    log: Log,
    agent: AgentSlot,
    d: Arc<Mutex<Dev>>,
}

impl MockDevice {
    fn dev(&self) -> std::sync::MutexGuard<'_, Dev> {
        self.d.lock().unwrap()
    }
}

async fn emit_changed(
    conn: &Connection,
    path: &str,
    iface: &str,
    changed: HashMap<&str, ZValue<'_>>,
) {
    conn.emit_signal(
        None::<&str>,
        path,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        &(iface, changed, Vec::<&str>::new()),
    )
    .await
    .unwrap();
}

#[interface(name = "org.bluez.Device1")]
impl MockDevice {
    /// BlueZ's Pair asks the agent and only returns once it has answered --
    /// on the caller's own connection, while the caller is still inside Pair.
    async fn pair(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> Result<(), BluezError> {
        let path = hdr.path().unwrap().as_str().to_string();
        self.log.push(format!("Pair {path}"));
        let Some((sender, agent)) = self.agent.lock().unwrap().clone() else {
            return Err(BluezError::AuthenticationRejected("no agent".into()));
        };
        let passkey = self.dev().passkey;
        let reply = conn
            .call_method(
                Some(sender.as_str()),
                agent.as_str(),
                Some("org.bluez.Agent1"),
                "RequestConfirmation",
                &(ObjectPath::try_from(path.as_str()).unwrap(), passkey),
            )
            .await;
        match reply {
            Ok(_) => {
                self.log.push("RequestConfirmation answered ok");
                self.dev().paired = true;
                emit_changed(
                    conn,
                    &path,
                    "org.bluez.Device1",
                    HashMap::from([("Paired", ZValue::from(true)), ("Bonded", ZValue::from(true))]),
                )
                .await;
                Ok(())
            }
            Err(zbus::Error::MethodError(name, _, _)) => {
                self.log.push(format!("RequestConfirmation answered {}", name.as_str()));
                Err(BluezError::AuthenticationRejected(name.as_str().to_string()))
            }
            Err(e) => Err(BluezError::Failed(e.to_string())),
        }
    }

    async fn connect(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> Result<(), BluezError> {
        let path = hdr.path().unwrap().as_str().to_string();
        self.log.push(format!("Connect {path}"));
        let failure = self.dev().connect_error.clone();
        if let Some(message) = failure {
            // Slow enough for a state to show the device busy meanwhile.
            sleep(Duration::from_millis(700)).await;
            return Err(BluezError::Failed(message));
        }
        self.dev().connected = true;
        emit_changed(
            conn,
            &path,
            "org.bluez.Device1",
            HashMap::from([("Connected", ZValue::from(true))]),
        )
        .await;
        Ok(())
    }

    fn disconnect(&self, #[zbus(header)] hdr: Header<'_>) {
        self.log.push(format!("Disconnect {}", hdr.path().unwrap().as_str()));
    }

    fn cancel_pairing(&self, #[zbus(header)] hdr: Header<'_>) {
        self.log.push(format!("CancelPairing {}", hdr.path().unwrap().as_str()));
    }

    #[zbus(property, name = "Address")]
    fn address(&self) -> String {
        self.dev().address.clone()
    }

    #[zbus(property, name = "AddressType")]
    fn address_type(&self) -> String {
        "public".into()
    }

    #[zbus(property, name = "Name")]
    fn name(&self) -> String {
        self.dev().name.clone()
    }

    #[zbus(property, name = "Alias")]
    fn alias(&self) -> String {
        self.dev().alias.clone()
    }

    #[zbus(property, name = "Alias")]
    fn set_alias(&mut self, v: String) {
        self.log.push(format!("Set Alias {v}"));
        let name = self.dev().name.clone();
        // BlueZ's rule: an empty alias is a reset to the device's own name.
        self.dev().alias = if v.is_empty() { name } else { v };
    }

    #[zbus(property, name = "Icon")]
    fn icon(&self) -> String {
        self.dev().icon.clone()
    }

    #[zbus(property, name = "Class")]
    fn class(&self) -> u32 {
        0x240404
    }

    #[zbus(property, name = "Paired")]
    fn paired(&self) -> bool {
        self.dev().paired
    }

    #[zbus(property, name = "Bonded")]
    fn bonded(&self) -> bool {
        self.dev().paired
    }

    #[zbus(property, name = "Trusted")]
    fn trusted(&self) -> bool {
        self.dev().trusted
    }

    #[zbus(property, name = "Trusted")]
    fn set_trusted(&mut self, v: bool) {
        self.log.push(format!("Set Trusted {v}"));
        self.dev().trusted = v;
    }

    #[zbus(property, name = "Blocked")]
    fn blocked(&self) -> bool {
        self.dev().blocked
    }

    #[zbus(property, name = "Blocked")]
    fn set_blocked(&mut self, v: bool) {
        self.log.push(format!("Set Blocked {v}"));
        self.dev().blocked = v;
    }

    #[zbus(property, name = "Connected")]
    fn connected(&self) -> bool {
        self.dev().connected
    }

    #[zbus(property, name = "ServicesResolved")]
    fn services_resolved(&self) -> bool {
        self.dev().connected
    }

    #[zbus(property, name = "LegacyPairing")]
    fn legacy_pairing(&self) -> bool {
        false
    }

    #[zbus(property, name = "WakeAllowed")]
    fn wake_allowed(&self) -> bool {
        self.dev().wake
    }

    #[zbus(property, name = "WakeAllowed")]
    fn set_wake_allowed(&mut self, v: bool) {
        self.log.push(format!("Set WakeAllowed {v}"));
        self.dev().wake = v;
    }

    #[zbus(property, name = "RSSI")]
    fn rssi(&self) -> i16 {
        self.dev().rssi
    }

    #[zbus(property, name = "TxPower")]
    fn tx_power(&self) -> i16 {
        4
    }

    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> Vec<String> {
        self.dev().uuids.clone()
    }

    #[zbus(property, name = "Modalias")]
    fn modalias(&self) -> String {
        "bluetooth:v004Cp0001d0001".into()
    }

    #[zbus(property, name = "Adapter")]
    fn adapter(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(ADAPTER).unwrap()
    }
}

/// A device that advertises only an address: no Name property at all.
struct MockBareDevice;

#[interface(name = "org.bluez.Device1")]
impl MockBareDevice {
    #[zbus(property, name = "Address")]
    fn address(&self) -> String {
        "AA:BB:CC:DD:EE:03".into()
    }

    #[zbus(property, name = "Alias")]
    fn alias(&self) -> String {
        "AA-BB-CC-DD-EE-03".into()
    }

    #[zbus(property, name = "Paired")]
    fn paired(&self) -> bool {
        false
    }

    #[zbus(property, name = "Adapter")]
    fn adapter(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(ADAPTER).unwrap()
    }
}

/// A phone under some other adapter, for the machine with two radios.
struct MockFarDevice {
    address: String,
    adapter: String,
}

#[interface(name = "org.bluez.Device1")]
impl MockFarDevice {
    #[zbus(property, name = "Address")]
    fn address(&self) -> String {
        self.address.clone()
    }

    #[zbus(property, name = "Alias")]
    fn alias(&self) -> String {
        "Far phone".into()
    }

    #[zbus(property, name = "Name")]
    fn name(&self) -> String {
        "Far phone".into()
    }

    #[zbus(property, name = "Paired")]
    fn paired(&self) -> bool {
        true
    }

    #[zbus(property, name = "Adapter")]
    fn adapter(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(self.adapter.as_str()).unwrap()
    }
}

struct MockBattery;

#[interface(name = "org.bluez.Battery1")]
impl MockBattery {
    #[zbus(property, name = "Percentage")]
    fn percentage(&self) -> u8 {
        80
    }
}

fn dev(address: &str, name: &str, paired: bool, trusted: bool, passkey: u32) -> Arc<Mutex<Dev>> {
    Arc::new(Mutex::new(Dev {
        address: address.into(),
        name: name.into(),
        alias: name.into(),
        icon: "phone".into(),
        paired,
        trusted,
        blocked: false,
        connected: false,
        wake: false,
        rssi: -70,
        uuids: vec!["00001105-0000-1000-8000-00805f9b34fb".into()],
        passkey,
        connect_error: None,
    }))
}

struct Bluez {
    conn: blocking::Connection,
    log: Log,
    agent: AgentSlot,
    phone: Arc<Mutex<Dev>>,
}

impl Bluez {
    fn start(addr: &str) -> Bluez {
        let log = Log::default();
        let agent: AgentSlot = Arc::default();
        let phone = dev("AA:BB:CC:DD:EE:01", "Phone", false, false, 12345);
        let headset = dev("AA:BB:CC:DD:EE:02", "Headset", true, true, 0);
        {
            let mut h = headset.lock().unwrap();
            h.icon = "audio-headset".into();
            h.connect_error = Some("br-connection-page-timeout".into());
        }
        let conn = blocking::connection::Builder::address(addr)
            .unwrap()
            .name("org.bluez")
            .unwrap()
            .serve_at("/", fdo::ObjectManager)
            .unwrap()
            .serve_at("/org/bluez", MockAgentManager { log: log.clone(), agent: agent.clone() })
            .unwrap()
            .serve_at(
                ADAPTER,
                MockAdapter {
                    log: log.clone(),
                    address: "00:11:22:33:44:55".into(),
                    alias: "laptop".into(),
                    powered: true,
                    discoverable: false,
                    discoverable_timeout: 180,
                    pairable: true,
                },
            )
            .unwrap()
            .serve_at(
                PHONE,
                MockDevice { log: log.clone(), agent: agent.clone(), d: phone.clone() },
            )
            .unwrap()
            .serve_at(HEADSET, MockDevice { log: log.clone(), agent: agent.clone(), d: headset })
            .unwrap()
            .serve_at(HEADSET, MockBattery)
            .unwrap()
            .serve_at(BARE, MockBareDevice)
            .unwrap()
            .build()
            .unwrap();
        Bluez { conn, log, agent, phone }
    }

    fn emit(
        &self,
        path: &str,
        iface: &str,
        changed: HashMap<&str, ZValue<'_>>,
        invalidated: Vec<&str>,
    ) {
        self.conn
            .emit_signal(
                None::<&str>,
                path,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(iface, changed, invalidated),
            )
            .unwrap();
    }

    /// Call the daemon's agent the way bluetoothd would, on a thread, since
    /// the call only returns once somebody answers.
    fn agent_call<B>(
        &self,
        method: &'static str,
        body: B,
    ) -> mpsc::Receiver<Result<zbus::Message, zbus::Error>>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType + Send + 'static,
    {
        let (sender, path) = self.agent.lock().unwrap().clone().expect("an agent registered");
        let conn = self.conn.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = conn.call_method(
                Some(sender.as_str()),
                path.as_str(),
                Some("org.bluez.Agent1"),
                method,
                &body,
            );
            let _ = tx.send(r);
        });
        rx
    }
}

fn error_name(r: &Result<zbus::Message, zbus::Error>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(zbus::Error::MethodError(n, _, _)) => n.as_str().to_string(),
        Err(e) => e.to_string(),
    }
}

// ── mock obexd ──

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.obex.Error")]
enum ObexdError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
}

/// Where real obexd would write a pushed file, given its agent's answer, or
/// None if it would refuse. Only org.bluez.obex.Error.Rejected refuses
/// (obexd/src/manager.c:642-645 and 736-737); any other error, and no answer
/// at all, leaves the folder and name unset, and opp_chkput then saves under
/// its root folder with the sender's name (obexd/plugins/opp.c:73-77).
fn obexd_writes(answer: &Result<String, String>, sent_as: &str) -> Option<String> {
    match answer {
        Ok(path) => Some(path.clone()),
        Err(name) if name == "org.bluez.obex.Error.Rejected" => None,
        Err(_) => Some(format!("~/.cache/obexd/{sent_as}")),
    }
}

struct TransferState {
    status: String,
    name: String,
    size: u64,
    transferred: u64,
    session: String,
}

struct MockTransfer {
    log: Log,
    t: Arc<Mutex<TransferState>>,
}

#[interface(name = "org.bluez.obex.Transfer1")]
impl MockTransfer {
    /// obexd ends a cancelled transfer as it ends any other: Status, then the
    /// object unregistered (client/transfer.c, transfer_complete).
    async fn cancel(&self, #[zbus(connection)] conn: &Connection, #[zbus(header)] hdr: Header<'_>) {
        let path = hdr.path().unwrap().as_str().to_string();
        self.log.push(format!("Cancel {path}"));
        self.t.lock().unwrap().status = "error".into();
        emit_changed(
            conn,
            &path,
            "org.bluez.obex.Transfer1",
            HashMap::from([("Status", ZValue::from("error"))]),
        )
        .await;
        // From a thread: this method holds the object, and removing it from
        // inside would wait on itself.
        let conn = blocking::Connection::from(conn.clone());
        std::thread::spawn(move || {
            let _ = conn.object_server().remove::<MockTransfer, _>(path.as_str());
        });
    }

    #[zbus(property, name = "Status")]
    fn status(&self) -> String {
        self.t.lock().unwrap().status.clone()
    }

    #[zbus(property, name = "Name")]
    fn name(&self) -> String {
        self.t.lock().unwrap().name.clone()
    }

    #[zbus(property, name = "Size")]
    fn size(&self) -> u64 {
        self.t.lock().unwrap().size
    }

    #[zbus(property, name = "Transferred")]
    fn transferred(&self) -> u64 {
        self.t.lock().unwrap().transferred
    }

    #[zbus(property, name = "Session")]
    fn session(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(self.t.lock().unwrap().session.clone()).unwrap()
    }
}

struct MockSession {
    destination: String,
    source: String,
}

#[interface(name = "org.bluez.obex.Session1")]
impl MockSession {
    #[zbus(property, name = "Destination")]
    fn destination(&self) -> String {
        self.destination.clone()
    }

    #[zbus(property, name = "Source")]
    fn source(&self) -> String {
        self.source.clone()
    }
}

#[derive(Clone, Default)]
struct ObexShared {
    log: Log,
    agent: AgentSlot,
    next: Arc<AtomicU32>,
    /// The next SendFile stalls part-way, for the cancel test.
    slow: Arc<AtomicBool>,
    /// Every SendFile completes at once, with no progress in between: the
    /// end-of-transfer signals back to back, as often as a test can ask.
    fast: Arc<AtomicBool>,
    /// The next SendFile's transfer is lost with the link.
    dies: Arc<AtomicBool>,
    /// Transfer objects appear this long after SendFile has answered.
    late_ms: Arc<AtomicU64>,
    /// CreateSession fails, as it does for a phone that is out of range.
    refuse: Arc<AtomicBool>,
    /// The next SendFile's obexd leaves before it answers: 1 having completed
    /// the transfer, 2 part-way through it. Its signals and the name's release
    /// are on the bus ahead of the reply, and the reply is held a while so the
    /// core hears that obexd left before it hears of the transfer.
    leaves: Arc<AtomicU32>,
}

/// How a mock transfer runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Run {
    Normal,
    Slow,
    Fast,
    Dies,
}

struct MockObexManager {
    s: ObexShared,
}

#[interface(name = "org.bluez.obex.AgentManager1")]
impl MockObexManager {
    fn register_agent(&self, #[zbus(header)] hdr: Header<'_>, agent: OwnedObjectPath) {
        self.s.log.push(format!("RegisterAgent {}", agent.as_str()));
        *self.s.agent.lock().unwrap() =
            Some((hdr.sender().unwrap().to_string(), agent.as_str().to_string()));
    }

    fn unregister_agent(&self, agent: OwnedObjectPath) {
        self.s.log.push(format!("UnregisterAgent {}", agent.as_str()));
        *self.s.agent.lock().unwrap() = None;
    }
}

struct MockClient {
    s: ObexShared,
}

#[interface(name = "org.bluez.obex.Client1")]
impl MockClient {
    async fn create_session(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        destination: String,
        args: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedObjectPath> {
        let mut keys: Vec<String> = args
            .iter()
            .map(|(k, v)| {
                format!("{k}={}", String::try_from(v.try_clone().unwrap()).unwrap_or_default())
            })
            .collect();
        keys.sort();
        self.s.log.push(format!("CreateSession {destination} {}", keys.join(" ")));
        if self.s.refuse.load(Ordering::SeqCst) {
            return Err(fdo::Error::Failed("Unable to connect".into()));
        }
        let n = self.s.next.fetch_add(1, Ordering::SeqCst);
        let path = OwnedObjectPath::try_from(format!("/org/bluez/obex/client/session{n}")).unwrap();
        let push = MockPush {
            s: self.s.clone(),
            session: path.as_str().to_string(),
            dead: Arc::default(),
        };
        server.at(&path, push).await?;
        server.at(&path, MockSession { destination, source: String::new() }).await?;
        Ok(path)
    }

    fn remove_session(&self, session: OwnedObjectPath) {
        self.s.log.push(format!("RemoveSession {}", session.as_str()));
    }
}

struct MockPush {
    s: ObexShared,
    session: String,
    /// The link under this session has dropped.
    dead: Arc<AtomicBool>,
}

#[interface(name = "org.bluez.obex.ObjectPush1")]
impl MockPush {
    #[zbus(out_args("transfer", "properties"))]
    async fn send_file(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(connection)] conn: &Connection,
        sourcefile: String,
    ) -> Result<(OwnedObjectPath, HashMap<String, OwnedValue>), ObexdError> {
        self.s.log.push(format!("SendFile {sourcefile}"));
        // obexd's own answer while a session's link is gone and the session
        // not yet unregistered (client/session.c:860-864, client/opp.c:62-69).
        if self.dead.load(Ordering::SeqCst) {
            return Err(ObexdError::Failed("Session not connected".into()));
        }
        let size = std::fs::metadata(&sourcefile).map(|m| m.len()).unwrap_or(0);
        let n = self.s.next.fetch_add(1, Ordering::SeqCst);
        let path = format!("{}/transfer{n}", self.session);
        let name = Path::new(&sourcefile).file_name().unwrap().to_string_lossy().into_owned();
        let t = Arc::new(Mutex::new(TransferState {
            status: "queued".into(),
            name: name.clone(),
            size,
            transferred: 0,
            session: self.session.clone(),
        }));
        let run = if self.s.dies.swap(false, Ordering::SeqCst) {
            Run::Dies
        } else if self.s.slow.swap(false, Ordering::SeqCst) {
            Run::Slow
        } else if self.s.fast.load(Ordering::SeqCst) {
            Run::Fast
        } else {
            Run::Normal
        };
        let leaves = self.s.leaves.swap(0, Ordering::SeqCst);
        if leaves != 0 {
            server
                .at(path.as_str(), MockTransfer { log: self.s.log.clone(), t: t.clone() })
                .await?;
            let props_iface = "org.freedesktop.DBus.Properties";
            t.lock().unwrap().status = "active".into();
            let active = (
                "org.bluez.obex.Transfer1",
                HashMap::from([("Status", ZValue::from("active"))]),
                Vec::<&str>::new(),
            );
            conn.emit_signal(
                None::<&str>,
                path.as_str(),
                props_iface,
                "PropertiesChanged",
                &active,
            )
            .await?;
            if leaves == 1 {
                t.lock().unwrap().status = "complete".into();
                let done = (
                    "org.bluez.obex.Transfer1",
                    HashMap::from([
                        ("Transferred", ZValue::from(size)),
                        ("Status", ZValue::from("complete")),
                    ]),
                    Vec::<&str>::new(),
                );
                conn.emit_signal(
                    None::<&str>,
                    path.as_str(),
                    props_iface,
                    "PropertiesChanged",
                    &done,
                )
                .await?;
                server.remove::<MockTransfer, _>(path.as_str()).await?;
                self.s.log.push(format!("Complete {path}"));
            }
            conn.release_name("org.bluez.obex").await?;
            std::thread::sleep(Duration::from_millis(400));
            let mut props = HashMap::new();
            props.insert("Size".to_string(), OwnedValue::from(size));
            props.insert("Name".to_string(), OwnedValue::try_from(ZValue::from(name)).unwrap());
            return Ok((OwnedObjectPath::try_from(path).unwrap(), props));
        }
        let late = Duration::from_millis(self.s.late_ms.load(Ordering::SeqCst));
        let object = MockTransfer { log: self.s.log.clone(), t: t.clone() };
        // obexd registers the transfer before it answers (client/session.c,
        // obc_session_queue), so InterfacesAdded is ahead of the reply.
        let object = if late.is_zero() {
            server.at(path.as_str(), object).await?;
            None
        } else {
            Some(object)
        };
        let conn = blocking::Connection::from(conn.clone());
        let (p, log, dead) = (path.clone(), self.s.log.clone(), self.dead.clone());
        std::thread::spawn(move || {
            if let Some(object) = object {
                std::thread::sleep(late);
                conn.object_server().at(p.as_str(), object).unwrap();
            }
            progress(&conn, &log, &p, &dead, &t, size, run)
        });
        let mut props = HashMap::new();
        props.insert("Size".to_string(), OwnedValue::from(size));
        props.insert("Name".to_string(), OwnedValue::try_from(ZValue::from(name)).unwrap());
        Ok((OwnedObjectPath::try_from(path).unwrap(), props))
    }
}

/// What obexd's client does: active, Transferred every so often, then the
/// end exactly as obexd sends it -- the last Transferred and Status in one
/// PropertiesChanged, and the object unregistered straight after, from the
/// same thread on the same connection (client/transfer.c xfer_complete, then
/// gdbus/object.c remove_interface, which flushes the properties first). A
/// slow transfer stops at a tenth and waits to be cancelled; one that dies
/// fails as the link drops, and leaves its session unable to send.
fn progress(
    conn: &blocking::Connection,
    log: &Log,
    path: &str,
    dead: &AtomicBool,
    t: &Arc<Mutex<TransferState>>,
    size: u64,
    run: Run,
) {
    let emit = |changed: HashMap<&str, ZValue<'_>>| {
        let _ = conn.emit_signal(
            None::<&str>,
            path,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.obex.Transfer1", changed, Vec::<&str>::new()),
        );
    };
    let server = conn.object_server();
    if run != Run::Fast {
        std::thread::sleep(Duration::from_millis(50));
    }
    t.lock().unwrap().status = "active".into();
    emit(HashMap::from([("Status", ZValue::from("active"))]));
    if run == Run::Dies {
        std::thread::sleep(Duration::from_millis(100));
        dead.store(true, Ordering::SeqCst);
        t.lock().unwrap().status = "error".into();
        emit(HashMap::from([("Status", ZValue::from("error"))]));
        let _ = server.remove::<MockTransfer, _>(path);
        log.push(format!("Lost {path}"));
        return;
    }
    // A slow transfer reaches its tenth once and then waits. It said so
    // again every 150 ms for over a second, and a test speaking for obexd in
    // that time -- `a_signal_from_anyone_but_the_service_is_ignored` sends
    // Transferred 900 -- was overwritten before it looked whenever a loaded
    // CPU stretched the wait before (found by `check_rust_tests`, 2026-11-07).
    if run == Run::Slow {
        std::thread::sleep(Duration::from_millis(150));
        if t.lock().unwrap().status == "active" {
            t.lock().unwrap().transferred = size / 10;
            emit(HashMap::from([("Transferred", ZValue::from(size / 10))]));
        }
        return;
    }
    if run != Run::Fast {
        let steps = 8u64;
        for i in 1..=steps {
            std::thread::sleep(Duration::from_millis(150));
            if t.lock().unwrap().status != "active" {
                return;
            }
            let done = size * i / steps;
            t.lock().unwrap().transferred = done;
            emit(HashMap::from([("Transferred", ZValue::from(done))]));
        }
    }
    {
        let mut t = t.lock().unwrap();
        t.status = "complete".into();
        t.transferred = size;
    }
    // Logged before it is said, so nothing can have heard it sooner.
    log.push(format!("Complete {path}"));
    emit(HashMap::from([
        ("Transferred", ZValue::from(size)),
        ("Status", ZValue::from("complete")),
    ]));
    let _ = server.remove::<MockTransfer, _>(path);
}

struct Obexd {
    conn: blocking::Connection,
    s: ObexShared,
}

impl Obexd {
    fn start(addr: &str) -> Obexd {
        let s = ObexShared::default();
        let conn = blocking::connection::Builder::address(addr)
            .unwrap()
            .name("org.bluez.obex")
            .unwrap()
            .serve_at("/", fdo::ObjectManager)
            .unwrap()
            .serve_at("/org/bluez/obex", MockObexManager { s: s.clone() })
            .unwrap()
            .serve_at("/org/bluez/obex", MockClient { s: s.clone() })
            .unwrap()
            .build()
            .unwrap();
        Obexd { conn, s }
    }

    /// An incoming push, as obexd's server presents one, and the agent asked
    /// about it on a thread.
    fn push(
        &self,
        n: u32,
        name: &str,
        from: &str,
    ) -> (String, mpsc::Receiver<Result<String, String>>) {
        let session = format!("/org/bluez/obex/server/session{n}");
        let transfer = format!("{session}/transfer{n}");
        let server = self.conn.object_server();
        server
            .at(
                session.as_str(),
                MockSession { destination: from.into(), source: "00:11:22:33:44:55".into() },
            )
            .unwrap();
        let t = Arc::new(Mutex::new(TransferState {
            status: "queued".into(),
            name: name.into(),
            size: 1234,
            transferred: 0,
            session: session.clone(),
        }));
        server.at(transfer.as_str(), MockTransfer { log: self.s.log.clone(), t }).unwrap();
        let rx = self.authorize(&transfer);
        (transfer, rx)
    }

    /// AuthorizePush for `transfer`, as obexd calls it, on a thread.
    fn authorize(&self, transfer: &str) -> mpsc::Receiver<Result<String, String>> {
        let (sender, agent) =
            self.s.agent.lock().unwrap().clone().expect("an obex agent registered");
        let conn = self.conn.clone();
        let (tx, rx) = mpsc::channel();
        let tp = transfer.to_string();
        std::thread::spawn(move || {
            let r = conn
                .call_method(
                    Some(sender.as_str()),
                    agent.as_str(),
                    Some("org.bluez.obex.Agent1"),
                    "AuthorizePush",
                    &(ObjectPath::try_from(tp.as_str()).unwrap(),),
                )
                .map_err(|e| match e {
                    zbus::Error::MethodError(n, _, _) => n.as_str().to_string(),
                    e => e.to_string(),
                })
                .and_then(|m| m.body().deserialize::<String>().map_err(|e| e.to_string()));
            let _ = tx.send(r);
        });
        rx
    }

    fn emit(&self, path: &str, changed: HashMap<&str, ZValue<'_>>) {
        self.conn
            .emit_signal(
                None::<&str>,
                path,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &("org.bluez.obex.Transfer1", changed, Vec::<&str>::new()),
            )
            .unwrap();
    }
}

/// xdg-desktop-portal's file chooser: answers OpenFile with a handle, and the
/// choice as a Response on that handle -- sent BEFORE the method reply, which
/// only a caller that subscribed first can see.
struct MockPortal {
    log: Log,
    answer: Arc<Mutex<(u32, Vec<String>)>>,
    /// Another connection on the bus, and the files it claims were chosen:
    /// it answers first, once to everybody and once to the caller alone.
    forger: Option<(Connection, Vec<String>)>,
}

#[interface(name = "org.freedesktop.portal.FileChooser")]
impl MockPortal {
    async fn open_file(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedObjectPath> {
        let token = String::try_from(options["handle_token"].try_clone().unwrap()).unwrap();
        let multiple = bool::try_from(&*options["multiple"]).unwrap_or(false);
        let caller = hdr.sender().unwrap().to_string();
        let handle = format!(
            "/org/freedesktop/portal/desktop/request/{}/{token}",
            caller.trim_start_matches(':').replace('.', "_")
        );
        self.log.push(format!("OpenFile {title} multiple={multiple}"));
        if let Some((forger, uris)) = &self.forger {
            let forged: HashMap<&str, ZValue> =
                HashMap::from([("uris", ZValue::from(uris.clone()))]);
            for to in [None, Some(caller.as_str())] {
                forger
                    .emit_signal(
                        to,
                        handle.as_str(),
                        "org.freedesktop.portal.Request",
                        "Response",
                        &(0u32, &forged),
                    )
                    .await
                    .unwrap();
            }
            // Long enough that the forgeries are what reach the daemon first.
            sleep(Duration::from_millis(300)).await;
        }
        let (code, uris) = self.answer.lock().unwrap().clone();
        let results: HashMap<&str, ZValue> = HashMap::from([("uris", ZValue::from(uris))]);
        conn.emit_signal(
            Some(caller.as_str()),
            handle.as_str(),
            "org.freedesktop.portal.Request",
            "Response",
            &(code, results),
        )
        .await
        .unwrap();
        Ok(OwnedObjectPath::try_from(handle).unwrap())
    }
}

/// Buses, both mocks, and a daemon configured to receive into `recv`.
struct Rig {
    env: Env,
    bluez: Bluez,
    obexd: Obexd,
    daemon: Proc,
    _buses: Buses,
}

impl Rig {
    fn start() -> Option<Rig> {
        let buses = Buses::start()?;
        let env = Env::of(&buses);
        let bluez = Bluez::start(&buses.system);
        let obexd = Obexd::start(&buses.session);
        let mut daemon = Proc::spawn(&env, "daemon");
        let recv = env.recv();
        daemon.ask(
            json!({"id": "cfg", "op": "config", "agent": true, "files": "ask", "receiveDir": recv}),
        );
        daemon.state("agents registered and the tree read", |s| {
            s["bluez"] == true
                && s["agent"] == "ready"
                && s["obex"] == "ready"
                && s["devices"].as_array().is_some_and(|d| d.len() == 3)
        });
        Some(Rig { env, bluez, obexd, daemon, _buses: buses })
    }
}

// ── the tests ───────────────────────────────────────────────────────────────

#[test]
fn the_initial_state_mirrors_bluez_and_follows_its_changes() {
    let Some(mut rig) = Rig::start() else { return };
    let d = &mut rig.daemon;
    assert!(
        rig.bluez.log.has(&format!("RegisterAgent {AGENT} KeyboardDisplay")),
        "{:?}",
        rig.bluez.log.all()
    );
    assert!(rig.bluez.log.has(&format!("RequestDefaultAgent {AGENT}")));
    assert!(rig.obexd.s.log.has("RegisterAgent /org/jayos/bluetooth/obex"));

    let s = d.state("the full tree", |s| s["adapters"].as_array().is_some_and(|a| a.len() == 1));
    let a = &s["adapters"][0];
    assert_eq!(a["path"], ADAPTER);
    assert_eq!(a["id"], "hci0");
    assert_eq!(a["name"], "laptop");
    assert_eq!(a["systemName"], "archbox");
    assert_eq!(a["discoverableTimeout"], 180);
    assert_eq!(s["receiveDir"], rig.env.recv().to_str().unwrap());
    let phone = device(&s, PHONE);
    assert_eq!(phone["name"], "Phone");
    assert_eq!(phone["rssi"], -70);
    assert_eq!(phone["txPower"], 4);
    assert_eq!(phone["class"], 0x240404);
    assert_eq!(phone["battery"], Value::Null);
    let headset = device(&s, HEADSET);
    assert_eq!(headset["battery"], 80);
    assert_eq!(headset["paired"], true);
    let bare = device(&s, BARE);
    assert_eq!(bare["deviceName"], "", "a device with no Name property has an empty deviceName");
    assert_eq!(bare["name"], "AA-BB-CC-DD-EE-03");
    assert_eq!(bare["rssi"], Value::Null);
    assert_eq!(bare["uuids"], json!([]));

    // `snapshot` sends a fresh state to the asker, then the reply.
    d.send(json!({"id": "snap", "op": "snapshot"}));
    d.wait("the snapshot's state", |v| v["event"] == "state");
    assert_eq!(d.reply("snap")["ok"], true);

    // A second client sees the same tree the moment it connects.
    let mut w = Proc::spawn(&rig.env, "watch");
    w.state("watch's first state", |s| s["devices"].as_array().is_some_and(|d| d.len() == 3));

    rig.bluez.emit(
        PHONE,
        "org.bluez.Device1",
        HashMap::from([("RSSI", ZValue::from(-42i16))]),
        vec![],
    );
    d.state("the RSSI change", |s| device(s, PHONE)["rssi"] == -42);
    w.state("the RSSI change, on the socket", |s| device(s, PHONE)["rssi"] == -42);
    rig.bluez.emit(PHONE, "org.bluez.Device1", HashMap::new(), vec!["RSSI", "TxPower"]);
    d.state("RSSI invalidated", |s| {
        device(s, PHONE)["rssi"].is_null() && device(s, PHONE)["txPower"].is_null()
    });

    let ops = [
        (json!({"op": "power", "adapter": ADAPTER, "on": false}), "Set Powered false"),
        (
            json!({"op": "adapter-name", "adapter": ADAPTER, "name": "workbench"}),
            "Set Alias workbench",
        ),
        (json!({"op": "discoverable", "adapter": ADAPTER, "on": true}), "Set Discoverable true"),
        (
            json!({"op": "discoverable-timeout", "adapter": ADAPTER, "seconds": 300}),
            "Set DiscoverableTimeout 300",
        ),
        (json!({"op": "pairable", "adapter": ADAPTER, "on": false}), "Set Pairable false"),
        (json!({"op": "trust", "device": PHONE, "on": true}), "Set Trusted true"),
        (json!({"op": "block", "device": PHONE, "on": true}), "Set Blocked true"),
        (json!({"op": "wake", "device": PHONE, "on": true}), "Set WakeAllowed true"),
        (json!({"op": "rename", "device": PHONE, "name": "Work phone"}), "Set Alias Work phone"),
        (
            json!({"op": "remove", "device": BARE}),
            "RemoveDevice /org/bluez/hci0/dev_AA_BB_CC_DD_EE_03",
        ),
        (
            json!({"op": "disconnect", "device": HEADSET}),
            "Disconnect /org/bluez/hci0/dev_AA_BB_CC_DD_EE_02",
        ),
        (
            json!({"op": "cancel-pair", "device": PHONE}),
            "CancelPairing /org/bluez/hci0/dev_AA_BB_CC_DD_EE_01",
        ),
    ];
    for (i, (mut cmd, logged)) in ops.into_iter().enumerate() {
        let id = format!("op{i}");
        cmd["id"] = json!(id);
        let r = d.ask(cmd.clone());
        assert_eq!(r["ok"], true, "{cmd} -> {r}");
        assert!(rig.bluez.log.has(logged), "{cmd}: the mock saw {:?}", rig.bluez.log.all());
    }
    let s = d.state("the property changes", |s| {
        s["adapters"][0]["name"] == "workbench"
            && s["adapters"][0]["powered"] == false
            && s["adapters"][0]["discoverableTimeout"] == 300
            && device(s, PHONE)["name"] == "Work phone"
            && device(s, PHONE)["wakeAllowed"] == true
    });
    assert_eq!(device(&s, PHONE)["trusted"], true);
    assert_eq!(device(&s, PHONE)["blocked"], true);
    assert_eq!(s["adapters"][0]["pairable"], false);
    assert_eq!(s["adapters"][0]["discoverable"], true);

    // An empty name is BlueZ's reset to the device's own.
    let r = d.ask(json!({"id": "reset", "op": "rename", "device": PHONE, "name": ""}));
    assert_eq!(r["ok"], true);
    d.state("the alias reset", |s| device(s, PHONE)["name"] == "Phone");

    // A failing Connect: the D-Bus error name and BlueZ's reason, and the
    // device marked busy while it runs.
    d.send(json!({"id": "c1", "op": "connect", "device": HEADSET}));
    d.state("busy while connecting", |s| device(s, HEADSET)["busy"] == "connect");
    let r = d.reply("c1");
    assert_eq!(
        r,
        json!({"event": "reply", "id": "c1", "op": "connect", "ok": false,
        "error": "org.bluez.Error.Failed", "message": "br-connection-page-timeout"})
    );
    d.state("not busy after the reply", |s| device(s, HEADSET)["busy"] == "");

    // The daemon's own refusals.
    assert_eq!(
        d.ask(json!({"id": "x1", "op": "connect", "device": "/org/bluez/hci0/dev_00"}))["error"],
        "no-such-device"
    );
    assert_eq!(
        d.ask(json!({"id": "x2", "op": "power", "adapter": "/org/bluez/hci9", "on": true}))
            ["error"],
        "no-such-adapter"
    );
    assert_eq!(d.ask(json!({"id": "x3", "op": "teleport"}))["error"], "unknown-op");
    assert_eq!(
        d.ask(json!({"id": "x4", "op": "power", "adapter": ADAPTER}))["error"],
        "bad-request"
    );
    assert_eq!(
        d.ask(json!({"id": "x5", "op": "answer", "prompt": 999, "accept": true}))["error"],
        "no-such-prompt"
    );
    assert_eq!(
        d.ask(json!({"id": "x6", "op": "cancel", "transfer": "/nope"}))["error"],
        "no-such-transfer"
    );
    // Garbage still gets a reply, with an empty id.
    d.send(json!("not an object"));
    let r = d.reply("");
    assert_eq!(r["error"], "bad-request");
}

#[test]
fn pairing_prompts_reach_every_client_and_answers_reach_bluez() {
    let Some(mut rig) = Rig::start() else { return };
    let mut w = Proc::spawn(&rig.env, "watch");
    w.state("watch connected", |s| s["bluez"] == true);
    let d = &mut rig.daemon;

    // pair -> RequestConfirmation -> a prompt for both clients -> accept.
    d.send(json!({"id": "p1", "op": "pair", "device": PHONE}));
    let s = d.state("the confirm prompt", |s| prompt_of(s, "confirm").is_some());
    let p = prompt_of(&s, "confirm").unwrap();
    assert_eq!(p["passkey"], "012345", "zero-padded to six digits");
    assert_eq!(p["device"], PHONE);
    assert_eq!(p["name"], "Phone");
    assert_eq!(p["address"], "AA:BB:CC:DD:EE:01");
    assert!(p["expires"].as_i64().unwrap() > 0);
    assert_eq!(device(&s, PHONE)["busy"], "pair");
    let ws = w.state("the same prompt on the socket", |s| prompt_of(s, "confirm").is_some());
    assert_eq!(prompt_of(&ws, "confirm").unwrap()["id"], p["id"]);
    let r = d.ask(json!({"id": "a1", "op": "answer", "prompt": p["id"], "accept": true}));
    assert_eq!(r["ok"], true);
    let r = d.reply("p1");
    assert_eq!(
        r,
        json!({"event": "reply", "id": "p1", "op": "pair", "ok": true, "connected": true})
    );
    assert!(rig.bluez.log.has("RequestConfirmation answered ok"));
    let log = rig.bluez.log.all();
    let at =
        |s: &str| log.iter().position(|l| l == s).unwrap_or_else(|| panic!("{s} not in {log:?}"));
    assert!(
        at("RequestConfirmation answered ok") < at("Set Trusted true"),
        "trusted after pairing: {log:?}"
    );
    assert!(
        at("Set Trusted true") < at(&format!("Connect {PHONE}")),
        "connected after trusting: {log:?}"
    );
    let s = d.state("the prompt gone and the phone paired", |s| {
        prompt_of(s, "confirm").is_none() && device(s, PHONE)["paired"] == true
    });
    assert_eq!(device(&s, PHONE)["trusted"], true);
    w.state("the prompt gone on the socket too", |s| prompt_of(s, "confirm").is_none());

    // RequestPasskey, then BlueZ's Cancel: the prompt goes and the call ends
    // as Canceled.
    let call = rig.bluez.agent_call("RequestPasskey", (ObjectPath::try_from(PHONE).unwrap(),));
    d.state("the passkey prompt", |s| prompt_of(s, "passkey").is_some());
    let r = rig.bluez.agent_call("Cancel", ());
    assert_eq!(error_name(&r.recv_timeout(WAIT).unwrap()), "ok");
    assert_eq!(error_name(&call.recv_timeout(WAIT).unwrap()), "org.bluez.Error.Canceled");
    d.state("the passkey prompt gone", |s| s["prompts"] == json!([]));

    // RequestPasskey answered: 000000 is a passkey, and a bad one is refused
    // without losing the prompt.
    let call = rig.bluez.agent_call("RequestPasskey", (ObjectPath::try_from(PHONE).unwrap(),));
    let s = d.state("the second passkey prompt", |s| prompt_of(s, "passkey").is_some());
    let id = prompt_of(&s, "passkey").unwrap()["id"].clone();
    let r =
        d.ask(json!({"id": "bad", "op": "answer", "prompt": id, "accept": true, "value": "12a"}));
    assert_eq!(r["error"], "bad-request");
    let r = d.ask(
        json!({"id": "good", "op": "answer", "prompt": id, "accept": true, "value": "000000"}),
    );
    assert_eq!(r["ok"], true);
    let m = call.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(m.body().deserialize::<u32>().unwrap(), 0);

    // DisplayPasskey answers at once, shows a display prompt, and updates it
    // in place; the device pairing takes it away.
    let r =
        rig.bluez.agent_call("DisplayPasskey", (ObjectPath::try_from(BARE).unwrap(), 42u32, 0u16));
    assert_eq!(
        error_name(&r.recv_timeout(Duration::from_secs(2)).unwrap()),
        "ok",
        "DisplayPasskey is not held"
    );
    let s = d.state("the display prompt", |s| prompt_of(s, "displayPasskey").is_some());
    let p = prompt_of(&s, "displayPasskey").unwrap();
    assert_eq!((p["passkey"].as_str(), p["entered"].as_i64()), (Some("000042"), Some(0)));
    let r =
        rig.bluez.agent_call("DisplayPasskey", (ObjectPath::try_from(BARE).unwrap(), 42u32, 3u16));
    assert_eq!(error_name(&r.recv_timeout(WAIT).unwrap()), "ok");
    let s = d.state("the display prompt updated", |s| {
        prompt_of(s, "displayPasskey").is_some_and(|p| p["entered"] == 3)
    });
    assert_eq!(
        prompt_of(&s, "displayPasskey").unwrap()["id"],
        p["id"],
        "the same prompt, not a second"
    );
    assert_eq!(s["prompts"].as_array().unwrap().len(), 1);
    rig.bluez.emit(
        BARE,
        "org.bluez.Device1",
        HashMap::from([("Paired", ZValue::from(true))]),
        vec![],
    );
    d.state("the display prompt gone once paired", |s| s["prompts"] == json!([]));

    // DisplayPinCode, ended by Cancel.
    let r = rig.bluez.agent_call("DisplayPinCode", (ObjectPath::try_from(PHONE).unwrap(), "0000"));
    assert_eq!(error_name(&r.recv_timeout(WAIT).unwrap()), "ok");
    d.state("the PIN shown", |s| {
        prompt_of(s, "displayPin").is_some_and(|p| p["passkey"] == "0000")
    });
    let _ = rig.bluez.agent_call("Cancel", ()).recv_timeout(WAIT).unwrap();
    d.state("the PIN gone on Cancel", |s| s["prompts"] == json!([]));

    // RequestAuthorization, rejected.
    let call =
        rig.bluez.agent_call("RequestAuthorization", (ObjectPath::try_from(PHONE).unwrap(),));
    let s = d.state("the authorisation prompt", |s| prompt_of(s, "authorize").is_some());
    let id = prompt_of(&s, "authorize").unwrap()["id"].clone();
    d.ask(json!({"id": "rej", "op": "answer", "prompt": id, "accept": false}));
    assert_eq!(error_name(&call.recv_timeout(WAIT).unwrap()), "org.bluez.Error.Rejected");

    // RequestPinCode, answered with text.
    let call = rig.bluez.agent_call("RequestPinCode", (ObjectPath::try_from(PHONE).unwrap(),));
    let s = d.state("the PIN prompt", |s| prompt_of(s, "pin").is_some());
    let id = prompt_of(&s, "pin").unwrap()["id"].clone();
    w.send(json!({"id": "w-pin", "op": "answer", "prompt": id, "accept": true, "value": "1234"}));
    assert_eq!(w.reply("w-pin")["ok"], true, "the socket client can answer too");
    let m = call.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(m.body().deserialize::<String>().unwrap(), "1234");

    // AuthorizeService carries its UUID.
    let uuid = "0000110b-0000-1000-8000-00805f9b34fb";
    let call =
        rig.bluez.agent_call("AuthorizeService", (ObjectPath::try_from(HEADSET).unwrap(), uuid));
    let s = d.state("the service prompt", |s| prompt_of(s, "service").is_some());
    let p = prompt_of(&s, "service").unwrap();
    assert_eq!(p["uuid"], uuid);
    d.ask(json!({"id": "svc", "op": "answer", "prompt": p["id"], "accept": true}));
    assert_eq!(error_name(&call.recv_timeout(WAIT).unwrap()), "ok");

    // `connect` on a device that is not paired pairs it first; a pairing the
    // person rejects fails with BlueZ's own error.
    rig.bluez.phone.lock().unwrap().paired = false;
    rig.bluez.emit(
        PHONE,
        "org.bluez.Device1",
        HashMap::from([("Paired", ZValue::from(false))]),
        vec![],
    );
    d.state("the phone unpaired", |s| device(s, PHONE)["paired"] == false);
    let pairs = rig.bluez.log.count(&format!("Pair {PHONE}"));
    d.send(json!({"id": "p2", "op": "connect", "device": PHONE}));
    let s = d.state("a second confirm prompt", |s| prompt_of(s, "confirm").is_some());
    assert_eq!(rig.bluez.log.count(&format!("Pair {PHONE}")), pairs + 1, "connect paired first");
    let id = prompt_of(&s, "confirm").unwrap()["id"].clone();
    d.ask(json!({"id": "no", "op": "answer", "prompt": id, "accept": false}));
    let r = d.reply("p2");
    assert_eq!(r["op"], "connect");
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"], "org.bluez.Error.AuthenticationRejected");
    assert!(rig.bluez.log.has("RequestConfirmation answered org.bluez.Error.Rejected"));

    // Accepted, the same `connect` pairs and connects, and its reply carries
    // `connected` as `pair`'s does: ok says Pair worked, and the network popup
    // reads `connected` to know whether anything did connect.
    d.send(json!({"id": "p3", "op": "connect", "device": PHONE}));
    let s = d.state("a third confirm prompt", |s| prompt_of(s, "confirm").is_some());
    let id = prompt_of(&s, "confirm").unwrap()["id"].clone();
    d.ask(json!({"id": "yes", "op": "answer", "prompt": id, "accept": true}));
    let r = d.reply("p3");
    assert_eq!(
        (r["op"].as_str(), r["ok"].as_bool(), r["connected"].as_bool()),
        (Some("connect"), Some(true), Some(true)),
        "{r}"
    );

    // Only bluetoothd may ask: a call from another connection is refused, at
    // once. Without the time limit a stranger let through still came back
    // Rejected -- when its unanswered prompt expired, 65 s later.
    let stranger = blocking::connection::Builder::address(rig._buses.system.as_str())
        .unwrap()
        .method_timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let (sender, path) = rig.bluez.agent.lock().unwrap().clone().unwrap();
    let r = stranger.call_method(
        Some(sender.as_str()),
        path.as_str(),
        Some("org.bluez.Agent1"),
        "RequestAuthorization",
        &(ObjectPath::try_from(PHONE).unwrap(),),
    );
    assert_eq!(error_name(&r), "org.bluez.Error.Rejected");
    // A state asked for now, not the one already held from before the call.
    assert_eq!(d.ask(json!({"id": "snap-stranger", "op": "snapshot"}))["ok"], true);
    assert_eq!(d.last.as_ref().unwrap()["prompts"], json!([]), "no prompt for the stranger");

    // BlueZ releasing the agent is the agent being off.
    assert_eq!(error_name(&rig.bluez.agent_call("Release", ()).recv_timeout(WAIT).unwrap()), "ok");
    d.state("the agent released", |s| s["agent"] == "off");
}

#[test]
fn received_files_land_inside_the_receive_folder_under_a_safe_name() {
    let Some(mut rig) = Rig::start() else { return };
    let recv = rig.env.recv();
    let d = &mut rig.daemon;

    // A hostile name, asked about and accepted.
    let (transfer, call) = rig.obexd.push(1, "../../evil.txt", "AA:BB:CC:DD:EE:01");
    let s = d.state("the push prompt", |s| prompt_of(s, "push").is_some());
    let p = prompt_of(&s, "push").unwrap();
    assert_eq!(p["file"], "evil.txt");
    assert_eq!(p["size"], 1234);
    assert_eq!(p["device"], PHONE);
    assert_eq!(p["name"], "Phone");
    d.ask(json!({"id": "acc", "op": "answer", "prompt": p["id"], "accept": true}));
    let path = PathBuf::from(call.recv_timeout(WAIT).unwrap().expect("accepted"));
    assert!(path.is_absolute(), "{}", path.display());
    assert_eq!(
        path.parent(),
        Some(recv.as_path()),
        "{} is not directly inside {}",
        path.display(),
        recv.display()
    );
    assert_eq!(path.file_name().unwrap(), "evil.txt");
    assert!(path.components().all(|c| c != std::path::Component::ParentDir));
    assert!(recv.is_dir(), "the receive folder is made on first use");
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&recv).unwrap().permissions().mode() & 0o777, 0o700);
    }
    rig.obexd.emit(&transfer, HashMap::from([("Status", ZValue::from("active"))]));
    rig.obexd.emit(&transfer, HashMap::from([("Transferred", ZValue::from(600u64))]));
    let s = d.state("the incoming transfer", |s| {
        s["transfers"].as_array().is_some_and(|t| {
            t.iter().any(|t| t["id"] == transfer.as_str() && t["transferred"] == 600)
        })
    });
    let t = s["transfers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == transfer.as_str())
        .unwrap()
        .clone();
    assert_eq!(t["direction"], "in");
    assert_eq!(t["path"], path.to_str().unwrap());
    assert_eq!(t["deviceName"], "Phone");
    rig.obexd.emit(
        &transfer,
        HashMap::from([
            ("Transferred", ZValue::from(1234u64)),
            ("Status", ZValue::from("complete")),
        ]),
    );
    let n = d.wait("the received notice", |v| v["event"] == "notice" && v["kind"] == "received");
    assert_eq!(n["file"], "evil.txt");
    assert_eq!(n["path"], path.to_str().unwrap());
    assert_eq!(n["device"], PHONE);

    // Rejected: obexd is told so.
    let (_, call) = rig.obexd.push(2, "photo.jpg", "AA:BB:CC:DD:EE:01");
    let s = d.state("a second push prompt", |s| prompt_of(s, "push").is_some());
    let id = prompt_of(&s, "push").unwrap()["id"].clone();
    d.ask(json!({"id": "rej", "op": "answer", "prompt": id, "accept": false}));
    assert_eq!(call.recv_timeout(WAIT).unwrap().unwrap_err(), "org.bluez.obex.Error.Rejected");

    // Trusted devices, silently -- and a name already taken gets a number.
    std::fs::write(recv.join("notes.txt"), b"mine").unwrap();
    d.ask(json!({"id": "cfg2", "op": "config", "files": "trusted"}));
    d.state("files: trusted", |s| s["files"] == "trusted");
    let (_, call) = rig.obexd.push(3, "notes.txt", "AA:BB:CC:DD:EE:02");
    let path = PathBuf::from(
        call.recv_timeout(Duration::from_secs(3)).unwrap().expect("accepted without a prompt"),
    );
    assert_eq!(path, recv.join("notes (1).txt"));
    assert_eq!(std::fs::read(recv.join("notes.txt")).unwrap(), b"mine");
    // A name that is nothing but a path component names no file.
    let (_, call) = rig.obexd.push(5, "..", "AA:BB:CC:DD:EE:02");
    assert_eq!(
        PathBuf::from(call.recv_timeout(Duration::from_secs(3)).unwrap().unwrap()),
        recv.join("file")
    );
    // An untrusted device is still asked.
    let (_, call) = rig.obexd.push(4, "x.bin", "AA:BB:CC:DD:EE:01");
    d.state("the untrusted device is asked", |s| prompt_of(s, "push").is_some());
    let _ = rig.obexd.conn.call_method(
        Some(rig.obexd.s.agent.lock().unwrap().clone().unwrap().0.as_str()),
        "/org/jayos/bluetooth/obex",
        Some("org.bluez.obex.Agent1"),
        "Cancel",
        &(),
    );
    // obexd sends Cancel when its own call has timed out, and then ignores
    // the answer; but a Rejected is the only answer that could never save it.
    let r = call.recv_timeout(WAIT).unwrap();
    assert_eq!(obexd_writes(&r, "x.bin"), None, "{r:?}");
    d.state("the push prompt gone on Cancel", |s| s["prompts"] == json!([]));

    // Off: the agent is unregistered, so obexd refuses pushes itself.
    d.ask(json!({"id": "cfg3", "op": "config", "files": "off"}));
    assert!(
        rig.obexd.s.log.wait("UnregisterAgent /org/jayos/bluetooth/obex"),
        "{:?}",
        rig.obexd.s.log.all()
    );
    d.state("obex off", |s| s["obex"] == "off" && s["files"] == "off");
    // And back.
    d.ask(json!({"id": "cfg4", "op": "config", "files": "ask"}));
    d.state("obex ready again", |s| s["obex"] == "ready");
    assert_eq!(rig.obexd.s.log.count("RegisterAgent /org/jayos/bluetooth/obex"), 2);

    // agent:false takes both agents away.
    d.ask(json!({"id": "cfg5", "op": "config", "agent": false}));
    d.state("both agents off", |s| s["agent"] == "off" && s["obex"] == "off");
    assert!(rig.bluez.log.wait(&format!("UnregisterAgent {AGENT}")));
}

#[test]
fn files_are_sent_in_one_session_that_is_removed_afterwards() {
    let Some(mut rig) = Rig::start() else { return };
    let dir = rig.env.dir.join("out");
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.pdf"), dir.join("b.txt"));
    std::fs::write(&a, vec![7u8; 8000]).unwrap();
    std::fs::write(&b, vec![9u8; 4000]).unwrap();
    let d = &mut rig.daemon;

    // A missing file is refused before any connection is made, and said to
    // every client, not only the one that asked.
    let r =
        d.ask(json!({"id": "bad", "op": "send", "device": PHONE, "files": [a, "/nonexistent/x"]}));
    assert_eq!(r["error"], "file-unreadable");
    assert!(r["message"].as_str().unwrap().contains("/nonexistent/x"));
    assert!(!rig.obexd.s.log.all().iter().any(|l| l.starts_with("CreateSession")));
    let n = d.wait("the unreadable file's notice", |v| v["event"] == "notice");
    assert_eq!(
        (n["kind"].as_str(), n["direction"].as_str(), n["file"].as_str(), n["name"].as_str()),
        (Some("failed"), Some("out"), Some("x"), Some("Phone")),
        "{n}"
    );
    assert_eq!(n["message"], r["message"]);

    d.send(json!({"id": "s1", "op": "send", "device": PHONE, "files": [a, b]}));
    let s = d.state("a transfer in progress with a rate", |s| {
        s["transfers"].as_array().is_some_and(|t| {
            t.iter().any(|t| t["status"] == "active" && t["rate"].as_u64().is_some_and(|r| r > 0))
        })
    });
    let t = s["transfers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["status"] == "active")
        .unwrap()
        .clone();
    assert_eq!(t["direction"], "out");
    assert_eq!(t["device"], PHONE);
    assert_eq!(t["deviceName"], "Phone");
    assert_eq!(t["file"], "a.pdf");
    assert_eq!(t["size"], 8000);
    let first = d.wait("the first sent notice", |v| v["event"] == "notice" && v["kind"] == "sent");
    assert_eq!(first["file"], "a.pdf");
    let second =
        d.wait("the second sent notice", |v| v["event"] == "notice" && v["kind"] == "sent");
    assert_eq!(second["file"], "b.txt");
    let r = d.reply("s1");
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["sent"], 2);
    assert_eq!(r["transfers"].as_array().unwrap().len(), 2);
    let log = rig.obexd.s.log.all();
    assert_eq!(log.iter().filter(|l| l.starts_with("CreateSession")).count(), 1, "{log:?}");
    assert!(
        log.contains(&"CreateSession AA:BB:CC:DD:EE:01 Target=opp".to_string()),
        "one adapter: no Source: {log:?}"
    );
    let sends: Vec<&String> = log.iter().filter(|l| l.starts_with("SendFile")).collect();
    assert_eq!(
        sends,
        vec![&format!("SendFile {}", a.display()), &format!("SendFile {}", b.display())]
    );
    assert!(
        rig.obexd.s.log.wait("RemoveSession /org/bluez/obex/client/session0"),
        "{:?}",
        rig.obexd.s.log.all()
    );
    let s = d.state("both complete", |s| {
        s["transfers"]
            .as_array()
            .is_some_and(|t| t.len() == 2 && t.iter().all(|t| t["status"] == "complete"))
    });
    assert!(s["transfers"][0]["finished"].as_i64().is_some());

    // Dismissing a finished transfer.
    let id = s["transfers"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(d.ask(json!({"id": "dis", "op": "dismiss", "transfer": id}))["ok"], true);
    d.state("dismissed", |s| s["transfers"].as_array().is_some_and(|t| t.len() == 1));

    // Cancel: Transfer1.Cancel, the transfer marked cancelled, the rest of the
    // batch dropped, the session still removed.
    rig.obexd.s.slow.store(true, Ordering::SeqCst);
    d.send(json!({"id": "s2", "op": "send", "device": PHONE, "files": [a, b]}));
    let s = d.state("the slow transfer active", |s| {
        s["transfers"].as_array().is_some_and(|t| t.iter().any(|t| t["status"] == "active"))
    });
    let tid = s["transfers"].as_array().unwrap().iter().find(|t| t["status"] == "active").unwrap()
        ["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = d.ask(json!({"id": "c", "op": "cancel", "transfer": tid}));
    assert_eq!(r["ok"], true, "{r}");
    assert!(rig.obexd.s.log.has(&format!("Cancel {tid}")));
    let r = d.reply("s2");
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"], "org.bluez.obex.Error.Canceled");
    assert_eq!(r["sent"], 0);
    d.state("cancelled, not error", |s| {
        s["transfers"].as_array().is_some_and(|t| {
            t.iter().any(|t| t["id"] == tid.as_str() && t["status"] == "cancelled")
        })
    });
    assert!(
        rig.obexd.s.log.wait("RemoveSession /org/bluez/obex/client/session3"),
        "{:?}",
        rig.obexd.s.log.all()
    );
    assert_eq!(
        rig.obexd.s.log.all().iter().filter(|l| l.starts_with("SendFile")).count(),
        3,
        "b.txt was dropped"
    );
    assert!(
        !d.backlog.iter().any(|v| v["event"] == "notice" && v["kind"] == "failed"),
        "a cancel is not a failure"
    );

    // pick, with no portal on this session bus: a notice too.
    let r = d.ask(json!({"id": "pk", "op": "pick", "device": PHONE}));
    assert_eq!(r["error"], "portal-unavailable");
    let n = d.wait("the no-portal notice", |v| v["event"] == "notice" && v["kind"] == "failed");
    assert_eq!((n["direction"].as_str(), n["device"].as_str()), (Some("out"), Some(PHONE)));

    // pick, with one: a cancelled chooser, then two files chosen -- one with a
    // space, which arrives percent-encoded -- sent like `send`.
    let portal_log = Log::default();
    let answer = Arc::new(Mutex::new((1u32, Vec::<String>::new())));
    let _portal = blocking::connection::Builder::address(rig._buses.session.as_str())
        .unwrap()
        .name("org.freedesktop.portal.Desktop")
        .unwrap()
        .serve_at(
            "/org/freedesktop/portal/desktop",
            MockPortal { log: portal_log.clone(), answer: answer.clone(), forger: None },
        )
        .unwrap()
        .build()
        .unwrap();
    let r = d.ask(json!({"id": "pk2", "op": "pick", "device": PHONE}));
    assert_eq!((r["ok"].as_bool(), r["cancelled"].as_bool()), (Some(true), Some(true)), "{r}");
    assert!(portal_log.has("OpenFile Send to Phone multiple=true"), "{:?}", portal_log.all());
    // The person closing the chooser is not a failure. Anything said about it
    // would have come before this snapshot's reply.
    d.ask(json!({"id": "after-pk2", "op": "snapshot"}));
    assert!(
        !d.backlog.iter().any(|v| v["event"] == "notice"),
        "a chooser closed by the person is not news: {:?}",
        d.backlog
    );
    let spaced = dir.join("c d.txt");
    std::fs::write(&spaced, b"spaced").unwrap();
    *answer.lock().unwrap() = (
        0,
        vec![format!("file://{}", dir.join("c%20d.txt").display()), "https://example.com/x".into()],
    );
    let r = d.ask(json!({"id": "pk3", "op": "pick", "device": PHONE}));
    assert_eq!(
        (r["op"].as_str(), r["ok"].as_bool(), r["sent"].as_u64()),
        (Some("pick"), Some(true), Some(1)),
        "{r}"
    );
    assert!(
        rig.obexd.s.log.has(&format!("SendFile {}", spaced.display())),
        "{:?}",
        rig.obexd.s.log.all()
    );

    // The send verb, from a terminal.
    let out =
        rig.env.command(&["send", "aa:bb:cc:dd:ee:01", a.to_str().unwrap()]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let last = lines.last().unwrap();
    assert_eq!((last["event"].as_str(), last["ok"].as_bool()), (Some("reply"), Some(true)));
    assert!(
        lines.iter().any(|l| l["direction"] == "out" && l["file"] == "a.pdf"),
        "progress lines: {text}"
    );
}

#[test]
fn scan_interest_is_released_when_its_client_disconnects() {
    let Some(mut rig) = Rig::start() else { return };
    let d = &mut rig.daemon;
    let log = &rig.bluez.log;

    let r = d.ask(json!({"id": "s1", "op": "scan", "adapter": ADAPTER, "on": true}));
    assert_eq!(r["ok"], true);
    assert_eq!(log.count("StartDiscovery"), 1);
    d.state("scanning", |s| s["adapters"][0]["scanning"] == true);

    let mut w = Proc::spawn(&rig.env, "watch");
    w.state("watch connected", |s| s["bluez"] == true);
    w.send(json!({"id": "w1", "op": "scan", "adapter": ADAPTER, "on": true}));
    assert_eq!(w.reply("w1")["ok"], true);
    assert_eq!(log.count("StartDiscovery"), 1, "one discovery for two interested clients");
    drop(w);
    d.state("the watch client gone", |s| s["adapters"][0]["scanning"] == true);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(log.count("StopDiscovery"), 0, "the owner still wants it");

    // The adapter going off and on again with interest outstanding: the
    // discovery BlueZ dropped is asked for again.
    assert_eq!(
        d.ask(json!({"id": "off", "op": "power", "adapter": ADAPTER, "on": false}))["ok"],
        true
    );
    d.state("powered off", |s| s["adapters"][0]["powered"] == false);
    assert_eq!(
        d.ask(json!({"id": "on", "op": "power", "adapter": ADAPTER, "on": true}))["ok"],
        true
    );
    assert!(
        {
            let deadline = Instant::now() + WAIT;
            while log.count("StartDiscovery") < 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            log.count("StartDiscovery") == 2
        },
        "{:?}",
        log.all()
    );

    assert_eq!(
        d.ask(json!({"id": "s2", "op": "scan", "adapter": ADAPTER, "on": false}))["ok"],
        true
    );
    assert!(log.wait("StopDiscovery"));
    d.state("not scanning", |s| s["adapters"][0]["scanning"] == false);

    // A client that leaves without saying so takes its interest with it.
    let mut w = Proc::spawn(&rig.env, "watch");
    w.state("watch connected", |s| s["bluez"] == true);
    w.send(json!({"id": "w2", "op": "scan", "adapter": ADAPTER, "on": true}));
    assert_eq!(w.reply("w2")["ok"], true);
    assert_eq!(log.count("StartDiscovery"), 3);
    drop(w);
    let deadline = Instant::now() + WAIT;
    while log.count("StopDiscovery") < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(log.count("StopDiscovery"), 2, "{:?}", log.all());
}

#[test]
fn without_bluez_or_obexd_the_state_says_absent() {
    let Some(buses) = Buses::start() else { return };
    let env = Env::of(&buses);
    let mut d = Proc::spawn(&env, "daemon");
    let s = d.state("absent", |s| s["bluez"] == false);
    for k in [
        "version",
        "bluez",
        "agent",
        "agentError",
        "obex",
        "obexError",
        "files",
        "receiveDir",
        "adapters",
        "devices",
        "transfers",
        "prompts",
    ] {
        assert!(s.get(k).is_some(), "{k} missing from {s}");
    }
    assert_eq!(s["agent"], "absent");
    assert_eq!(s["obex"], "absent");
    assert_eq!(s["adapters"], json!([]));
    let r = d.ask(json!({"id": "p", "op": "power", "adapter": ADAPTER, "on": true}));
    assert_eq!(r["error"], "bluez-absent");
    let r = d.ask(json!({"id": "s", "op": "send", "device": PHONE, "files": ["/etc/hostname"]}));
    assert_eq!(r["error"], "bluez-absent");

    // `status` over the socket answers with the same state.
    let out = env.command(&["status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        (v["event"].as_str(), v["bluez"].as_bool(), v["obex"].as_str()),
        (Some("state"), Some(false), Some("absent"))
    );

    // bluetoothd starting later is noticed, and so is it stopping -- with
    // whatever it was asking dropped, not left on screen.
    let bluez = Bluez::start(&buses.system);
    d.state("bluez arrived", |s| {
        s["bluez"] == true
            && s["agent"] == "ready"
            && s["devices"].as_array().is_some_and(|d| d.len() == 3)
    });
    let call = bluez.agent_call("RequestAuthorization", (ObjectPath::try_from(PHONE).unwrap(),));
    d.state("a prompt from bluez", |s| prompt_of(s, "authorize").is_some());
    // Releasing the name is what bluetoothd exiting looks like on the bus;
    // dropping the connection would not do, since the call above holds it.
    bluez.conn.release_name("org.bluez").unwrap();
    let s = d.state("bluez gone", |s| s["bluez"] == false);
    assert_eq!(s["agent"], "absent");
    assert_eq!(s["devices"], json!([]));
    assert_eq!(s["prompts"], json!([]));
    assert_eq!(error_name(&call.recv_timeout(WAIT).unwrap()), "org.bluez.Error.Canceled");
}

#[test]
fn stdin_eof_ends_the_daemon_and_a_second_daemon_is_fatal() {
    // No buses are needed for either, so this runs without dbus-daemon too.
    let dir = scratch();
    let env = Env::unreachable(&dir);
    let mut first = Proc::spawn(&env, "daemon");
    let s = first.state("a state with no buses", |s| s["bluez"] == false);
    assert_eq!(s["obex"], "absent");
    let socket = dir.join("run/quickshell/bluetooth/control.sock");
    assert!(socket.exists());

    // The second waits out the grace for the first to go, and only then
    // gives up: no sooner, or a reload would still lose the race, and with
    // nothing on stdout before the fatal line.
    let started = Instant::now();
    let mut second = Proc::spawn(&env, "daemon");
    let v = second.wait_within("the fatal line", LOCK_GRACE + WAIT, |v| v["event"] == "fatal");
    let waited = started.elapsed();
    assert_eq!(v["message"], "another jayos-bluetooth daemon is running");
    assert!(waited >= LOCK_GRACE, "fatal after {waited:?}, inside the {LOCK_GRACE:?} grace");
    assert!(second.last.is_none() && second.backlog.is_empty(), "{:?}", second.backlog);
    assert_eq!(second.exit(WAIT), Some(1));

    first.close_stdin();
    assert_eq!(first.exit(WAIT), Some(0), "stdin EOF is the shell going away");
    assert!(!socket.exists(), "the socket goes with the daemon");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_daemon_started_while_another_exits_takes_over() {
    // A shell reload: the new generation's daemon starts while the old one's
    // is still running, and the old one goes a moment later -- by stdin EOF
    // here, then by SIGKILL, which is how Quickshell ends a Process's child
    // and leaves the socket file behind. No buses are needed.
    let dir = scratch();
    let env = Env::unreachable(&dir);
    let socket = dir.join("run/quickshell/bluetooth/control.sock");
    let status = || {
        let out = env.command(&["status"]).stdin(Stdio::null()).output().unwrap();
        (out.status.code(), serde_json::from_slice::<Value>(&out.stdout).ok())
    };

    let mut a = Proc::spawn(&env, "daemon");
    a.state("the first daemon's state", |s| s["bluez"] == false);
    let mut b = Proc::spawn(&env, "daemon");
    // Long enough for B to have found the lock taken, so the handover below
    // is from a live holder rather than to a B that had not yet looked.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(b.child.try_wait().unwrap(), None, "B gave up while A held the lock");
    assert!(b.events.try_recv().is_err(), "B spoke before it held the lock");
    a.close_stdin();
    assert_eq!(a.exit(WAIT), Some(0));
    let v = b.wait("B's first event", |v| v["event"] == "state" || v["event"] == "fatal");
    assert_eq!(v["event"], "state", "B took over: {v}");
    let (code, v) = status();
    assert_eq!((code, v.as_ref().map(|v| v["event"].clone())), (Some(0), Some(json!("state"))));

    let mut c = Proc::spawn(&env, "daemon");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(c.child.try_wait().unwrap(), None, "C gave up while B held the lock");
    b.child.kill().unwrap();
    b.child.wait().unwrap();
    assert!(socket.exists(), "a killed daemon leaves its socket file");
    let v = c.wait("C's first event", |v| v["event"] == "state" || v["event"] == "fatal");
    assert_eq!(v["event"], "state", "C took over from a killed daemon: {v}");
    let (code, v) = status();
    assert_eq!((code, v.as_ref().map(|v| v["event"].clone())), (Some(0), Some(json!("state"))));

    c.close_stdin();
    assert_eq!(c.exit(WAIT), Some(0));
    assert!(!socket.exists(), "the socket goes with the last daemon");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clients_without_a_daemon_exit_2_with_an_envelope() {
    let dir = scratch();
    let env = Env::unreachable(&dir);
    for args in [vec!["status"], vec!["watch"], vec!["send", "AA:BB:CC:DD:EE:FF", "/etc/hostname"]]
    {
        let out = env.command(&args).stdin(Stdio::null()).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let v: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{args:?}: {}", String::from_utf8_lossy(&out.stdout)));
        assert_eq!(v["ok"], false, "{args:?}");
        assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()), "{args:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ── what went wrong once ────────────────────────────────────────────────────

/// Small files in `<dir>/<sub>`, as absolute paths.
fn files_in(dir: &Path, sub: &str, names: &[&str]) -> Vec<String> {
    let dir = dir.join(sub);
    std::fs::create_dir_all(&dir).unwrap();
    names
        .iter()
        .map(|n| {
            let p = dir.join(n);
            std::fs::write(&p, vec![b'x'; 2000]).unwrap();
            p.to_str().unwrap().to_string()
        })
        .collect()
}

/// Sends repeated until the order obexd ends a transfer in -- complete, then
/// removed, back to back from one thread -- has had every chance to reach the
/// core reversed.
#[test]
fn a_transfer_that_completes_and_is_removed_is_sent_every_time() {
    // Measured on the pump this replaced, which read each subscription on a
    // thread of its own: all 10 runs failed, 8 with 16 to 34 of the 300 files
    // reported failed and 2 hung waiting for a reply. At the lowest of those
    // rates, about 1 in 20, a clean run of 300 is a chance of about 2 in a
    // million (0.95^300), and the test takes a little over a second; 20 files
    // a send keeps CreateSession out of most of that time.
    const ROUNDS: usize = 15;
    const FILES: usize = 20;
    let Some(mut rig) = Rig::start() else { return };
    let names: Vec<String> = (0..FILES).map(|i| format!("f{i}.txt")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let files = files_in(&rig.env.dir, "many", &names);
    rig.obexd.s.fast.store(true, Ordering::SeqCst);
    let d = &mut rig.daemon;
    let mut failed = Vec::new();
    for round in 0..ROUNDS {
        let r = d
            .ask(json!({"id": format!("r{round}"), "op": "send", "device": PHONE, "files": files}));
        if r["ok"] != true || r["sent"] != FILES {
            failed.push(r);
        }
    }
    let notices: Vec<&Value> =
        d.backlog.iter().filter(|v| v["event"] == "notice" && v["kind"] == "failed").collect();
    assert!(
        failed.is_empty() && notices.is_empty(),
        "{} of {ROUNDS} sends reported a delivered file as failed: {failed:?} {notices:?}",
        failed.len()
    );
    let removed = rig.obexd.s.log.all().iter().filter(|l| l.starts_with("Complete ")).count();
    assert_eq!(removed, ROUNDS * FILES, "every transfer ended as obexd ends one");
}

/// With Incoming files off, `obex` reads "off" and sending still works. obexd
/// stopping must end a send then too: it sends no InterfacesRemoved as it
/// dies, so nothing else ever would.
#[test]
fn a_send_ends_when_obexd_stops_even_with_incoming_files_off() {
    let Some(mut rig) = Rig::start() else { return };
    let files = files_in(&rig.env.dir, "out", &["slow.bin"]);
    let d = &mut rig.daemon;
    d.ask(json!({"id": "off", "op": "config", "files": "off"}));
    d.state("incoming files off", |s| s["obex"] == "off");
    rig.obexd.s.slow.store(true, Ordering::SeqCst);
    d.send(json!({"id": "s1", "op": "send", "device": PHONE, "files": files}));
    let s = d.state("the send under way", |s| {
        s["transfers"].as_array().is_some_and(|t| t.iter().any(|t| t["status"] == "active"))
    });
    let tid = s["transfers"][0]["id"].as_str().unwrap().to_string();
    rig.obexd.conn.release_name("org.bluez.obex").unwrap();
    let r = d.reply("s1");
    assert_eq!(r["ok"], false, "{r}");
    assert!(r["message"].as_str().unwrap().contains("obexd stopped"), "{r}");
    d.state("the row ended", |s| {
        s["transfers"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["id"] == tid.as_str() && t["status"] == "error"))
    });
}

/// With two adapters a send names the one its device is under as `Source`,
/// or obexd sends out of whichever it likes; with one it names none. The
/// choice is model::send_source's, unit-tested there; this holds that it
/// reaches CreateSession.
#[test]
fn a_send_on_a_machine_with_two_adapters_names_the_devices_own() {
    let Some(mut rig) = Rig::start() else { return };
    let files = files_in(&rig.env.dir, "out", &["one.txt"]);
    let d = &mut rig.daemon;
    let r = d.ask(json!({"id": "s0", "op": "send", "device": PHONE, "files": files}));
    assert_eq!(r["ok"], true, "{r}");
    let server = rig.bluez.conn.object_server();
    server
        .at(
            "/org/bluez/hci1",
            MockAdapter {
                log: rig.bluez.log.clone(),
                address: "00:11:22:33:44:66".into(),
                alias: "dongle".into(),
                powered: true,
                discoverable: false,
                discoverable_timeout: 180,
                pairable: true,
            },
        )
        .unwrap();
    let far = "/org/bluez/hci1/dev_AA_BB_CC_DD_EE_07";
    server
        .at(
            far,
            MockFarDevice {
                address: "AA:BB:CC:DD:EE:07".into(),
                adapter: "/org/bluez/hci1".into(),
            },
        )
        .unwrap();
    d.state("two adapters, and the far phone", |s| {
        s["adapters"].as_array().is_some_and(|a| a.len() == 2)
            && s["devices"].as_array().is_some_and(|v| v.iter().any(|x| x["path"] == far))
    });
    // hci1 arrived powered with nobody asking to scan: no discovery for it.
    // A `power` goes through the same bus worker as StartDiscovery, so its
    // reply comes after any StartDiscovery the arrival queued.
    let p = d.ask(json!({"id": "p1", "op": "power", "adapter": "/org/bluez/hci1", "on": true}));
    assert_eq!(p["ok"], true, "{p}");
    assert_eq!(
        rig.bluez.log.count("StartDiscovery"),
        0,
        "an adapter nobody asked to scan: {:?}",
        rig.bluez.log.all()
    );
    let r = d.ask(json!({"id": "s1", "op": "send", "device": far, "files": files}));
    assert_eq!(r["ok"], true, "{r}");
    let r = d.ask(json!({"id": "s2", "op": "send", "device": PHONE, "files": files}));
    assert_eq!(r["ok"], true, "{r}");
    let log: Vec<String> =
        rig.obexd.s.log.all().into_iter().filter(|l| l.starts_with("CreateSession")).collect();
    assert_eq!(
        log,
        [
            "CreateSession AA:BB:CC:DD:EE:01 Target=opp",
            "CreateSession AA:BB:CC:DD:EE:07 Source=00:11:22:33:44:66 Target=opp",
            "CreateSession AA:BB:CC:DD:EE:01 Source=00:11:22:33:44:55 Target=opp",
        ],
        "one adapter: no Source; two: each device's own"
    );
}

/// obexd leaving between finishing a transfer and the core hearing SendFile's
/// reply: the core has failed every transfer it knew by then, and this one it
/// did not know yet. It completed, and the obexd that sent it said so before
/// it went, so the send is a success -- not a hang, which it was while the
/// graveyard was cleared as obexd left.
#[test]
fn a_transfer_its_obexd_completed_before_leaving_is_sent() {
    let Some(mut rig) = Rig::start() else { return };
    let files = files_in(&rig.env.dir, "out", &["last.bin"]);
    rig.obexd.s.leaves.store(1, Ordering::SeqCst);
    let d = &mut rig.daemon;
    d.send(json!({"id": "s1", "op": "send", "device": PHONE, "files": files}));
    let r = d.reply("s1");
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["sent"], 1, "{r}");
}

/// The same, part-way through: nothing will ever end that transfer, so the
/// send ends now, as stopped -- not waiting for ever.
#[test]
fn a_transfer_whose_obexd_left_before_answering_fails_the_send() {
    let Some(mut rig) = Rig::start() else { return };
    let files = files_in(&rig.env.dir, "out", &["half.bin"]);
    rig.obexd.s.leaves.store(2, Ordering::SeqCst);
    let d = &mut rig.daemon;
    d.send(json!({"id": "s1", "op": "send", "device": PHONE, "files": files}));
    let r = d.reply("s1");
    assert_eq!(r["ok"], false, "{r}");
    assert!(r["message"].as_str().unwrap().contains("obexd stopped"), "{r}");
}

/// obexd numbers its transfers from 0 in every process, so a restarted one
/// hands out the paths the last one did. Each new obexd here registers its
/// transfer late, so SendFile's reply reaches the core while the path is
/// known only from before the restart; the send must not be called done
/// before the mock has finished it.
#[test]
fn a_restarted_obexd_does_not_inherit_the_last_ones_transfers() {
    let Some(mut rig) = Rig::start() else { return };
    let files = files_in(&rig.env.dir, "out", &["a.txt"]);
    rig.obexd.s.fast.store(true, Ordering::SeqCst);
    let session = rig._buses.session.clone();
    let d = &mut rig.daemon;
    let r = d.ask(json!({"id": "first", "op": "send", "device": PHONE, "files": files}));
    assert_eq!(r["ok"], true, "{r}");
    let path = r["transfers"][0].as_str().unwrap().to_string();

    // A restart as obexd does one: the name unowned in between, since obexd
    // asks for it with DO_NOT_QUEUE (gdbus/mainloop.c:325-326).
    rig.obexd.conn.release_name("org.bluez.obex").unwrap();
    d.state("obexd gone", |s| s["obex"] != "ready");
    let second = Obexd::start(&session);
    second.s.fast.store(true, Ordering::SeqCst);
    second.s.late_ms.store(400, Ordering::SeqCst);
    d.state("the new obexd taken on", |s| s["obex"] == "ready");
    let r = d.ask(json!({"id": "second", "op": "send", "device": PHONE, "files": files}));
    assert_eq!(r["transfers"][0], path.as_str(), "the same path again: {r}");
    assert!(
        second.s.log.has(&format!("Complete {path}")),
        "reported {r} while obexd had not finished it: {:?}",
        second.s.log.all()
    );
    assert_eq!(r["ok"], true, "{r}");

    // And handed straight from one process to the next, which obexd cannot
    // do but anything else owning the name can: still a different obexd,
    // with none of the last one's transfers and no agent until it is given
    // one.
    let third = Obexd::start(&session);
    third.s.fast.store(true, Ordering::SeqCst);
    third.s.late_ms.store(400, Ordering::SeqCst);
    second.conn.release_name("org.bluez.obex").unwrap();
    assert!(
        third.s.log.wait("RegisterAgent /org/jayos/bluetooth/obex"),
        "the new owner was not given the agent: {:?}",
        third.s.log.all()
    );
    d.state("the third obexd taken on", |s| s["obex"] == "ready");
    let r = d.ask(json!({"id": "third", "op": "send", "device": PHONE, "files": files}));
    assert_eq!(r["transfers"][0], path.as_str(), "{r}");
    assert!(
        third.s.log.has(&format!("Complete {path}")),
        "reported {r} while obexd had not finished it: {:?}",
        third.s.log.all()
    );
    assert_eq!(r["ok"], true, "{r}");
}

/// A socket client that stops reading is dropped, but its reader thread runs
/// on until the writer has drained and shut the socket. A `scan on` it sends
/// in that time must not take discovery interest that nothing will release.
#[test]
fn a_client_dropped_for_not_reading_cannot_take_discovery() {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    let Some(mut rig) = Rig::start() else { return };
    let socket = rig.env.dir.join("run/quickshell/bluetooth/control.sock");
    let mut c = UnixStream::connect(&socket).unwrap();
    // A state and a reply each, against a queue of 256 and a socket buffer
    // far smaller than 800 states: dropped well inside the second waited.
    let burst: String = (0..800)
        .map(|i| format!("{}\n", json!({"id": format!("n{i}"), "op": "snapshot"})))
        .collect();
    c.write_all(burst.as_bytes()).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let scan = json!({"id": "late", "op": "scan", "adapter": ADAPTER, "on": true});
    c.write_all(format!("{scan}\n").as_bytes()).unwrap();
    // Time for the reader to post it before the writer can drain anything,
    // since nothing is read until after this.
    std::thread::sleep(Duration::from_millis(200));
    c.set_read_timeout(Some(WAIT)).unwrap();
    let mut seen = Vec::new();
    // EOF, not a timeout: the daemon closed this client, which a client it
    // still served would never see.
    c.read_to_end(&mut seen).expect("the dropped client's socket closed");
    drop(c);

    // The owner is now the only client that has asked for discovery, and its
    // letting go must stop it.
    let d = &mut rig.daemon;
    assert_eq!(
        d.ask(json!({"id": "on", "op": "scan", "adapter": ADAPTER, "on": true}))["ok"],
        true
    );
    assert_eq!(
        d.ask(json!({"id": "off", "op": "scan", "adapter": ADAPTER, "on": false}))["ok"],
        true
    );
    assert!(
        rig.bluez.log.wait("StopDiscovery"),
        "discovery left running after every client let go: {:?}",
        rig.bluez.log.all()
    );
    d.state("not scanning", |s| s["adapters"][0]["scanning"] == false);
}

type Tree = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// How a hand-driven bluetoothd answers GetManagedObjects.
#[derive(Clone, Default)]
struct Raw {
    /// Objects nobody shows, so the daemon is still reading the answer when
    /// whatever follows it arrives.
    padding: usize,
    /// How long it thinks before answering.
    delay: Duration,
    /// A device added the moment the answer is sent, on the same connection,
    /// so strictly after it on the wire: the order gdbus keeps.
    late_device: Option<String>,
}

/// A bluetoothd driven by hand rather than by zbus's object server, so that
/// what it sends after an answer can be placed exactly. It logs
/// `GetManagedObjects` when asked and `answered` once it has.
///
/// It listens before it takes the name. zbus hands a MessageIterator only
/// what arrives after it exists, and the name was taken while the connection
/// was built, with the iterator made after: the daemon, which asks for the
/// tree the moment the name has an owner, could ask in between, and its
/// GetManagedObjects was never seen and never answered. On a loaded CPU that
/// was 2 runs in 3 of `a_device_bluez_adds_just_after_answering_the_fetch_is_kept`,
/// the daemon holding `bluez:false` with its fetch waiting out the call's
/// 120 s (found by `check_rust_tests`, 2026-11-07).
fn raw_bluez(addr: &str, how: Raw) -> (blocking::Connection, Log) {
    let conn = blocking::connection::Builder::address(addr).unwrap().build().unwrap();
    let c = conn.clone();
    let messages = blocking::MessageIterator::from(&c);
    conn.request_name("org.bluez").unwrap();
    let log = Log::default();
    let said = log.clone();
    std::thread::spawn(move || {
        let text = |v: &str| OwnedValue::try_from(ZValue::from(v)).unwrap();
        for msg in messages {
            let Ok(msg) = msg else { return };
            if msg.message_type() != zbus::message::Type::MethodCall {
                continue;
            }
            let hdr = msg.header();
            if hdr.member().map(|m| m.as_str()) != Some("GetManagedObjects") {
                // RegisterAgent, RequestDefaultAgent, StartDiscovery: yes to
                // everything, and written down for the tests that count them.
                if let Some(m) = hdr.member() {
                    said.push(m.as_str());
                }
                let _ = c.reply(&hdr, &());
                continue;
            }
            said.push("GetManagedObjects");
            std::thread::sleep(how.delay);
            let mut tree: Tree = HashMap::new();
            tree.insert(
                OwnedObjectPath::try_from(ADAPTER).unwrap(),
                HashMap::from([(
                    "org.bluez.Adapter1".to_string(),
                    HashMap::from([
                        ("Address".to_string(), text("00:11:22:33:44:55")),
                        ("Alias".to_string(), text("laptop")),
                        ("Powered".to_string(), OwnedValue::from(true)),
                    ]),
                )]),
            );
            for i in 0..how.padding {
                let props = (0..8).map(|k| (format!("P{k}"), text("padding, padding"))).collect();
                tree.insert(
                    OwnedObjectPath::try_from(format!("/org/bluez/hci0/pad{i}")).unwrap(),
                    HashMap::from([("org.jayos.Padding".to_string(), props)]),
                );
            }
            if c.reply(&hdr, &tree).is_err() {
                return;
            }
            said.push("answered");
            let Some(late) = &how.late_device else { continue };
            let device: HashMap<&str, HashMap<&str, ZValue>> = HashMap::from([(
                "org.bluez.Device1",
                HashMap::from([
                    ("Address", ZValue::from("AA:BB:CC:DD:EE:09")),
                    ("Alias", ZValue::from("Late")),
                    ("Adapter", ZValue::from(ObjectPath::try_from(ADAPTER).unwrap())),
                ]),
            )]);
            let _ = c.emit_signal(
                None::<&str>,
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesAdded",
                &(ObjectPath::try_from(late.as_str()).unwrap(), device),
            );
        }
    });
    (conn, log)
}

/// A change BlueZ makes just after answering the daemon's GetManagedObjects
/// reaches the core before the answer does, which is still being read on
/// another thread. It must be kept and applied to the snapshot, not dropped:
/// a device dropped this way was missing until bluetoothd next restarted.
/// Each trial is a bluetoothd arriving, the path every restart takes.
#[test]
fn a_device_bluez_adds_just_after_answering_the_fetch_is_kept() {
    const TRIALS: usize = 3;
    let Some(buses) = Buses::start() else { return };
    let env = Env::of(&buses);
    let late = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_09";
    let mut d = Proc::spawn(&env, "daemon");
    d.state("no bluetoothd yet", |s| s["bluez"] == false);
    let how = Raw { padding: 1000, late_device: Some(late.into()), ..Raw::default() };
    for trial in 0..TRIALS {
        let (bluez, _) = raw_bluez(&buses.system, how.clone());
        d.wait_within(
            &format!("trial {trial}: the device added after the snapshot"),
            Duration::from_secs(5),
            |v| {
                v["event"] == "state"
                    && v["bluez"] == true
                    && v["devices"].as_array().is_some_and(|d| d.iter().any(|x| x["path"] == late))
            },
        );
        bluez.release_name("org.bluez").unwrap();
        d.state("bluetoothd gone", |s| s["bluez"] == false);
    }
}

/// A fetch answered after its bluetoothd has gone describes nothing. Applied,
/// it put back adapters and devices that were not there, and `bluez:true`.
#[test]
fn an_answer_from_a_bluetoothd_that_has_left_is_not_applied() {
    let Some(buses) = Buses::start() else { return };
    let env = Env::of(&buses);
    let mut d = Proc::spawn(&env, "daemon");
    d.state("no bluetoothd yet", |s| s["bluez"] == false);
    let how = Raw { delay: Duration::from_millis(500), ..Raw::default() };
    let (bluez, log) = raw_bluez(&buses.system, how);
    assert!(log.wait("GetManagedObjects"), "never asked: {:?}", log.all());
    bluez.release_name("org.bluez").unwrap();
    assert!(log.wait("answered"), "{:?}", log.all());
    // The late answer is applied, if it is, as soon as it is read: a second
    // is long enough to see it, and the state must say no bluetoothd
    // throughout.
    let until = Instant::now() + Duration::from_secs(1);
    while let Ok(v) = d.events.recv_timeout(until.saturating_duration_since(Instant::now())) {
        assert!(
            !(v["event"] == "state" && v["bluez"] == true),
            "a bluetoothd that had left came back: {v}"
        );
    }
}

/// An `off` sent while bluetoothd is away is honoured. Refused, it left the
/// client's interest behind, and discovery started again for a client that
/// had stopped looking as soon as bluetoothd came back: the network popup,
/// moved to its Wi-Fi tab by the same restart.
#[test]
fn a_scan_given_up_while_bluetoothd_is_away_stays_given_up() {
    let Some(buses) = Buses::start() else { return };
    let env = Env::of(&buses);
    let mut d = Proc::spawn(&env, "daemon");
    let (bluez, log) = raw_bluez(&buses.system, Raw::default());
    d.state("bluetoothd", |s| s["bluez"] == true);
    let mut w = Proc::spawn(&env, "watch");
    w.state("watch connected", |s| s["bluez"] == true);
    w.send(json!({"id": "on", "op": "scan", "adapter": ADAPTER, "on": true}));
    assert_eq!(w.reply("on")["ok"], true);
    assert!(log.wait("StartDiscovery"), "{:?}", log.all());

    bluez.release_name("org.bluez").unwrap();
    w.state("bluetoothd gone", |s| s["bluez"] == false);
    w.send(json!({"id": "off", "op": "scan", "adapter": ADAPTER, "on": false}));
    assert_eq!(w.reply("off")["ok"], true, "giving interest back needs no bluetoothd");

    let (_back, again) = raw_bluez(&buses.system, Raw::default());
    w.state("bluetoothd back", |s| s["bluez"] == true);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(again.count("StartDiscovery"), 0, "{:?}", again.all());
}

/// An adapter that comes back already powered -- re-enumerated after a
/// resume, a dongle plugged in again -- is scanned for whoever kept asking.
/// It sends no Powered change, so only its arrival can start discovery. The
/// client this stands for is the settings page, which keeps its interest
/// through the adapter's absence and does not ask again; the popup gives its
/// scan back when the adapter goes and asks afresh when it returns.
#[test]
fn an_adapter_that_returns_powered_is_scanned_for_again() {
    let Some(buses) = Buses::start() else { return };
    let env = Env::of(&buses);
    let mut d = Proc::spawn(&env, "daemon");
    let (bluez, log) = raw_bluez(&buses.system, Raw::default());
    d.state("bluetoothd", |s| s["bluez"] == true);
    let mut w = Proc::spawn(&env, "watch");
    w.state("watch connected", |s| s["bluez"] == true);
    w.send(json!({"id": "on", "op": "scan", "adapter": ADAPTER, "on": true}));
    assert_eq!(w.reply("on")["ok"], true);
    assert!(log.wait("StartDiscovery"), "{:?}", log.all());

    bluez
        .emit_signal(
            None::<&str>,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesRemoved",
            &(ObjectPath::try_from(ADAPTER).unwrap(), vec!["org.bluez.Adapter1"]),
        )
        .unwrap();
    w.state("the adapter gone", |s| s["adapters"].as_array().is_some_and(|a| a.is_empty()));

    let adapter: HashMap<&str, HashMap<&str, ZValue>> = HashMap::from([(
        "org.bluez.Adapter1",
        HashMap::from([
            ("Address", ZValue::from("00:11:22:33:44:55")),
            ("Alias", ZValue::from("laptop")),
            ("Powered", ZValue::from(true)),
        ]),
    )]);
    bluez
        .emit_signal(
            None::<&str>,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            &(ObjectPath::try_from(ADAPTER).unwrap(), adapter),
        )
        .unwrap();
    w.state("the adapter back", |s| s["adapters"][0]["powered"] == true);
    let deadline = Instant::now() + WAIT;
    while log.count("StartDiscovery") < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(log.count("StartDiscovery"), 2, "{:?}", log.all());
}

/// A graceful stop's departures reach the clients. bluetoothd disconnects its
/// devices and powers its adapters off while it still owns the name, then
/// leaves; inside one throttle interval, the change was folded into the state
/// saying there is no BlueZ, and the island never said the device had gone.
#[test]
fn what_bluetoothd_changes_before_it_leaves_is_sent_before_it_is_gone() {
    let Some(buses) = Buses::start() else { return };
    let env = Env::of(&buses);
    let mut d = Proc::spawn(&env, "daemon");
    let (bluez, _) = raw_bluez(&buses.system, Raw::default());
    d.state("bluetoothd", |s| s["bluez"] == true);
    let dev = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_07";
    let device: HashMap<&str, HashMap<&str, ZValue>> = HashMap::from([(
        "org.bluez.Device1",
        HashMap::from([
            ("Address", ZValue::from("AA:BB:CC:DD:EE:07")),
            ("Alias", ZValue::from("Buds")),
            ("Adapter", ZValue::from(ObjectPath::try_from(ADAPTER).unwrap())),
            ("Connected", ZValue::from(true)),
        ]),
    )]);
    bluez
        .emit_signal(
            None::<&str>,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            &(ObjectPath::try_from(dev).unwrap(), device),
        )
        .unwrap();
    d.state("the device connected", |s| {
        s["devices"]
            .as_array()
            .is_some_and(|v| v.iter().any(|x| x["path"] == dev && x["connected"] == true))
    });
    // Within the interval the state above opened: folded, unless flushed.
    let gone: HashMap<&str, ZValue> = HashMap::from([("Connected", ZValue::from(false))]);
    bluez
        .emit_signal(
            None::<&str>,
            dev,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.Device1", gone, Vec::<&str>::new()),
        )
        .unwrap();
    bluez.release_name("org.bluez").unwrap();
    let mut said_gone = false;
    let until = Instant::now() + WAIT;
    while let Ok(v) = d.events.recv_timeout(until.saturating_duration_since(Instant::now())) {
        if v["event"] != "state" {
            continue;
        }
        if v["bluez"] == false {
            break;
        }
        if v["devices"]
            .as_array()
            .is_some_and(|x| x.iter().any(|x| x["path"] == dev && x["connected"] == false))
        {
            said_gone = true;
        }
    }
    assert!(said_gone, "no state said the device had disconnected before bluetoothd was gone");
}

/// Every way the daemon says no to a push reaches obexd as
/// org.bluez.obex.Error.Rejected, the only answer obexd does not take as a
/// yes; and a push accepted on its card into a folder that cannot be used is
/// said in a notice, as the trusted path says it.
#[test]
fn a_push_the_daemon_refuses_is_rejected_whatever_the_reason() {
    let Some(mut rig) = Rig::start() else { return };
    let d = &mut rig.daemon;

    // Incoming files switched off while a card is open.
    let (_, call) = rig.obexd.push(1, "a.txt", "AA:BB:CC:DD:EE:01");
    d.state("the card", |s| prompt_of(s, "push").is_some());
    d.ask(json!({"id": "off", "op": "config", "files": "off"}));
    let r = call.recv_timeout(WAIT).unwrap();
    assert_eq!(obexd_writes(&r, "a.txt"), None, "files off with the card open: {r:?}");
    d.state("the agent unregistered", |s| s["obex"] == "off");
    d.ask(json!({"id": "ask", "op": "config", "files": "ask"}));
    d.state("asking again", |s| s["obex"] == "ready");

    // A transfer that cannot be read back from obexd.
    let r = rig.obexd.authorize("/org/bluez/obex/server/session9/transfer9").recv_timeout(WAIT);
    let r = r.unwrap();
    assert_eq!(obexd_writes(&r, "b.txt"), None, "an unreadable transfer: {r:?}");

    // A receive folder that cannot be made: accepted on the card.
    let blocker = rig.env.dir.join("blocker");
    std::fs::write(&blocker, b"a file, not a folder").unwrap();
    let bad = blocker.join("in");
    d.ask(json!({"id": "dir", "op": "config", "receiveDir": bad}));
    let (_, call) = rig.obexd.push(2, "photo.jpg", "AA:BB:CC:DD:EE:01");
    let s = d.state("the second card", |s| prompt_of(s, "push").is_some());
    let id = prompt_of(&s, "push").unwrap()["id"].clone();
    let r = d.ask(json!({"id": "acc", "op": "answer", "prompt": id, "accept": true}));
    assert_eq!(r["error"], "receive-dir", "{r}");
    let n = d.wait("the folder's notice", |v| v["event"] == "notice" && v["kind"] == "failed");
    assert_eq!(
        (n["direction"].as_str(), n["file"].as_str(), n["device"].as_str(), n["name"].as_str()),
        (Some("in"), Some("photo.jpg"), Some(PHONE), Some("Phone")),
        "{n}"
    );
    assert_eq!(n["message"], r["message"]);
    let r = call.recv_timeout(WAIT).unwrap();
    assert_eq!(obexd_writes(&r, "photo.jpg"), None, "accepted into a bad folder: {r:?}");

    // And from a trusted device, with no card.
    d.ask(json!({"id": "trusted", "op": "config", "files": "trusted"}));
    let (_, call) = rig.obexd.push(3, "notes.txt", "AA:BB:CC:DD:EE:02");
    let r = call.recv_timeout(WAIT).unwrap();
    assert_eq!(obexd_writes(&r, "notes.txt"), None, "trusted, into a bad folder: {r:?}");
    let n = d.wait("the trusted notice", |v| v["event"] == "notice" && v["kind"] == "failed");
    assert_eq!((n["direction"].as_str(), n["file"].as_str()), (Some("in"), Some("notes.txt")));
}

/// On the session bus any process of the user's can call the file agent. A
/// stranger is refused at once, with no card, even about a transfer obexd
/// really has.
#[test]
fn only_obexd_may_ask_about_a_push() {
    let Some(mut rig) = Rig::start() else { return };
    let session = "/org/bluez/obex/server/session77";
    let transfer = format!("{session}/transfer77");
    let server = rig.obexd.conn.object_server();
    server
        .at(session, MockSession { destination: "AA:BB:CC:DD:EE:01".into(), source: String::new() })
        .unwrap();
    let t = Arc::new(Mutex::new(TransferState {
        status: "queued".into(),
        name: "invoice.pdf".into(),
        size: 10,
        transferred: 0,
        session: session.into(),
    }));
    server.at(transfer.as_str(), MockTransfer { log: rig.obexd.s.log.clone(), t }).unwrap();
    let (sender, agent) = rig.obexd.s.agent.lock().unwrap().clone().unwrap();
    let stranger = blocking::connection::Builder::address(rig._buses.session.as_str())
        .unwrap()
        .method_timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let r = stranger.call_method(
        Some(sender.as_str()),
        agent.as_str(),
        Some("org.bluez.obex.Agent1"),
        "AuthorizePush",
        &(ObjectPath::try_from(transfer.as_str()).unwrap(),),
    );
    assert_eq!(error_name(&r), "org.bluez.obex.Error.Rejected");
    let d = &mut rig.daemon;
    assert_eq!(d.ask(json!({"id": "snap", "op": "snapshot"}))["ok"], true);
    assert_eq!(d.last.as_ref().unwrap()["prompts"], json!([]), "no card for the stranger");
}

/// A link lost in the middle of a batch: the transfer that was running fails,
/// the session can send nothing more, and that is one notice and one more
/// SendFile -- not a notice and a SendFile for every file left.
#[test]
fn a_link_lost_mid_batch_is_one_notice_and_no_more_tries() {
    let Some(mut rig) = Rig::start() else { return };
    let files =
        files_in(&rig.env.dir, "batch", &["p1.jpg", "p2.jpg", "p3.jpg", "p4.jpg", "p5.jpg"]);
    rig.obexd.s.dies.store(true, Ordering::SeqCst);
    let d = &mut rig.daemon;
    let r = d.ask(json!({"id": "lost", "op": "send", "device": PHONE, "files": files}));
    assert_eq!(
        (r["ok"].as_bool(), r["sent"].as_u64(), r["failed"].as_u64()),
        (Some(false), Some(0), Some(5)),
        "{r}"
    );
    assert!(r["message"].as_str().unwrap().starts_with("5 of 5 files were not sent"), "{r}");
    let tries = rig.obexd.s.log.all().iter().filter(|l| l.starts_with("SendFile")).count();
    assert_eq!(tries, 2, "the file the link dropped under, and the one that found it gone");
    let notices: Vec<&Value> =
        d.backlog.iter().filter(|v| v["event"] == "notice" && v["kind"] == "failed").collect();
    assert_eq!(
        notices.len(),
        2,
        "one for the file lost, one for the files never tried: {notices:?}"
    );
    assert_eq!(notices[0]["file"], "p1.jpg");
    assert_eq!(notices[1]["file"], "3 more files", "p3 to p5 were never offered");
    assert_eq!(notices[1]["direction"], "out");
}

/// A `pick` or `send` that fails before any file is offered is said to every
/// client: the page closes the settings panel as it asks for a pick, and the
/// watch client the reply goes to leaves with it.
#[test]
fn a_send_or_pick_that_fails_before_any_transfer_is_a_notice() {
    let Some(mut rig) = Rig::start() else { return };
    let files = files_in(&rig.env.dir, "out", &["a.txt"]);

    let mut w = Proc::spawn(&rig.env, "watch");
    w.state("watch connected", |s| s["bluez"] == true);
    w.send(json!({"id": "w-pk", "op": "pick", "device": PHONE}));
    let r = w.reply("w-pk");
    assert_eq!(r["error"], "portal-unavailable");
    drop(w);
    let d = &mut rig.daemon;
    let n = d.wait("the pick's notice", |v| v["event"] == "notice" && v["kind"] == "failed");
    assert_eq!(
        (n["direction"].as_str(), n["device"].as_str(), n["name"].as_str(), n["file"].as_str()),
        (Some("out"), Some(PHONE), Some("Phone"), Some("")),
        "{n}"
    );
    assert_eq!(n["message"], r["message"]);

    // A chooser that fails, rather than one the person closes.
    let answer = Arc::new(Mutex::new((2u32, Vec::<String>::new())));
    let _portal = blocking::connection::Builder::address(rig._buses.session.as_str())
        .unwrap()
        .name("org.freedesktop.portal.Desktop")
        .unwrap()
        .serve_at(
            "/org/freedesktop/portal/desktop",
            MockPortal { log: Log::default(), answer: answer.clone(), forger: None },
        )
        .unwrap()
        .build()
        .unwrap();
    let r = d.ask(json!({"id": "pk-fail", "op": "pick", "device": PHONE}));
    assert_eq!(r["ok"], false, "{r}");
    let n = d.wait("the chooser's failure", |v| v["event"] == "notice" && v["kind"] == "failed");
    assert_eq!(
        (n["direction"].as_str(), n["message"].as_str()),
        (Some("out"), r["message"].as_str())
    );

    // A phone obexd cannot reach: CreateSession fails, and no transfer began.
    rig.obexd.s.refuse.store(true, Ordering::SeqCst);
    let r = d.ask(json!({"id": "refused", "op": "send", "device": PHONE, "files": files}));
    assert_eq!(r["ok"], false, "{r}");
    let n = d.wait("the refused session", |v| v["event"] == "notice" && v["kind"] == "failed");
    assert_eq!(
        (n["direction"].as_str(), n["file"].as_str(), n["message"].as_str()),
        (Some("out"), Some("a.txt"), Some("Unable to connect")),
        "{n}"
    );
}

/// The portal's Response comes on a handle anybody can work out, and anybody
/// on the session bus can emit one. Only the portal's is the person's choice.
#[test]
fn a_response_from_anyone_but_the_portal_is_not_the_persons_choice() {
    let Some(mut rig) = Rig::start() else { return };
    let chosen = files_in(&rig.env.dir, "pick", &["chosen.txt"]);
    let secret = files_in(&rig.env.dir, "keys", &["id_ed25519"]);
    let forger = blocking::connection::Builder::address(rig._buses.session.as_str())
        .unwrap()
        .build()
        .unwrap();
    let answer = Arc::new(Mutex::new((0u32, vec![format!("file://{}", chosen[0])])));
    let _portal = blocking::connection::Builder::address(rig._buses.session.as_str())
        .unwrap()
        .name("org.freedesktop.portal.Desktop")
        .unwrap()
        .serve_at(
            "/org/freedesktop/portal/desktop",
            MockPortal {
                log: Log::default(),
                answer,
                forger: Some((forger.inner().clone(), vec![format!("file://{}", secret[0])])),
            },
        )
        .unwrap()
        .build()
        .unwrap();
    let r = rig.daemon.ask(json!({"id": "pk", "op": "pick", "device": PHONE}));
    let log = rig.obexd.s.log.all();
    assert!(!log.contains(&format!("SendFile {}", secret[0])), "a forged choice was sent: {log:?}");
    assert!(log.contains(&format!("SendFile {}", chosen[0])), "{log:?}");
    assert_eq!((r["ok"].as_bool(), r["sent"].as_u64()), (Some(true), Some(1)), "{r}");
}

/// A signal addressed to the daemon reaches it whatever a match rule says
/// about its sender. One from a process that is not bluetoothd or obexd must
/// change nothing: on the session bus a forged Status ended a running send as
/// complete, and on the system bus any user may send a signal.
#[test]
fn a_signal_from_anyone_but_the_service_is_ignored() {
    let Some(mut rig) = Rig::start() else { return };
    let files = files_in(&rig.env.dir, "out", &["slow.bin"]);
    rig.obexd.s.slow.store(true, Ordering::SeqCst);
    let d = &mut rig.daemon;
    d.send(json!({"id": "s1", "op": "send", "device": PHONE, "files": files}));
    let s = d.state("the send under way", |s| {
        s["transfers"].as_array().is_some_and(|t| t.iter().any(|t| t["status"] == "active"))
    });
    let tid = s["transfers"][0]["id"].as_str().unwrap().to_string();
    let fake = "/org/bluez/hci0/dev_66_66_66_66_66_66";

    let (on_session, _) = rig.obexd.s.agent.lock().unwrap().clone().unwrap();
    let (on_system, _) = rig.bluez.agent.lock().unwrap().clone().unwrap();
    let session = blocking::connection::Builder::address(rig._buses.session.as_str())
        .unwrap()
        .build()
        .unwrap();
    let system = blocking::connection::Builder::address(rig._buses.system.as_str())
        .unwrap()
        .build()
        .unwrap();
    session
        .emit_signal(
            Some(on_session.as_str()),
            tid.as_str(),
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(
                "org.bluez.obex.Transfer1",
                HashMap::from([("Status", ZValue::from("complete"))]),
                Vec::<&str>::new(),
            ),
        )
        .unwrap();
    let spoof: HashMap<&str, HashMap<&str, ZValue>> = HashMap::from([(
        "org.bluez.Device1",
        HashMap::from([
            ("Address", ZValue::from("66:66:66:66:66:66")),
            ("Alias", ZValue::from("Your headphones")),
        ]),
    )]);
    system
        .emit_signal(
            Some(on_system.as_str()),
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            &(ObjectPath::try_from(fake).unwrap(), spoof),
        )
        .unwrap();
    // Taken, the forgeries showed within a few milliseconds; then the real
    // services speak, and what they say must get through.
    std::thread::sleep(Duration::from_millis(300));
    rig.obexd.emit(&tid, HashMap::from([("Transferred", ZValue::from(900u64))]));
    rig.bluez.emit(
        PHONE,
        "org.bluez.Device1",
        HashMap::from([("RSSI", ZValue::from(-33i16))]),
        vec![],
    );
    let s = d.state("bluetoothd's own change", |s| device(s, PHONE)["rssi"] == -33);
    assert!(
        !s["devices"].as_array().unwrap().iter().any(|x| x["path"] == fake),
        "a stranger added a device: {s}"
    );
    // obexd's own progress, or the send already ended: whichever comes first.
    let s = d.state("obexd's own progress", |s| {
        s["transfers"][0]["transferred"] == 900 || s["transfers"][0]["status"] != "active"
    });
    assert_eq!(s["transfers"][0]["status"], "active", "a stranger ended the send: {s}");
}
