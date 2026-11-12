//! Everything BlueZ (or obexd) has said, and the typed `state` built from it.
//!
//! The model is a mirror of the remote ObjectManager: path, interface,
//! property. It is filled by `GetManagedObjects` and kept current by
//! `InterfacesAdded`, `InterfacesRemoved` and `PropertiesChanged`, and the
//! snapshot is rebuilt from it every time -- so a device's row cannot hold a
//! value from before a refresh, which is how overskride's list went stale
//! (overskride 0.6.5, src/bluetooth/device.rs:326 skips re-subscribing a
//! device it already listed; read, not run).
//!
//! Values are kept as a small enum of their own rather than as
//! `zvariant::OwnedValue`, which cannot be cloned infallibly (it may hold a
//! file descriptor) and compares awkwardly. Nothing BlueZ publishes on the
//! interfaces read here needs more than these shapes.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};
use zbus::zvariant::{OwnedValue, Value as ZValue};

pub const ADAPTER: &str = "org.bluez.Adapter1";
pub const DEVICE: &str = "org.bluez.Device1";
pub const BATTERY: &str = "org.bluez.Battery1";
/// An A2DP or LE Audio stream between an adapter and a device. BlueZ puts it
/// under the device's path (`/org/bluez/hci0/dev_.../sep1/fd0`), so the
/// adapter it uses is the path's own prefix.
pub const TRANSPORT: &str = "org.bluez.MediaTransport1";

/// A property value, reduced to what a snapshot can show.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Bool(bool),
    Int(i64),
    Str(String),
    /// An array of strings or object paths: `UUIDs`.
    List(Vec<String>),
    /// The integer keys of a dictionary: `ManufacturerData` is
    /// `a{qv}` keyed by company identifier, and the keys are what is shown.
    Keys(Vec<u64>),
    /// Anything else: kept so the property counts as present.
    Other,
}

impl Val {
    pub fn from_value(v: &ZValue<'_>) -> Val {
        match v {
            ZValue::Bool(b) => Val::Bool(*b),
            ZValue::U8(n) => Val::Int(i64::from(*n)),
            ZValue::I16(n) => Val::Int(i64::from(*n)),
            ZValue::U16(n) => Val::Int(i64::from(*n)),
            ZValue::I32(n) => Val::Int(i64::from(*n)),
            ZValue::U32(n) => Val::Int(i64::from(*n)),
            ZValue::I64(n) => Val::Int(*n),
            // u64 above i64::MAX is not a value BlueZ or obexd sends (sizes and
            // byte counts); saturating is better than wrapping negative.
            ZValue::U64(n) => Val::Int(i64::try_from(*n).unwrap_or(i64::MAX)),
            ZValue::Str(s) => Val::Str(s.as_str().to_string()),
            ZValue::ObjectPath(p) => Val::Str(p.as_str().to_string()),
            ZValue::Value(inner) => Val::from_value(inner),
            ZValue::Array(a) => {
                let mut out = Vec::new();
                for item in a.inner() {
                    match item {
                        ZValue::Str(s) => out.push(s.as_str().to_string()),
                        ZValue::ObjectPath(p) => out.push(p.as_str().to_string()),
                        _ => return Val::Other,
                    }
                }
                Val::List(out)
            }
            ZValue::Dict(d) => {
                let mut keys = Vec::new();
                for (k, _) in d.iter() {
                    match Val::from_value(k) {
                        Val::Int(n) if n >= 0 => keys.push(n as u64),
                        _ => return Val::Other,
                    }
                }
                keys.sort_unstable();
                keys.dedup();
                Val::Keys(keys)
            }
            _ => Val::Other,
        }
    }
}

pub type Props = BTreeMap<String, Val>;
pub type Ifaces = BTreeMap<String, Props>;

/// Convert a D-Bus property map.
pub fn props_from(map: &HashMap<String, OwnedValue>) -> Props {
    map.iter().map(|(k, v)| (k.clone(), Val::from_value(v))).collect()
}

/// The remote object tree.
#[derive(Debug, Default, Clone)]
pub struct Model {
    objects: BTreeMap<String, Ifaces>,
}

