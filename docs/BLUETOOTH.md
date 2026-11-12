# Bluetooth

`jayos-bluetooth` is the whole of a Bluetooth manager's back end, for the
JayOS shell and usable without it: the adapter -- its name, its power,
whether other devices can see it and for how long, whether it takes pairing
requests -- and every device it knows or can hear, with signal, battery,
services and the rest; pairing and connecting, trust, blocking, renaming and
removing; a pairing agent that asks its client every question BlueZ asks;
and files sent and received through obexd. One daemon talks to BlueZ and
obexd over D-Bus and speaks JSON lines to its clients. `jayos-bluetooth send`
does the sending from a terminal or a file manager.

It moved here from JayOS's `rust/crates/jayos-bluetooth` on 2026-11-12, by the
owner's call: "Move all Bluetooth related stuff (except UI/QML) into
jayos-bluetooth from JayOS". What moved is everything that is not drawn: the
daemon, its tests, and the settings a headset's audio depends on -- the two
WirePlumber drop-ins in `share/wireplumber/` and BlueZ's `main.conf` in
`share/bluetooth/`. What stayed is JayOS's: the control room's Bluetooth
page, the pairing prompts, the network popup's Bluetooth tab, the bar's pill
and the island, the model that turns the daemon's state into their words
(`ts/src/bluetooth/BluetoothModel.ts` there), the doctor, and the installer
step that puts `main.conf` in place. JayOS's `docs/BLUETOOTH.md` is the
account of those, and the paths below that are not this repository's --
`services/BluetoothService.qml`, `settings/BluetoothPage.qml`,
`network/NetworkPopup.qml`, `ts/`, `tools/`, `install.d/` -- are JayOS's.

**What has run here, and what has only been read.** There is no Bluetooth
adapter, no bluetoothd and no obexd in the container this was built in, so
everything executed ran against stand-ins. `cargo test` is 105 tests: 77 in
the crate's modules, and 28 in `tests/daemon.rs` that start the real binary
against a mock BlueZ and a mock obexd, each on a `dbus-daemon` of its own
standing in for the system and the session bus.
`tests/shell/test-manager.sh` is 35 assertions on the binary run the way the
shell runs it, with no bus at all and on a private bus with nothing behind
it. Every other count and every mutation in the sections below was measured
in JayOS before the move, and is that tree's record, kept here because it is
the evidence for this code; `docs/RELEASES.md` and *Testing* say what this
repository has run since.

**Not run:** a Bluetooth adapter, bluetoothd, obexd, a desktop portal, and
Arch's own packages; and the BlueZ and obexd behaviour the daemon relies on --
obexd splitting the agent's answer at its last `/`, obexd saving a push on
any answer but `Rejected` and on its own timeout, obexd ending a transfer
with its status and then its removal, both daemons' 60-second timeouts, the
16-byte PIN limit -- which is read in their sources. Every overskride defect
named here is read in its source, and none was reproduced.

## Why a helper, and not Quickshell.Bluetooth

Quickshell 0.3.1 has a `Quickshell.Bluetooth` module, and it is not enough for
any of the parts that make this a manager rather than a list. Read at its
`v0.3.1` tag:

- **No pairing agent.** Nothing in Quickshell implements `org.bluez.Agent1`, so
  nothing in it can answer a PIN, a passkey or a "does this code match" — and
  BlueZ refuses an authentication request when no agent is registered ("No
  agent available for request type", `device.c`).
- **No OBEX**, so no file transfer in either direction.
- **No RSSI, TxPower, UUIDs, Class, Appearance, Modalias or manufacturer
  data** — so no signal, no sort by it, no distance, no services list.
- **The adapter's name cannot be written**: `name` is bound to `Alias` with no
  setter.
- **Every failure goes to the log and nowhere else.** `connect`, `pair`,
  `forget` and discovery report errors with `qCWarning`; QML is never told.
  Property writes are optimistic and never rolled back, so after BlueZ refuses
  one the switch shows a value BlueZ does not hold, and the setter's
  `if (x == bX) return;` swallows the retry.

So the choice was between a helper beside Quickshell's module, with two sources
of truth that can disagree, and one helper that sees everything. One helper
also means one owner for the agent, which matters more than it sounds: BlueZ
sends a question to one agent, and whoever registered it has to be there to
answer.

**zbus, not the `dbus` crate.** zbus is pure Rust, so the build needs no libdbus
headers, and its object server answers an agent call while another call on the
same connection is still outstanding — which is exactly the shape of pairing,
where the daemon's own `Pair` makes BlueZ call back into the daemon's agent.
The cost was measured on a probe crate before it was taken: 48 crates new to
the workspace and about 72 seconds of a cold release build. `zbus = "5.13"`
resolves to 5.13.2 under `resolver = "3"` in `Cargo.toml`, which picks
versions the declared Rust can build: zbus from 5.14 needs Rust 1.87, and the
crate declares 1.85.

## How it is put together

```
       system bus: org.bluez               session bus: org.bluez.obex, the portal
                    \                           /
                     jayos-bluetooth daemon   (this repository)
                     |  stdin and stdout: its owner      |  control.sock: everyone else
     services/BluetoothService.qml               settings/BluetoothPage.qml
     the prompts, the island, notifications      network/NetworkPopup.qml
                     |  ShellState.bluetooth                 each runs jayos-bluetooth watch
     TopBar.qml: the pill, on every screen
```

**The shell owns the daemon through its stdin.** `BluetoothService.qml`, a
plain Item in `services/Services.qml` beside UsbGuardService, starts
`jayos-bluetooth daemon` with stdin open and reads its stdout; when the shell goes,
stdin closes and the daemon exits 0. The reason is the agent. The daemon
registers itself with BlueZ as the default pairing agent, and a daemon that
outlived its shell would keep that agent registered with nobody to show the
questions to: every incoming pairing would wait out BlueZ's 60 seconds and
fail. That is also why it is not
a systemd unit — a unit outlives the shell it answers for. The service starts it
again three seconds after an ordinary end, ten seconds after a fatal one, and
five minutes after finding no binary, since only installing this package fixes that. The
fatal one is another daemon holding the lock, and the commonest other daemon is
this shell's own from before a reload: the old generation's process is ended
only when its `Process` object is destroyed (`Process::~Process`, Quickshell's
`src/io/process.cpp`, read), and nothing orders that before the new generation
starts its own. So a new daemon tries the lock for five seconds before it calls
itself the second one (`LOCK_GRACE` in `src/control.rs`),
and the service's ten seconds are chosen to be past that.

**The settings go first.** Every start sends the `config` command before the
daemon has registered anything: until its first `config` it answers pairing and
asks about files, and a machine set to leave pairing to blueman should not have
a moment of the opposite.

**The page is a second client, over a socket.** A settings page is built by
`SettingsPanel.qml`'s Loader in a root of its own and cannot reach an object in
`Services.qml`, so the page runs `jayos-bluetooth watch` — a client of the
daemon's socket that passes on what is written to it and prints every event —
while it is on screen. The socket is
`$XDG_RUNTIME_DIR/quickshell/bluetooth/control.sock`, 0600 in a 0700
directory, and a peer with a different uid is refused. A lock file beside it,
held with `flock` for the daemon's life, decides who the daemon is, not the
socket file: a socket left by a daemon that was killed is debris, and the lock
says so where the file cannot. A second daemon that still cannot take the lock
after its five seconds prints
`{"event":"fatal","message":"another jayos-bluetooth daemon is running"}` and
exits 1.

**One state, sent whole.** Every client receives a `state` the moment it
connects, and then at most one every 400 ms while anything changes; a prompt
appearing or going is sent at once rather than waiting its turn. Whole rather
than as changes, because a client that connects late or drops a line cannot
then drift from the daemon, and with every key on every object present, because
QML that reads a missing key gets `undefined` and draws it. The model behind it
is BlueZ's own objects, filled by `GetManagedObjects` and kept current by
`InterfacesAdded`, `InterfacesRemoved` and `PropertiesChanged`. An RSSI or
TxPower that BlueZ invalidates — it does when discovery stops — is removed and
drawn as "no reading" rather than kept stale. A socket client that falls 256
lines behind is not reading and is dropped, and whatever it wrote that is still
on its way in is ignored. It used to be run, and a `scan on` from such a client
took discovery interest that nothing could give back, so the adapter scanned
for good.

**Signals in the order they were sent, and only from the service.** Each bus
connection's signals are read from one stream, in the order the bus delivered
them (`pump`, `src/bus.rs`). It was a stream per match
rule, each on a thread of its own, and that lost the order: obexd ends a
transfer with its `Status` set to `complete` and removes the object straight
after, the two arrived on different threads, and the daemon sometimes saw the
transfer removed before it saw it finish — against the mock obexd, 16 to 34 of
300 delivered files reported failed, and now and then a send never answered. A
signal is taken only from the unique name that owns the service, and a change
of owner only from the bus driver: the bus applies a match rule's sender to
broadcasts alone, and delivers a signal addressed to one connection whoever
sent it, so a forged `PropertiesChanged` sent straight to the daemon marked a
running send complete. While the daemon's `GetManagedObjects` is outstanding,
BlueZ's signals are held and applied over its answer, so a device added just
after bluetoothd answered is not lost under the older snapshot, and an answer
from a bluetoothd that has since left the bus is dropped rather than applied.
obexd numbers its sessions and transfers from zero in each process, so what one
obexd said about its transfers is forgotten when it goes, or when its name
passes straight to another process — a new obexd's first transfer has the path
of the last one's first, and a `complete` kept from before would have finished
it at once. A send whose obexd stops is ended as failed, whatever *Incoming
files* is set to.

**Nothing slow runs on the loop.** `Pair` can take more than a minute, and the
question it makes BlueZ ask has to be forwarded by the same loop that is waiting
for it, so `Pair`, `Connect`, `CreateSession` and the portal each run on a
thread of their own. The agent methods are `async` and await the answer: a
blocking one stalls zbus's executor, and BlueZ's `Cancel` then cannot be
delivered at all. That was shown twice rather than assumed — by the probe the
design started from, and by a mutation of the crate, under which the
passkey-then-Cancel case times out.

**The agent and the default.** The pairing agent is
`/org/jayos/bluetooth/agent`, registered with capability `KeyboardDisplay` and
then made the default; the file agent is `/org/jayos/bluetooth/obex`. BlueZ
sends a pairing a program starts to that program's own agent, and every other
question to the default (`agent_get`, `src/agent.c`, read) — so a pairing
started from the page or the popup comes here, and so does a phone asking to
pair. Another program's agent — blueman's, or a `bluetoothctl` somebody left
running — does not take the questions from it: a newly registered agent becomes
the default only when there is none (`agent_create`, same file, read). When bluetoothd leaves the bus the
model is cleared, every open prompt is dropped and its pending call cancelled;
when it comes back the daemon fetches everything again and registers again.


## Audio while discovering

**A scan stops while audio streams, and starts again when it ends.** The
owner's report of 2026-11-12 was a WH-1000XM5 whose audio lagged and cut out
frequently. While BlueZ discovers it runs an inquiry on BR/EDR and an LE scan
beside it, at full duty (src/adapter.c's discovery, and the kernel's
interleaved discovery in `net/bluetooth/hci_sync.c`; read, not run), and the
controller has one radio: every inquiry window is time an A2DP link is not
served, and the headset's few hundred milliseconds of buffer run dry. JayOS's
page scans for as long as it is open and its network popup's Bluetooth tab for
as long as it is shown, so the case is ordinary -- open either to connect the
headset, then listen with it open.

So while a `MediaTransport1` under an adapter is `pending` or `active`, the
daemon stops its own discovery there (`Core::audio_yield`,
`src/daemon.rs`; `Model::streaming_on`, `src/model.rs`). Every client keeps its
interest: a `scan on` is answered `ok` and held back, and discovery starts
again when the last stream goes idle or its transport is removed -- a headset
switched off mid-track sends `InterfacesRemoved` and no `State`. Each adapter
in `state` carries `audioPaused`, so a page can say why nothing new turns up.
An `idle` transport -- connected, nothing playing -- holds nothing back. Only
this daemon's discovery is touched: BlueZ keeps a session per D-Bus
connection, and another program's scan is that program's.

**And the sink's quantum is held.** `share/wireplumber/wireplumber.conf.d/
51-bluez-headset.conf` asked for 1024 frames on every `bluez_output` node with
`node.latency`, which is a request: the graph runs at the smallest request
among its nodes, so a call, a browser or a game playing beside the music took
the sink back to 256 frames. `node.force-quantum = 1024` holds it while the
sink runs -- the floor jayos-audio already holds under a call's recording.

The third remedy is the headset's own: Sony's *Bluetooth Connection Quality*
setting chooses between priority on sound quality and priority on a stable
connection (Sony's help guide for the WH-1000XM5; read, not run), and
jayos-sony-xm5 sets it (`qs-sony send connection stable`). It is the person's
setting, so nothing here changes it unasked.

**Run:** `discovery_yields_to_audio_streaming_over_the_adapter` in
`tests/daemon.rs`, against the mock BlueZ with a mock `MediaTransport1` --
idle, pending, active, idle again, removed, a scan asked for over a stream,
and the interest given back by a client leaving and by the owner -- and
`audio_streams_only_where_a_transport_under_the_adapter_is_acquired` in
`src/model.rs`. Mutated on 2026-11-12, eight changes -- the start held back
no longer, a `State` change, an arrival and a removal each ignored, a
held-back scan stopped on `scan off` and on a client leaving, `pending` not
counted, and the adapter's prefix without its slash -- each failed one of the
two, and each was restored. **Read, not run:** BlueZ, PipeWire and WirePlumber
themselves, and a headset. Whether these were the causes on the owner's
machine is for the machine to say: `pw-top` shows the sink's quantum under
QUANT and its underruns under ERR.

## Everything overskride does, and where it is now

The first column is overskride's feature inventory, read in its source; the
third is what has been done here to the second.

| overskride | here | evidence |
|---|---|---|
| The adapter at start: the stored one, else `hci0`, else the first | `adapterFor` in `ts/src/bluetooth/BluetoothModel.ts`: the one chosen on the page, else `hci0`, else the first by path | Executed: `ts/test/bluetooth.mjs` |
| A window when bluetoothd or the adapter is missing, running `systemctl enable --now bluetooth` through pkexec | The page's placeholder says which it is — the helper not installed, the daemon not answering, bluetoothd not running, no adapter — and shows `sudo systemctl enable --now bluetooth` to copy. Nothing runs it for you. rfkill and power are the headline above the switches | Executed: the wording (`linkSummary`), and the placeholder waiting and failed in `tools/test-page-layout.sh` |
| The adapters listed, one checked; choosing one switches the list | An adapter chooser, when there is more than one; the device list follows it. Not remembered, as overskride's choice is not | Read: every run here had one adapter |
| Powered, with a read-back and a revert on error | *Bluetooth*, a switch drawn from the state that moves only when the next state says it did; an error is said on the page's top line | Executed: the `power` command against the mock |
| Discoverable | *Visible to other devices* | Executed |
| Discoverable timeout, in minutes from 0 to 60 — and fed seconds at start | *Stays visible for*: until turned off, 1, 2, 3 (BlueZ's default), 5, 10 or 30 minutes, or an hour; seconds throughout | Executed |
| Rename the adapter | The page's name field | Executed |
| — | *Accept pairing from other devices*, the adapter's `Pairable` | Executed |
| Discovery for as long as the window is open; *Refresh* restarts it | *Look for devices*, on while the page is open. The interest is the page's own, and discovery stops when its last interested client has gone. There is no refresh: the list follows BlueZ's signals | Executed: a scan released when its client disconnects, and started again when the adapter powers back on |
| A row: the alias, or "Unknown Device"; a connected icon; an RSSI icon | A glyph, the name as plain text, chips for Connected, Paired, Trusted and Blocked, the battery, and four signal bars at overskride's thresholds | Executed: the model; the page under Qt |
| Sorted: unknown last, then connected, trusted, RSSI **weakest first**, name | Connected, paired, trusted, named before unknown, **stronger signal first** with no reading last, then name and address | Executed, and mutation-tested |
| *Hide Unknown Devices*, for rows added after it is switched on | *Hide unnamed devices*, applied to the whole list at once, and never to a paired or connected device | Executed |
| The device page: icon, alias, a Connected switch with a spinner | The row opens in place: Connect or Disconnect, Pair or Cancel, with the daemon's `busy` while a command is in flight | Executed: the crate; the page under Qt |
| Connect: `Pair` when not paired, then `Connect` twice | `pair`: `Pair`, then `Trusted`, then one `Connect`; the reply is ok when `Pair` worked, with the connection's own result beside it. `connect` on an unpaired device pairs first | Executed |
| Rename, refusing a name another device has | The rename field, refusing a name over 248 bytes or one another device on the adapter shows; empty gives the device back its own name | Executed |
| Trusted and Blocked switches | The same, and *Wakes this computer* for an input device, the only kind BlueZ's `WakeAllowed` applies to | Executed |
| Remove, after a confirmation | *Remove…*, then *Remove* or *Keep* in place | Executed: the command; the confirmation read |
| Battery, from `Battery1` | `battery` on the row, from the same interface | Executed |
| Audio profiles, from PulseAudio's card, `off` excluded; a switch for `off` | *Audio profile*: every profile the card offers and can use, best first and `off` last, through `jayos-devices profiles` and `jayos-devices profile <card> <name>`. PipeWire has a card only for a connected device, so only then, with *Check again* for a card that arrives late | Executed: `cargo test -p jayos-hw` and the page fetching them under Qt; pactl's format read |
| *More Info*: name, address, manufacturer, type, distance, services | *More info*: address and its type, class, manufacturer, BlueZ's icon name, RSSI and TxPower as numbers, distance, services, modalias, bonded, legacy pairing. The distance takes TxPower less 41 dB as the one-metre reading, where overskride takes TxPower itself (*What overskride gets wrong*, below) | Executed: the model |
| A pairing agent with seven methods | The agent above, and *Pairing prompts* below | Executed |
| Receiving: *Auto Accept* from trusted devices, a store folder, a dialog | *Incoming files* (off, ask, trusted), *Save received files to*; *Receiving files* below | Executed |
| Sending: a file chooser, `CreateSession`, `SendFile` each in turn | *Send files…* through the desktop portal's chooser, which closes the control room so the chooser can be used, and `jayos-bluetooth send`. A batch stops at the first file obexd will not take | Executed: against the mock obexd and a mock portal; the panel closing under Qt |
| A transfers popover: progress, rate, cancel, finished rows gone after 60 s | *Transfers* on the page: a bar, `42% · 1.2 of 3.4 MB · 512 KB/s · 3 s left`, Cancel or Dismiss; finished rows go after 60 s; the island while one runs | Executed |
| Toasts, from a table of BlueZ's errors | Through `friendlyError`, against BlueZ's real strings, falling back to BlueZ's own message rather than "Unknown error": a line under the buttons of the row whose button was pressed, while that row is open; the page's top line otherwise, and for the adapter's switches; and the card's failure line | Executed: every row; the row's line under Qt |
| Settings in GSettings | Four keys in `settings.json` | Executed: the `config` command against the mock, and the service sending it again on a change, under Qt in `tools/test-bluetooth-ui.sh` |
| Window size, keyboard shortcuts, *About* | Nothing: it is a page in the control room | — |
| Its README's "doesn't work": auto-trust on pair | Done: `pair` trusts | Executed |
| Its README's "doesn't work": remembering an allowed service | Trust is the remembering: BlueZ authorises a trusted device's services without asking the agent (`process_auth_queue`, `src/adapter.c`), and an untrusted one is asked each time | Read |
| Its README's "doesn't work": running in the background | The shell is the background: the daemon runs for the session, and the prompts come with the page closed | Read: no compositor here |

## What overskride gets wrong, and this does not

Each of these is read in overskride's source; none was reproduced. The fix is
named beside it, with what holds it.

- **It sorts the weakest signal first.** `one.cmp(two)` over RSSI, ascending,
  under a README that says "sorting by signal strength". `compareDevices` puts
  the stronger first, and `ts/test/bluetooth.mjs` keeps overskride's order as a
  regression case: reversing the direction fails four assertions.
- **It never trusts what it pairs**, and its README lists that as not working.
  `pair` sets `Trusted` after `Pair`, so the device reconnects without asking —
  which is what the network popup has always done for a paired device it
  connects.
- **It calls `Connect` twice after `Pair`**, and the second can come back
  already-connected, which it treats as a failure. One `Connect` here.
- **Its display prompts block BlueZ.** `DisplayPinCode` and `DisplayPasskey`
  return only after *Okay* is pressed, where BlueZ expects an immediate reply,
  and the passkey dialog is cached forever, so a second one is invisible. Here
  they reply at once, the card only shows the code, and a later
  `DisplayPasskey` for the same device updates the same card, with the digits
  typed so far.
- **It cannot be cancelled.** Its agent waits on a dialog in a hot loop that
  never yields, so BlueZ's `Cancel` — sent at 60 seconds, or when the other
  device gives up — cannot interrupt it, and the dialog stays up for a pairing
  that is over. `Cancel` here answers every pending question with
  `org.bluez.Error.Canceled` and removes its card.
- **It rejects a passkey of 000000**, parsing to a number and treating 0 as
  "nothing typed". A passkey is six digits, and 000000 is one of them.
- **It never calls `RemoveSession`.** The method is defined
  (`src/obex/obex_utils.rs:36-47`) and nothing calls it, so every send leaves an
  OBEX session open. Here a batch's session is removed at its end, whatever
  happened, and the integration test asserts it.
- **Received files go to `~/.cache/obexd` first.** It answers obexd with a bare
  name, obexd writes into its cache, and on completion overskride renames the
  file into the store folder — overwriting a file already there, failing across
  filesystems, and leaving the file in the cache with "Unknown error occurred"
  when the move fails. Here the answer is an absolute path inside the receive
  folder, so obexd writes it where it belongs and there is no move.
- **Its rate is per signal, not per second.** obexd's client reports progress
  once a second and its server on every chunk it writes, so for an incoming
  file the "KB/s" is bytes per chunk; one global last-count is shared by every
  transfer; a signal that carries only a status sets the bar to 0; and a size
  of 0 divides by zero. The rate here is computed in the daemon, per transfer,
  over the last three seconds of samples, and the percentage is guarded.
- **Its distance puts any device that advertises a TxPower twenty times too
  far away.** It takes TxPower as the reading expected at one metre
  (`src/bluetooth/device.rs:485-495`), and TxPower is the advertised transmit
  power level — the power leaving the antenna, which BlueZ's own discovery
  filter takes as the start of the path loss (`tx_power - rssi`,
  `src/adapter.c`). Free space costs about 40 dB over the first metre across
  the 2.4 GHz band, so a device read at −66 dBm that advertises +4 dBm was
  215 m away. `distanceMetres` takes TxPower less 41 dB, the figure the
  Eddystone beacon format uses (from memory, not fetched), which puts that
  device at about 9 m; `ts/test/bluetooth.mjs` keeps it as a regression case,
  and the formula put back to overskride's fails four assertions.
- **Its cancel button cancels whichever transfer is current**, not the row it is
  on, and can end the receiving agent's loop as a side effect, after which
  pushes go unanswered. `cancel` here names its transfer.
- **Device names and file names are taken as they come.** A file name in a push
  reaches the store folder unsanitised, and names are drawn as markup. Here a
  sender's file name is cleaned before it is used (*Receiving files*, below),
  control and direction-override characters are removed from every device
  string before it is drawn, and every Text that draws one is plain text.
- **It panics on what BlueZ is allowed to leave out**: a push with no `Name` or
  `Size`, a device with no `UUIDs`, an obexd that already has an agent. Absent
  values are `null` here, and a second agent is `obex: "busy"` on the page.
- **Its error table misses BlueZ's real strings** — `invalid-argument` is
  singular, LE's is `abort-by`, not `aborted-by` — and two of its own keys map to
  nothing, so the user reads "Unknown error occurred". `friendlyError` is checked
  against `src/error.h` row by row, and falls back to BlueZ's own message.
- **Its service names are wrong in places, and one never matches.** `0x1108` is
  Headset, not Hands Free, and the SIM Access key is upper-case where the UUIDs
  compared with it are lower-case. Corrected, and compared case-insensitively.
- **Hide Unknown applies only to rows added later**, the discoverable timeout is
  read in seconds at start and minutes afterwards, and the auto-accept warning's
  answer is not used — the setting itself is not saved. Hiding and accepting are
  settings here, applied as they change and saved, and the timeout is BlueZ's
  own, in seconds throughout.
- **Discovery never stops** while it is open, and after a refresh or an adapter
  change, devices already listed stop updating. Discovery here is held per
  client and released with it, and the list is one model fed by BlueZ's signals,
  so there is nothing to resubscribe.

## Pairing prompts

`bluetooth/BluetoothPrompt.qml` is one window holding a card per question,
`bluetooth/BluetoothCard.qml`, up to three at a time with a line saying how many
more are waiting.

| kind | BlueZ asks | the card shows | answers |
|---|---|---|---|
| `pin` | `RequestPinCode` | a field, 1 to 16 characters | *Pair*, *Cancel* |
| `passkey` | `RequestPasskey` | a field, six digits; 000000 is valid | *Pair*, *Cancel* |
| `confirm` | `RequestConfirmation` | the passkey, large, padded to six digits | *Confirm*, *Cancel* |
| `authorize` | `RequestAuthorization` | who is asking | *Allow*, *Deny* |
| `service` | `AuthorizeService` | the service, named from its UUID | *Allow*, *Deny* |
| `displayPin` | `DisplayPinCode` | the PIN to type on the other device | *Cancel pairing*, *Dismiss* |
| `displayPasskey` | `DisplayPasskey` | the passkey, and how many digits are typed | *Cancel pairing*, *Dismiss* |
| `push` | `AuthorizePush`, from obexd | the file's cleaned name and size | *Accept*, *Decline* |

**The keyboard, only for a pairing somebody here started.** USB guard's prompt
takes no keyboard focus, so that an Enter meant for a terminal cannot answer it,
and every yes-or-no here is answered the same way: by a click. A PIN cannot be
clicked, so a card with a field needs keys — but not every such card was asked
for. While this computer is visible, a device in range can start legacy pairing
and open a PIN card with nobody here involved, and a window that took the
keyboard for it would send the rest of whatever was being typed elsewhere, Enter
and all, as the PIN, which the other end can recover from the exchange. So the
window takes the keyboard exclusively, as the polkit dialog does for a
password, only when the card's device has a `pair` or a `connect` in flight
through the daemon — its `busy` in the state, set for a command from any of the
daemon's clients — and gives it back the moment that card goes (`promptFocus`
in `ts/src/bluetooth/BluetoothModel.ts`). Any other card with a field takes no
keyboard at all until somebody presses on its field, which claims it: the
window then takes the keyboard for that card as it would for one asked for.
Not on-demand focus: Hyprland gives a layer surface that asks for it on demand
the keyboard the moment it maps, which is exactly when an unasked card
appears, and Tab would then walk into its field. A press reaches the window
without keyboard focus, because pointer input does not depend on it, and an
unclaimed field is off the tab chain besides.

**Accept arms; saying no does not.** As on USB guard's card, the accepting
button takes no click for 1.5 seconds after the question arrived — counted from
when the service first saw it, not from when the card was built, because cards
are rebuilt whenever the list changes. Saying no is never the dangerous answer,
so *Cancel*, *Deny* and *Decline* work at once. Return in a field is armed the
same way: the field starting empty stops only a stray Enter, and a line typed
for another window that ends in Enter would otherwise go as the PIN.

**A display prompt is already answered.** BlueZ expects `DisplayPinCode` and
`DisplayPasskey` to return at once, and the daemon has; the card exists to show
the code to type on the other device. Its one real action is *Cancel pairing*,
which cancels the pairing itself (`cancel-pair`); *Dismiss* hides the card here
and changes nothing, for as long as the card shows the same code. The daemon
keeps one display prompt per device and writes a retried pairing's new code into
it, and a pairing that fails ends no display prompt, so a card dismissed and
then retried within its minute comes back with the new code, which is shown
nowhere else; the count of digits typed changing under the same code leaves it
hidden. It stays in the state until BlueZ cancels it, the device pairs, or 60
seconds pass — and the last is the one to know about: a device that is removed
mid-pairing leaves its card up until then.

**Where, and over what.** The Overlay layer of the output Hyprland has focused,
hanging from the top under the island — USB guard's recipe. A pairing started
from a phone in your hand has to be answered inside BlueZ's 60 seconds, so it is
not hidden under a fullscreen window or by Do Not Disturb. Each card counts down
to that deadline. The daemon's own backstop is 65 seconds, past BlueZ's, so that
BlueZ is the one that decides. A push card is the reverse, and *Receiving
files* says why: obexd takes its own 60-second timeout as a yes, so a push card
counts down 55 seconds and the daemon then refuses it.

**What the cards keep.** The service hands the window a new list only when the
prompts in it change, and keeps what was typed: a state arrives every 400 ms
while anything moves, RSSI during a scan does, and a rebuilt card would lose the
PIN half-typed into it.

**Answer pairing requests** off (`bluetoothAgent`) unregisters both agents, for
a machine where blueman or another program should answer instead, and the page
says pairing requests are left to another program. If no program answers, a
device that asks a question cannot pair.

## Receiving files

**Where they land.** *Save received files to* (`bluetoothReceiveDir`), or, when
it is empty, the XDG download folder: `$XDG_DOWNLOAD_DIR`, else
`XDG_DOWNLOAD_DIR` in `user-dirs.dirs`, else `~/Downloads`. A leading `~/` is
expanded, and a relative path is taken from the home folder, because the daemon's
working directory is wherever the shell happened to start it. The folder is made
on first use, 0700 when it is new: what arrives over Bluetooth is nobody else's
business. An existing folder keeps its mode.

**The name is the sender's, so it is cleaned.** `sanitise` in
`src/files.rs` keeps what follows the last `/` or `\`,
drops control characters and the invisible characters that reorder text on
screen — a name like `photo` + U+202E + `gpj.exe` displays as `photoexe.jpg` in
the question that asks whether to accept it — trims spaces and dots from both
ends, which also rules out `.` and `..`, and keeps at most 200 bytes, cut on a
character boundary. What is left of a name that was nothing else is `file`. RUN
in the integration test: `../../evil.txt` arrives as `evil.txt` inside the
folder, and a push named `..` arrives as `file`. The answer to obexd is an
absolute path, because obexd splits whatever the agent returns at its last `/`
and writes there (`agent_reply`, `obexd/src/manager.c`, read): the agent's
answer is the only place this can be enforced.

**A name already taken gets a number** — `notes.txt`, then `notes (1).txt` —
checked against the folder and against names promised to transfers still in
flight, so two pushes of `photo.jpg` at once do not get the same path. A name
already in the folder is never the one chosen.

**Every no is `Rejected`, and comes before obexd's minute is up.** obexd
refuses a push only when its agent answers `org.bluez.obex.Error.Rejected`. Any
other error, `Canceled` among them, and no answer inside obexd's own 60
seconds, leave it saving the file under its own root folder with the sender's
name (`agent_reply` and `manager_request_authorization`, `obexd/src/manager.c`;
`opp_chkput`, `obexd/plugins/opp.c`; read). So every way the daemon says no is
a `Rejected`, and the file agent has no `Canceled` to give: *Decline*; a card
nobody answered, refused at 55 seconds, five before obexd would take the
silence as a yes; *Incoming files* switched off with a card open; a transfer
the agent cannot read; the daemon exiting; obexd's own `Cancel`; and a receive
folder that cannot be used. The mock obexd in the integration test saves on
anything but `Rejected`, as obexd's source reads, and
`a_push_the_daemon_refuses_is_rejected_whatever_the_reason` refuses a push for
four of those reasons; two unit tests in `prompts.rs` hold the 55 seconds and
the answer a timeout gives.

**The three policies** (`bluetoothFiles`):

| | |
|---|---|
| `off` | the file agent is not registered, and obexd refuses a push when no agent is (`opp_chkput`, `obexd/plugins/opp.c`, read) |
| `ask` | every file, from every device, is a card. The default |
| `trusted` | a device whose `Trusted` is set sends without asking; any other is asked |

**`trusted` is a real cost, and the setting says so in its own words**: *accept
from trusted devices unasked, ask the rest*, under a description that ends *any
device you once trusted can then send files*. Pairing from the page trusts the
device, so that is every phone, headset and laptop ever paired here, including
one sold since. A file arriving is not a file opened, but it is a file written
into your downloads by somebody you did not ask. Overskride showed a warning
the first time and then ignored the answer; here the warning is in the choice,
every time it is looked at.

A file that arrives is a notification — *Received photo.jpg*, *from Phone —
saved to* the folder — and one that fails is too, including a push accepted,
from the card or unasked from a trusted device, into a folder that cannot be
used: the daemon's reply reaches only the card, and the card goes in the next
state. A file sent is not: it was watched across the island and is listed on
the page, and a notification per file of a twenty-photo batch would bury
anything else that arrived.

## Sending files

*Send files…* on a device that offers Object Push opens the desktop portal's
file chooser (`org.freedesktop.portal.FileChooser`, `OpenFile`, several files
allowed), and sends what is chosen in order, in one OBEX session, removed when
the batch ends. It closes the control room as it does: the panel is an Overlay
layer surface holding the keyboard and every click outside it, and the chooser
is an ordinary window, which would open underneath where it could be neither
seen nor used. The panel closes only once the `pick` has been written to the
daemon, which carries the pick and the send on without the page; the island
shows the transfers, and a failure is a notification. The chooser's answer
is subscribed to before the chooser is asked for — the portal names the path
its answer will come on in advance, and an answer that arrives before the
subscription is lost — and a mutation that subscribed after made the test time
out. It is taken only from the name that answered `OpenFile`: anyone on the
session bus can send a `Response`, and the bus delivers one addressed to the
daemon whatever its match rule says, so a forgery could otherwise choose the
files. It does not need the device connected first, as overskride did: Object
Push opens a connection of its own.

**What reaches you when it goes wrong.** Closing the chooser sends nothing and
says nothing. Anything else that ends a send or a pick before a file goes — no
portal (`portal-unavailable`) or the chooser failing, bluetoothd or the device
gone, a file that cannot be read, obexd refusing the session — is a `failed`
notice to every client as well as the reply, and the shell makes it a
notification, because the page that asked has usually closed. Once files are
going, a batch stops at the first one obexd will not take. Every file is
checked before the session is made, so a `SendFile` that fails is the session
failing — a lost link answers *Session not connected* (`client/session.c`,
read) — and each file after it would fail the same way, a notification apiece.
A lost link is one notification for the file it took, and one more counting
the files never offered — *Could not send 3 more files* — because the reply
that counts them goes to the page, which has usually gone. A transfer whose
obexd left the bus before the daemon heard of it ends then: sent, if that
obexd said it had completed before it went, and failed otherwise. What obexd
said about a transfer is kept by the obexd that said it, since a restarted one
hands out the same paths.

**Two adapters.** On a machine with more than one, a send names the address of
the device's own adapter as `Source`, so the file leaves through the radio the
device is paired with; with one adapter obexd chooses, and naming it would fail
the day its address changed (`send_source`,
`src/model.rs`, held by
`model::tests::a_send_names_its_adapter_only_when_there_are_two`).

From a terminal, or a file manager's custom action, with the shell running:

```bash
jayos-bluetooth send AA:BB:CC:DD:EE:FF photo.jpg notes.pdf   # an address, or a BlueZ object path
jayos-bluetooth status | jq '.devices[] | {name, address, connected}'
```

`send` prints each of its transfers as JSON as it moves, then the daemon's
reply; it exits 0 when every file arrived, 1 when any did not or the device is
not one BlueZ knows, and 2 when no daemon answered — it is a client of the
shell's daemon, not a second one. Relative paths are made absolute first, and a
missing or unreadable file is refused before anything is sent, naming the file.
A file manager's action that runs it with the selected files and a fixed address
is all the integration there is; nothing here writes one.

## obexd: found at run time, not installed by name

File transfer needs obexd, BlueZ's OBEX daemon, on the session bus. It is not a
command — it lives at `/usr/lib/bluetooth/obexd` — so no `command -v` finds it,
and it is in no install list, deliberately.

**Read, not run:** Arch's packaging, as far as it could be read from here,
installs obexd in the `bluez` package itself. The PKGBUILD at the head of the
archived svntogit mirror's `bluez` branch — `bluez` 5.66-1; the mirror stopped
when Arch's packaging moved to its GitLab in 2023 — builds with
`--libexecdir=/usr/lib` and installs, in `package_bluez`,
`install-pkglibexecPROGRAMS` (bluetoothd and obexd),
`install-systemduserunitDATA` (`obex.service`) and `install-dbussessionbusDATA`
(the session bus's activation file for `org.bluez.obex`), and links
`dbus-org.bluez.obex.service` to the user unit, for Arch's bug 45816. It was
fetched and read here on 2026-10-08. `bootstrap.d/80-desktop.sh` already offers
`bluez`. Whether a later release split obexd into a package of its own was not
checked: the proxy this container goes through refused archlinux.org and
gitlab.archlinux.org. A package name written into an install list on that basis
would be a guess, and a wrong one would fail the install.

**So the daemon finds out.** Registering the file agent with
`org.bluez.obex.AgentManager1` starts obexd through D-Bus activation where it is
installed; where nothing provides `org.bluez.obex`, the bus answers
`ServiceUnknown`, the state says `obex: "absent"`, the page says *File transfer
needs obexd, which BlueZ provides (/usr/lib/bluetooth/obexd)*, and the daemon
asks again a minute later. Pairing, connecting and everything else carry on
without it. obexd takes one agent per session (`AlreadyExists`,
`obexd/src/manager.c`, read), so another program holding the slot — a second
Bluetooth manager's applet — is `obex: "busy"`, said as *Another program is
answering file transfers*.

## The settings

Four keys, drawn on the page under its devices and transfers once there is an
adapter to manage, and found by the control room's search before then. The
first three are what the service sends the daemon as its `config` command, on
every start and whenever one changes.

| key | default | row | what it does |
|---|---|---|---|
| `bluetoothAgent` | `true` | *Answer pairing requests* | registers the pairing agent and the file agent; off for blueman or another agent |
| `bluetoothFiles` | `"ask"` | *Incoming files*: off, ask, trusted | *Receiving files*, above. Anything but those three words is sent as `ask` |
| `bluetoothReceiveDir` | `""` | *Save received files to* | empty is the XDG download folder |
| `bluetoothHideUnknown` | `false` | *Hide unnamed devices* | leaves out devices that advertise only an address, here and in the network popup's tab; paired and connected ones always show |

`docs/SETTINGS.md` has them with the other rows.

## BlueZ's main.conf, which was never read

The settings the headphones need -- `Experimental` for the battery, and
`FastConnectable`, with the controller mode, the temporary-device timeout and
the reconnect policy beside them -- were once installed by JayOS as a drop-in
at `/etc/bluetooth/main.conf.d/50-quickshell.conf`, and bluetoothd reads
`<configdir>/main.conf` and nothing else (`load_config`, BlueZ's
`src/main.c`, read). On every machine that had "installed" them, every key
was whatever main.conf said.

So the file is whole, `share/bluetooth/main.conf`, and this package installs
it as `/usr/share/jayos-bluetooth/main.conf`, not over
`/etc/bluetooth/main.conf`: that path is the `bluez` package's, a `backup=`
file there, and two packages cannot own one path. JayOS's
`install.d/60-system.sh` installs this copy over `/etc/bluetooth/main.conf`
as root, compares byte for byte so that a run with it in place calls no
`sudo`, keeps the file it replaces once as `main.conf.jayos-orig`, and
restarts a running bluetoothd with `try-restart` to read it; JayOS's
`docs/BLUETOOTH.md` has that step and the doctor that reads the effective
`[General] Experimental` back. An upgrade of bluez leaves the installed file
in place and puts upstream's beside it as `.pacnew`.

## The protocol

One JSON object per line, UTF-8, in both directions, on the daemon's stdin and
stdout and on its socket alike. The full contract is
`src/protocol.rs`; this is the part a client needs.

**Commands.** Each carries an `op` and an `id` the reply echoes. Unknown fields
are ignored; an unknown `op` is the error `unknown-op`.

| op | fields |
|---|---|
| `config` | `agent`: bool, `files`: `"off"`, `"ask"` or `"trusted"`, `receiveDir`: string |
| `snapshot` | — the daemon sends this client a fresh `state` |
| `power`, `discoverable`, `pairable`, `scan` | `adapter`: path, `on`: bool |
| `adapter-name` | `adapter`: path, `name`: string |
| `discoverable-timeout` | `adapter`: path, `seconds`: number |
| `connect`, `disconnect`, `pair`, `cancel-pair`, `remove` | `device`: path |
| `trust`, `block`, `wake` | `device`: path, `on`: bool |
| `rename` | `device`: path, `name`: string; empty gives back the device's own name |
| `send` | `device`: path, `files`: absolute paths |
| `pick` | `device`: path — the portal's chooser, then as `send` |
| `cancel`, `dismiss` | `transfer`: id |
| `answer` | `prompt`: number, `accept`: bool, and `value`, the PIN or the passkey's digits, when one was typed |

**Events.**

| event | what it carries |
|---|---|
| `state` | `version` (1), `bluez`, `agent` and `agentError`, `obex` and `obexError`, `files`, `receiveDir`, and the arrays `adapters`, `devices`, `transfers` and `prompts`. Every key on every object is present: an absent number is `null`, an absent string `""` |
| `reply` | `id`, `op`, `ok`, and on failure `error` and `message`. `pair`, and `connect` on a device that is not paired (it pairs first), add `connected`, and `connectError` and `connectMessage` when the connection failed — such a reply is `ok` when `Pair` worked, so a client reads `connected` as well; `send` and `pick` reply when the batch ends, with `transfers`, `sent` and `failed`, and `pick` with `cancelled` when the chooser was closed. A batch stops at the first `SendFile` obexd refuses, and `failed` counts every file that did not go |
| `notice` | `kind` — `received`, `sent` or `failed` — with `file`, `device`, `name`, and `path` for a received file or `direction` (`in` or `out`) and `message` for a failure. Sent to every client. A `failed` is sent for each transfer that fails; for a `SendFile` obexd refuses, unless the batch has already had a failure, since the batch then stops; once for the files a stopped batch never offered, with `file` saying how many (`3 more files`); for a push refused because the receive folder cannot be used; and for a `send` or `pick` that ends before any file is offered, apart from the chooser being closed; `file` is then `""` when the batch named more than one, which the shell words as "a file" |
| `fatal` | `message`, just before the daemon exits 1 |

`agent` and `obex` are `ready`, `off` (switched off by `config`), `absent` (the
service is not on the bus), `busy` (another agent holds the slot) or `error`,
with the reason in `agentError` or `obexError`. A transfer's `status` is
`queued`, `active`, `suspended`, `complete`, `error` or `cancelled`, the last set
by the daemon when its own `cancel` worked. A device's `busy` is `""` or the
command in flight. A failed reply carries the D-Bus error's name in `error` and
its message in `message`, which is where BlueZ puts a `br-connection-*` reason;
the daemon's own are `bad-request`, `no-such-device`, `no-such-adapter`,
`no-such-prompt`, `no-such-transfer`, `bluez-absent`, `obex-absent`,
`portal-unavailable`, `file-unreadable` and `receive-dir`.

**The verbs:**

```text
jayos-bluetooth daemon                     the manager; its owner is on stdin and stdout
jayos-bluetooth watch                      stdin to the daemon, its events to stdout
jayos-bluetooth status                     the current state, once
jayos-bluetooth send <device> <file>...    Object Push; <device> is an address or a path
```

`watch`, `status` and `send` exit 2 with `{"ok":false,"error":"..."}` when no
daemon answers, so a script can tell "not running" from `send`'s 1, a file that
did not arrive. The page does not look for 2: it takes any end of `watch` as
the daemon gone and shows the envelope's `error` as the reason, and only exit
127, or a `watch` that never started, reads as not installed. A refusal never
reaches the page as an exit status; it is a `reply` with `ok` false.
JayOS's `docs/HELPERS.md` lists them with its other helpers.


## Testing

```bash
tools/check.sh                    # all of it: syntax, the contract, cargo, every suite
cargo test --locked               # 105: 77 unit, 28 against a mock BlueZ and obexd
tests/shell/test-manager.sh       # 35: the binary, with no bus and on an empty one
tests/shell/test-packaging.sh     # the version, steps.sh package against contract/files.txt
tests/shell/test-release.sh       # tools/release.sh and tools/publish-release.sh
tools/contract.sh                 # contract/ against the code
```

- **`cargo test`** needs `dbus-daemon` for the daemon tests; without it those
  that need a bus print `skip: dbus-daemon not found` and return, and the ones
  that need none still run. No test reaches the machine's own buses.
- **`tests/shell/test-manager.sh`** exits 2 when the binary is not built, and
  counts a skip for a binary older than its source, for no `dbus-daemon` and
  for no `jq`. `JAYOS_BLUETOOTH_BIN` names the binary: `tools/check.sh` hands
  it the debug build it has just made, and `packaging/steps.sh check` the
  release build. What it cannot check, it names at the end: pairing,
  connecting and files, which are the crate's against mocks and nobody's
  against an adapter.
- **JayOS's suites** hold its side: its Bluetooth page, popup, service and
  prompt card under Qt, its model, its installer's `main.conf` step and its
  doctor -- against the copy of `contract/` and `share/` it vendors from the
  release it pins.

## Known gaps

- **Nothing has met a real adapter, bluetoothd or obexd.** The mocks
  implement what the daemon calls and what BlueZ and obexd call back, as
  their documentation and source describe it.
- **Transfers obexd started before the daemon came up are not tracked**, and
  a send batch waits for as long as obexd stays on the bus without signalling
  a transfer's end. A batch whose obexd leaves the bus ends at once.
- **A display prompt for a device removed mid-pairing** stays up until its 60
  seconds run out.
- **The 65-second backstop and the 55-second push deadline** are unit-tested
  and not in the integration test, which would wait out both:
  `requests_time_out_as_rejections_after_bluez_gives_up` and
  `a_push_is_refused_before_obexd_would_take_its_silence_as_a_yes` in
  `prompts::tests`. The `Source` argument for a machine with two adapters is
  held both ways: `a_send_names_its_adapter_only_when_there_are_two` in
  `model::tests` chooses it, and
  `a_send_on_a_machine_with_two_adapters_names_the_devices_own` sees it reach
  CreateSession.
- **Two of the review's checks are not held by a test of their own**: the
  portal match rule's sender and the bus driver as the only source of a change
  of owner. Each survived its mutation because another check stops the same
  forgery, and each is kept as defence in depth.
- **A reload can meet the old daemon still holding the lock.** The new one
  waits five seconds for it and JayOS's service ten more after a fatal, which
  is the read answer; whether a reload ends the old generation's process
  inside those five seconds was not observed, because no Quickshell ran here.