impl Model {
    pub fn clear(&mut self) {
        self.objects.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// The answer to `GetManagedObjects`: everything, replacing what was here.
    pub fn replace(&mut self, objects: BTreeMap<String, Ifaces>) {
        self.objects = objects;
    }

    /// `InterfacesAdded`. An interface that was already present is replaced
    /// whole, which is what the signal means.
    pub fn add(&mut self, path: &str, ifaces: Ifaces) {
        let entry = self.objects.entry(path.to_string()).or_default();
        for (name, props) in ifaces {
            entry.insert(name, props);
        }
    }

    /// `InterfacesRemoved`. The object goes once it has no interfaces left.
    pub fn remove(&mut self, path: &str, ifaces: &[String]) {
        if let Some(entry) = self.objects.get_mut(path) {
            for name in ifaces {
                entry.remove(name);
            }
            if entry.is_empty() {
                self.objects.remove(path);
            }
        }
    }

    /// `PropertiesChanged`. Applied only to an interface the model already
    /// holds: a signal for an object that was just removed would otherwise
    /// resurrect it as a row with no address.
    ///
    /// An invalidated property is REMOVED, not kept: BlueZ invalidates RSSI
    /// and TxPower when discovery stops, and the last reading shown as current
    /// is a signal-strength bar for a device that may have left the room.
    pub fn changed(
        &mut self,
        path: &str,
        iface: &str,
        changed: Props,
        invalidated: &[String],
    ) -> bool {
        let Some(props) = self.objects.get_mut(path).and_then(|o| o.get_mut(iface)) else {
            return false;
        };
        for (k, v) in changed {
            props.insert(k, v);
        }
        for k in invalidated {
            props.remove(k);
        }
        true
    }

    pub fn props(&self, path: &str, iface: &str) -> Option<&Props> {
        self.objects.get(path).and_then(|o| o.get(iface))
    }

    pub fn has(&self, path: &str, iface: &str) -> bool {
        self.props(path, iface).is_some()
    }

    /// Every path carrying `iface`, in path order -- which is the order the
    /// snapshot lists them in, so two snapshots of one tree are identical.
    pub fn paths_with<'a>(&'a self, iface: &'a str) -> impl Iterator<Item = &'a String> + 'a {
        self.objects.iter().filter(move |(_, i)| i.contains_key(iface)).map(|(p, _)| p)
    }

    pub fn str(&self, path: &str, iface: &str, key: &str) -> Option<&str> {
        match self.props(path, iface)?.get(key)? {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn bool(&self, path: &str, iface: &str, key: &str) -> Option<bool> {
        match self.props(path, iface)?.get(key)? {
            Val::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn int(&self, path: &str, iface: &str, key: &str) -> Option<i64> {
        match self.props(path, iface)?.get(key)? {
            Val::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// The adapter a device belongs to: its `Adapter` property, or failing
    /// that the parent of its path, which is where BlueZ puts every device.
    pub fn device_adapter(&self, device: &str) -> String {
        if let Some(a) = self.str(device, DEVICE, "Adapter") {
            return a.to_string();
        }
        device.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default()
    }

    /// Find a device by its address, case-insensitively. With several
    /// adapters the same remote address can appear under each; the one under
    /// the adapter whose address is `source` wins, which is how an incoming
    /// push is matched to the adapter it arrived on.
    pub fn device_by_address(&self, address: &str, source: Option<&str>) -> Option<String> {
        let mut found: Option<String> = None;
        for path in self.paths_with(DEVICE) {
            let Some(a) = self.str(path, DEVICE, "Address") else { continue };
            if !a.eq_ignore_ascii_case(address) {
                continue;
            }
            if let Some(src) = source {
                let adapter = self.device_adapter(path);
                if self
                    .str(&adapter, ADAPTER, "Address")
                    .is_some_and(|s| s.eq_ignore_ascii_case(src))
                {
                    return Some(path.clone());
                }
            }
            found.get_or_insert_with(|| path.clone());
        }
        found
    }

    pub fn adapter_count(&self) -> usize {
        self.paths_with(ADAPTER).count()
    }

    /// The `Source` a send to `device` names: the address of the adapter it
    /// is under, so the file leaves through the radio the device is paired
    /// with. Only when there is a choice: with one adapter obexd picks it,
    /// and naming it would fail the day the adapter's address changes.
    pub fn send_source(&self, device: &str) -> Option<String> {
        if self.adapter_count() < 2 {
            return None;
        }
        self.str(&self.device_adapter(device), ADAPTER, "Address").map(str::to_string)
    }

    /// Adapters, in the shape the protocol's `adapters` array takes.
    /// `scanning` is whether any client of this daemon wants discovery on
    /// that adapter, which BlueZ's own `Discovering` cannot say: that is true
    /// whenever anybody on the system is scanning.
    ///
    /// `audioPaused` is whether this daemon is holding that adapter's
    /// discovery back because audio is streaming over it
    /// (`Model::streaming_on`), so a page can say why a scan it asked for
    /// finds nothing new rather than look broken.
    pub fn adapters_json(
        &self,
        scanning: impl Fn(&str) -> bool,
        audio_paused: impl Fn(&str) -> bool,
    ) -> Vec<Value> {
        self.paths_with(ADAPTER)
            .map(|path| {
                let s = |k: &str| self.str(path, ADAPTER, k).unwrap_or("").to_string();
                let b = |k: &str| self.bool(path, ADAPTER, k).unwrap_or(false);
                let n = |k: &str| self.int(path, ADAPTER, k);
                json!({
                    "path": path,
                    "id": path.rsplit('/').next().unwrap_or(""),
                    "address": s("Address"),
                    "name": s("Alias"),
                    "systemName": s("Name"),
                    "powered": b("Powered"),
                    "powerState": s("PowerState"),
                    "discoverable": b("Discoverable"),
                    "discoverableTimeout": n("DiscoverableTimeout"),
                    "pairable": b("Pairable"),
                    "pairableTimeout": n("PairableTimeout"),
                    "discovering": b("Discovering"),
                    "scanning": scanning(path),
                    "audioPaused": audio_paused(path),
                })
            })
            .collect()
    }

    /// Whether audio is streaming between `adapter` and any device: a
    /// MediaTransport1 under it whose State is `active`, or `pending`, the
    /// moment before it is -- BlueZ's two states for a transport that has
    /// been or is being acquired (doc/org.bluez.MediaTransport.rst). `idle`
    /// is a transport that exists and carries nothing: a headset connected
    /// with nothing playing, which discovery does not hurt.
    ///
    /// By path rather than by the transport's `Device` property: every
    /// transport BlueZ makes lives under its device, and the device under its
    /// adapter, so the prefix answers without a second lookup that a
    /// transport added before its device's properties arrived could miss.
    pub fn streaming_on(&self, adapter: &str) -> bool {
        let under = format!("{adapter}/");
        self.paths_with(TRANSPORT).any(|p| {
            p.starts_with(&under)
                && matches!(self.str(p, TRANSPORT, "State"), Some("active" | "pending"))
        })
    }

    /// Devices, in the shape the protocol's `devices` array takes. Every key
    /// is present on every device: a number BlueZ did not report is `null`, a
    /// string is `""`, a list is `[]`.
    pub fn devices_json(&self, busy: impl Fn(&str) -> String) -> Vec<Value> {
        self.paths_with(DEVICE)
            .map(|path| {
                let s = |k: &str| self.str(path, DEVICE, k).unwrap_or("").to_string();
                let b = |k: &str| self.bool(path, DEVICE, k).unwrap_or(false);
                let n = |k: &str| self.int(path, DEVICE, k);
                let props = self.props(path, DEVICE);
                let uuids = match props.and_then(|p| p.get("UUIDs")) {
                    Some(Val::List(l)) => l.clone(),
                    _ => Vec::new(),
                };
                let manufacturers = match props.and_then(|p| p.get("ManufacturerData")) {
                    Some(Val::Keys(k)) => k.clone(),
                    _ => Vec::new(),
                };
                json!({
                    "path": path,
                    "adapter": self.device_adapter(path),
                    "address": s("Address"),
                    "addressType": s("AddressType"),
                    "name": s("Alias"),
                    "deviceName": s("Name"),
                    "icon": s("Icon"),
                    "class": n("Class"),
                    "appearance": n("Appearance"),
                    "paired": b("Paired"),
                    "bonded": b("Bonded"),
                    "trusted": b("Trusted"),
                    "blocked": b("Blocked"),
                    "connected": b("Connected"),
                    "servicesResolved": b("ServicesResolved"),
                    "legacyPairing": b("LegacyPairing"),
                    "wakeAllowed": b("WakeAllowed"),
                    "rssi": n("RSSI"),
                    "txPower": n("TxPower"),
                    "battery": self.int(path, BATTERY, "Percentage"),
                    "uuids": uuids,
                    "modalias": s("Modalias"),
                    "manufacturers": manufacturers,
                    "busy": busy(path),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Dict, ObjectPath, Signature};

    fn props(pairs: &[(&str, Val)]) -> Props {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn tree() -> Model {
        let mut m = Model::default();
        let mut objs = BTreeMap::new();
        objs.insert(
            "/org/bluez/hci0".to_string(),
            Ifaces::from([(
                ADAPTER.to_string(),
                props(&[
                    ("Address", Val::Str("00:11:22:33:44:55".into())),
                    ("Alias", Val::Str("laptop".into())),
                    ("Name", Val::Str("archbox".into())),
                    ("Powered", Val::Bool(true)),
                    ("PowerState", Val::Str("on".into())),
                    ("DiscoverableTimeout", Val::Int(180)),
                ]),
            )]),
        );
        objs.insert(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".to_string(),
            Ifaces::from([
                (
                    DEVICE.to_string(),
                    props(&[
                        ("Address", Val::Str("AA:BB:CC:DD:EE:FF".into())),
                        ("Alias", Val::Str("WH-1000XM5".into())),
                        ("Name", Val::Str("WH-1000XM5".into())),
                        ("Paired", Val::Bool(true)),
                        ("RSSI", Val::Int(-58)),
                        ("Class", Val::Int(2360324)),
                        ("UUIDs", Val::List(vec!["0000110b-0000-1000-8000-00805f9b34fb".into()])),
                        ("ManufacturerData", Val::Keys(vec![301])),
                    ]),
                ),
                (BATTERY.to_string(), props(&[("Percentage", Val::Int(80))])),
            ]),
        );
        objs.insert(
            "/org/bluez/hci0/dev_11_22_33_44_55_66".to_string(),
            Ifaces::from([(
                DEVICE.to_string(),
                props(&[
                    ("Address", Val::Str("11:22:33:44:55:66".into())),
                    ("Alias", Val::Str("11-22-33-44-55-66".into())),
                ]),
            )]),
        );
        m.replace(objs);
        m
    }

    #[test]
    fn values_reduce_to_what_a_snapshot_shows() {
        assert_eq!(Val::from_value(&ZValue::I16(-70)), Val::Int(-70));
        assert_eq!(Val::from_value(&ZValue::U32(0x240404)), Val::Int(0x240404));
        assert_eq!(Val::from_value(&ZValue::U8(80)), Val::Int(80));
        assert_eq!(Val::from_value(&ZValue::U64(u64::MAX)), Val::Int(i64::MAX));
        assert_eq!(Val::from_value(&ZValue::from("x")), Val::Str("x".into()));
        let p = ObjectPath::try_from("/org/bluez/hci0").unwrap();
        assert_eq!(Val::from_value(&ZValue::from(p)), Val::Str("/org/bluez/hci0".into()));
        let a = Array::from(vec!["a", "b"]);
        assert_eq!(Val::from_value(&ZValue::from(a)), Val::List(vec!["a".into(), "b".into()]));
        let mut d = Dict::new(&Signature::U16, &Signature::Variant);
        d.add(0x004cu16, ZValue::from(vec![1u8, 2])).unwrap();
        d.add(0x0006u16, ZValue::from(vec![3u8])).unwrap();
        assert_eq!(Val::from_value(&ZValue::from(d)), Val::Keys(vec![6, 0x4c]));
        assert_eq!(Val::from_value(&ZValue::new(ZValue::Bool(true))), Val::Bool(true));
        assert_eq!(Val::from_value(&ZValue::F64(1.0)), Val::Other);
    }

    #[test]
    fn every_device_key_is_present_with_nulls_for_what_bluez_did_not_say() {
        let m = tree();
        let devs = m.devices_json(|_| String::new());
        assert_eq!(devs.len(), 2);
        let keys = [
            "path",
            "adapter",
            "address",
            "addressType",
            "name",
            "deviceName",
            "icon",
            "class",
            "appearance",
            "paired",
            "bonded",
            "trusted",
            "blocked",
            "connected",
            "servicesResolved",
            "legacyPairing",
            "wakeAllowed",
            "rssi",
            "txPower",
            "battery",
            "uuids",
            "modalias",
            "manufacturers",
            "busy",
        ];
        for d in &devs {
            let obj = d.as_object().unwrap();
            assert_eq!(obj.len(), keys.len(), "{d}");
            for k in keys {
                assert!(obj.contains_key(k), "{k} missing from {d}");
            }
        }
        // Path order: dev_11.. sorts before dev_AA..
        let bare = &devs[0];
        assert_eq!(bare["deviceName"], "");
        assert_eq!(bare["rssi"], Value::Null);
        assert_eq!(bare["class"], Value::Null);
        assert_eq!(bare["battery"], Value::Null);
        assert_eq!(bare["uuids"], json!([]));
        assert_eq!(bare["paired"], false);
        assert_eq!(bare["adapter"], "/org/bluez/hci0");
        let full = &devs[1];
        assert_eq!(full["rssi"], -58);
        assert_eq!(full["battery"], 80);
        assert_eq!(full["class"], 2360324);
        assert_eq!(full["manufacturers"], json!([301]));
        assert_eq!(full["txPower"], Value::Null);
    }

    #[test]
    fn every_adapter_key_is_present() {
        let m = tree();
        let a = &m.adapters_json(|p| p == "/org/bluez/hci0", |_| false)[0];
        assert_eq!(a["id"], "hci0");
        assert_eq!(a["name"], "laptop");
        assert_eq!(a["systemName"], "archbox");
        assert_eq!(a["powered"], true);
        assert_eq!(a["discoverable"], false);
        assert_eq!(a["discoverableTimeout"], 180);
        assert_eq!(a["pairableTimeout"], Value::Null);
        assert_eq!(a["scanning"], true);
        assert_eq!(a["audioPaused"], false);
        assert_eq!(a.as_object().unwrap().len(), 14);
    }

    #[test]
    fn audio_streams_only_where_a_transport_under_the_adapter_is_acquired() {
        let mut m = tree();
        let hci0 = "/org/bluez/hci0";
        let fd = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/sep1/fd0";
        assert!(!m.streaming_on(hci0), "no transport at all");
        m.add(
            fd,
            Ifaces::from([(TRANSPORT.to_string(), props(&[("State", Val::Str("idle".into()))]))]),
        );
        assert!(!m.streaming_on(hci0), "a connected headset playing nothing");
        for state in ["pending", "active"] {
            m.changed(fd, TRANSPORT, props(&[("State", Val::Str(state.into()))]), &[]);
            assert!(m.streaming_on(hci0), "{state}");
            // hci1 is not hci0, and a prefix without its slash would say it was.
            assert!(!m.streaming_on("/org/bluez/hci"), "{state} on a prefix of the path");
            assert!(!m.streaming_on("/org/bluez/hci1"), "{state} on another adapter");
        }
        m.changed(fd, TRANSPORT, props(&[("State", Val::Str("idle".into()))]), &[]);
        assert!(!m.streaming_on(hci0), "back to idle");
        m.changed(fd, TRANSPORT, props(&[("State", Val::Str("active".into()))]), &[]);
        m.remove(fd, &[TRANSPORT.to_string()]);
        assert!(!m.streaming_on(hci0), "an active transport removed with its stream");
    }

    #[test]
    fn an_invalidated_rssi_is_removed_not_kept_stale() {
        let mut m = tree();
        let dev = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
        assert!(m.changed(dev, DEVICE, props(&[("RSSI", Val::Int(-40))]), &[]));
        assert_eq!(m.int(dev, DEVICE, "RSSI"), Some(-40));
        assert!(m.changed(dev, DEVICE, Props::new(), &["RSSI".into(), "TxPower".into()]));
        assert_eq!(m.int(dev, DEVICE, "RSSI"), None);
        assert_eq!(m.devices_json(|_| String::new())[1]["rssi"], Value::Null);
    }

    #[test]
    fn a_change_for_an_unknown_object_does_not_resurrect_it() {
        let mut m = tree();
        assert!(!m.changed(
            "/org/bluez/hci0/dev_00",
            DEVICE,
            props(&[("RSSI", Val::Int(-1))]),
            &[]
        ));
        assert!(!m.has("/org/bluez/hci0/dev_00", DEVICE));
    }

    #[test]
    fn interfaces_come_and_go_and_the_object_with_them() {
        let mut m = tree();
        let dev = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
        m.remove(dev, &[BATTERY.to_string()]);
        assert!(m.has(dev, DEVICE));
        assert_eq!(m.devices_json(|_| String::new())[1]["battery"], Value::Null);
        m.remove(dev, &[DEVICE.to_string()]);
        assert_eq!(m.paths_with(DEVICE).count(), 1);
        m.add("/org/bluez/hci1", Ifaces::from([(ADAPTER.to_string(), Props::new())]));
        assert_eq!(m.adapter_count(), 2);
    }

    #[test]
    fn a_device_is_found_by_address_preferring_the_named_adapter() {
        let mut m = tree();
        m.add(
            "/org/bluez/hci1",
            Ifaces::from([(
                ADAPTER.to_string(),
                props(&[("Address", Val::Str("66:66:66:66:66:66".into()))]),
            )]),
        );
        m.add(
            "/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF",
            Ifaces::from([(
                DEVICE.to_string(),
                props(&[("Address", Val::Str("AA:BB:CC:DD:EE:FF".into()))]),
            )]),
        );
        assert_eq!(
            m.device_by_address("aa:bb:cc:dd:ee:ff", None).as_deref(),
            Some("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF")
        );
        assert_eq!(
            m.device_by_address("AA:BB:CC:DD:EE:FF", Some("66:66:66:66:66:66")).as_deref(),
            Some("/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF")
        );
        assert_eq!(m.device_by_address("00:00:00:00:00:00", None), None);
    }

    #[test]
    fn a_send_names_its_adapter_only_when_there_are_two() {
        let mut m = tree();
        let dev = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
        assert_eq!(m.send_source(dev), None, "one adapter: obexd picks it");
        m.add(
            "/org/bluez/hci1",
            Ifaces::from([(
                ADAPTER.to_string(),
                props(&[("Address", Val::Str("66:66:66:66:66:66".into()))]),
            )]),
        );
        let other = "/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF";
        m.add(
            other,
            Ifaces::from([(
                DEVICE.to_string(),
                props(&[
                    ("Address", Val::Str("AA:BB:CC:DD:EE:FF".into())),
                    ("Adapter", Val::Str("/org/bluez/hci1".into())),
                ]),
            )]),
        );
        assert_eq!(m.send_source(dev).as_deref(), Some("00:11:22:33:44:55"));
        assert_eq!(m.send_source(other).as_deref(), Some("66:66:66:66:66:66"));
        m.remove("/org/bluez/hci1", &[ADAPTER.to_string()]);
        assert_eq!(m.send_source(dev), None, "back to one adapter");
    }

    #[test]
    fn busy_is_whatever_the_daemon_says() {
        let m = tree();
        let d =
            m.devices_json(|p| if p.ends_with("FF") { "connect".into() } else { String::new() });
        assert_eq!(d[1]["busy"], "connect");
        assert_eq!(d[0]["busy"], "");
    }
}
