mod access_point;
mod link;
mod manager;
mod profile;
mod wifi;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::dbus::Signal;
use crate::services::worker;
use crate::{Bus, Service, Value};

pub use access_point::AccessPoint;
pub use link::Link;

#[derive(Default, Clone, PartialEq)]
pub struct Network {
    link: Link,

    // empty on a wired link
    ssid: String,

    // 0 to 100, and 0 on a wired link
    strength: u8,

    wifi_enabled: bool,

    // false while connected to a network that doesn't reach the internet
    internet: bool,

    // joining a wifi network that isn't up yet
    connecting: bool,

    // empty without a wifi device, or with the radio off
    access_points: Vec<AccessPoint>,
}

// networkmanager's own check only runs every five minutes
const RECHECK: Duration = Duration::from_secs(20);

// changes nothing here shows, which some cards send every few seconds
const UNSHOWN: [&str; 2] = ["Bitrate", "LastSeen"];

// a burst of changes, like joining a network, is fetched once it has been quiet this long, or after a second at most
const SETTLE: Duration = Duration::from_millis(200);
const SETTLE_AT_MOST: Duration = Duration::from_secs(1);

// how long a lost bus is left before the changes are watched again
const REWATCH: Duration = Duration::from_secs(5);

// one access point networkmanager sees, hidden ones included, by its object path
#[derive(Debug, Clone)]
struct Radio {
    path: String,
    ssid: String,
    strength: u8,
}

// the access points behind a fetch and the one joined, so a change of strength alone needs no new fetch
#[derive(Debug, Clone, Default)]
struct Radios {
    seen: Vec<Radio>,
    joined: String,
}

// what a signal from networkmanager changed
#[derive(Debug, PartialEq)]
enum Change {
    // an access point's strength and nothing else, which comes every few seconds
    Strength(String, u8),

    Other,
}

const ACCESS_POINT: &str = "org.freedesktop.NetworkManager.AccessPoint";

/*
 * fetched again when networkmanager announces a change, so an idle
 * connection costs nothing. a change of strength alone, which comes
 * every few seconds, is taken from the signal instead
 */
impl Service for Network {
    fn new() -> Self {
        fetch().0
    }

    /*
     * networkmanager is asked outside the lock, so a slow answer never
     * holds up a frame, and nothing is written when nothing changed
     */
    fn listen() {
        let mut changes = watch();
        let mut checked: Option<Instant> = None;

        let mut radios = Radios::default();

        // what changed since the last round; the first one fetches
        let mut burst = vec![Change::Other];

        loop {
            let applied = with_strengths(&Self::read(), &radios, &burst);

            let fresh = match applied {
                Some((network, updated)) => {
                    radios = updated;

                    network
                }

                None => {
                    let (network, found) = fetch();

                    radios = found;

                    network
                }
            };

            burst.clear();

            let without_internet = fresh.connected() && !fresh.internet;

            if without_internet && checked.is_none_or(|at| at.elapsed() >= RECHECK) {
                checked = Some(Instant::now());

                worker::run(manager::check_connectivity);
            }

            if *Self::read() != fresh {
                *Self::write() = fresh;
            }

            // without internet the check above comes round again, even when nothing changes
            let next = if without_internet {
                changes.recv_timeout(RECHECK)
            } else {
                changes.recv().map_err(|_| RecvTimeoutError::Disconnected)
            };

            match next {
                Ok(change) => {
                    burst.push(change);

                    settle(&changes, &mut burst);
                }

                // a recheck that timed out has no burst to wait out
                Err(RecvTimeoutError::Timeout) => burst.push(Change::Other),

                Err(RecvTimeoutError::Disconnected) => {
                    thread::sleep(REWATCH);

                    changes = watch();

                    burst.push(Change::Other);
                }
            }
        }
    }
}

impl Network {
    pub fn connected(&self) -> bool {
        self.link != Link::Offline
    }

    pub fn link(&self) -> Link {
        self.link
    }

    pub fn ssid(&self) -> &str {
        &self.ssid
    }

    pub fn strength(&self) -> u8 {
        self.strength
    }

    pub fn wifi_enabled(&self) -> bool {
        self.wifi_enabled
    }

    // reaching the internet, by networkmanager's own check
    pub fn internet(&self) -> bool {
        self.internet
    }

    pub fn connecting(&self) -> bool {
        self.connecting
    }

    pub fn access_points(&self) -> &[AccessPoint] {
        &self.access_points
    }
}

/*
 * these talk to networkmanager right away, and the
 * next update shows what changed
 */
impl Network {
    // new networks show up in access_points a few seconds later
    pub fn scan() {
        worker::run(|| {
            let Some(device) = wifi::device() else {
                return;
            };

            wifi::scan(&device);
        });
    }

    pub fn connect(ssid: &str, password: Option<&str>) {
        let ssid = String::from(ssid);
        let password = password.map(String::from);

        worker::run(move || join(&ssid, password.as_deref()));
    }

    pub fn disconnect() {
        worker::run(|| {
            let Some(device) = wifi::device() else {
                return;
            };

            wifi::disconnect(&device);
        });
    }

    pub fn set_wifi(enabled: bool) {
        worker::run(move || manager::set_wifi(enabled));
    }

    // drops the saved profile and its password, so joining asks again
    pub fn forget(ssid: &str) {
        let ssid = String::from(ssid);

        worker::run(move || profile::forget(&ssid));
    }
}

/*
 * a saved profile already holds its password, so it is joined as it is;
 * otherwise networkmanager saves a new one, password included
 */
fn join(ssid: &str, password: Option<&str>) {
    let Some(device) = wifi::device() else {
        return;
    };

    if let Some(profile) = profile::find(ssid) {
        manager::activate(&profile, &device);

        return;
    }

    let (access_points, _) = wifi::access_points(&device);

    let mut same_name = access_points
        .iter()
        .filter(|access_point| access_point.ssid == ssid);

    // a password alone can't make a profile for a network that also asks for a username
    if same_name.clone().any(AccessPoint::enterprise) {
        return;
    }

    let wpa3 = same_name.any(|access_point| access_point.wpa3_only);

    let Some(profile) = profile::add(profile::settings(ssid, password, wpa3)) else {
        return;
    };

    manager::activate(&profile, &device);
}

// networkmanager's changes, watched before the first fetch so none slips past; ends with the bus
fn watch() -> Receiver<Change> {
    // every program sends PropertiesChanged, so the bus is asked for networkmanager's only
    let signals = Bus::system().signals_from(
        manager::NAME,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
    );

    let (sender, changes) = mpsc::channel();

    thread::spawn(move || {
        for signal in signals {
            if shown(&signal) && sender.send(change(&signal)).is_err() {
                return;
            }
        }
    });

    changes
}

// a shown change that is an access point's strength alone, or anything else
fn change(signal: &Signal) -> Change {
    let [interface, Value::Map(changed), invalidated] = signal.arguments() else {
        return Change::Other;
    };

    let mut names = changed
        .keys()
        .filter(|name| !UNSHOWN.contains(&name.as_str()));

    let only_strength =
        names.next().is_some_and(|name| name == "Strength") && names.next().is_none();

    match changed.get("Strength") {
        Some(Value::Number(strength))
            if only_strength
                && interface.text() == ACCESS_POINT
                && invalidated.list().is_empty() =>
        {
            Change::Strength(String::from(signal.path()), *strength as u8)
        }
        _ => Change::Other,
    }
}

/*
 * the network as shown with a burst of strength changes applied, the best
 * access point of each name standing for it as a fetch has it. nothing
 * when the burst holds any other change or an access point not seen yet
 */
fn with_strengths(
    current: &Network,
    radios: &Radios,
    burst: &[Change],
) -> Option<(Network, Radios)> {
    let known = |path: &str| radios.seen.iter().any(|radio| radio.path == path);

    // checked before anything is copied, as a burst with any other change fetches instead
    let applies = burst
        .iter()
        .all(|change| matches!(change, Change::Strength(path, _) if known(path)));

    if !applies {
        return None;
    }

    let mut network = current.clone();
    let mut radios = radios.clone();

    for change in burst {
        let Change::Strength(path, strength) = change else {
            return None;
        };

        let radio = radios.seen.iter_mut().find(|radio| radio.path == *path)?;

        radio.strength = *strength;

        if *path == radios.joined {
            network.strength = *strength;
        }
    }

    for access_point in &mut network.access_points {
        let best = radios
            .seen
            .iter()
            .filter(|radio| radio.ssid == access_point.ssid)
            .map(|radio| radio.strength)
            .max();

        if let Some(best) = best {
            access_point.strength = best;
        }
    }

    network
        .access_points
        .sort_by_key(|access_point| std::cmp::Reverse(access_point.strength));

    Some((network, radios))
}

// the arguments are the interface, the changed properties and the invalidated ones
fn shown(signal: &Signal) -> bool {
    match signal.arguments().get(1) {
        Some(Value::Map(changed)) if !changed.is_empty() => {
            changed.keys().any(|name| !UNSHOWN.contains(&name.as_str()))
        }
        _ => true,
    }
}

fn settle(changes: &Receiver<Change>, burst: &mut Vec<Change>) {
    let started = Instant::now();

    while started.elapsed() < SETTLE_AT_MOST
        && let Ok(change) = changes.recv_timeout(SETTLE)
    {
        burst.push(change);
    }
}

// the ssid comes as raw bytes, and is almost always utf-8
fn ssid(bytes: &Value) -> String {
    let mut raw = Vec::new();

    for byte in bytes.list() {
        raw.push(byte.number() as u8);
    }

    String::from_utf8_lossy(&raw).into_owned()
}

// asks networkmanager for everything shown, from scratch
fn fetch() -> (Network, Radios) {
    // networks to join are listed even while offline
    let mut network = Network {
        wifi_enabled: manager::wifi_enabled(),
        ..Network::default()
    };

    let mut radios = Radios::default();

    let device = wifi::device();

    if let Some(device) = &device {
        (network.access_points, radios.seen) = wifi::access_points(device);
        network.connecting = wifi::connecting(device);
    }

    let state = manager::state();

    if !manager::connected(state) {
        return (network, radios);
    }

    network.internet = manager::online(state);

    let mut connection = manager::primary_connection();

    // without a default route networkmanager names no primary connection, so the wifi device's own says which it is
    if connection.get("Type").text().is_empty()
        && let Some(device) = &device
    {
        connection = wifi::connection(device);
    }

    network.link = Link::from_type(connection.get("Type").text());

    if network.link != Link::Wifi {
        return (network, radios);
    }

    let joined = connection.get("SpecificObject").text();

    let access_point = manager::access_point(joined);

    network.ssid = ssid(access_point.get("Ssid"));
    network.strength = access_point.get("Strength").number() as u8;

    radios.joined = String::from(joined);

    (network, radios)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        ACCESS_POINT, AccessPoint, Change, Link, Network, Radio, Radios, change, shown,
        with_strengths,
    };
    use crate::Value;
    use crate::dbus::Signal;

    const HOME_NEAR: &str = "/org/freedesktop/NetworkManager/AccessPoint/1";
    const HOME_FAR: &str = "/org/freedesktop/NetworkManager/AccessPoint/2";
    const CAFE: &str = "/org/freedesktop/NetworkManager/AccessPoint/3";
    const HIDDEN: &str = "/org/freedesktop/NetworkManager/AccessPoint/4";

    fn access_point_changed(
        interface: &str,
        properties: &[(&str, Value)],
        invalidated: &[&str],
    ) -> Signal {
        let properties = properties
            .iter()
            .map(|(name, value)| (String::from(*name), value.clone()))
            .collect::<BTreeMap<_, _>>();

        Signal {
            sender: String::from(":1.4"),
            path: String::from(HOME_NEAR),
            arguments: vec![
                Value::Text(String::from(interface)),
                Value::Map(properties),
                Value::List(
                    invalidated
                        .iter()
                        .map(|name| Value::Text(String::from(*name)))
                        .collect(),
                ),
            ],
        }
    }

    fn listed(ssid: &str, strength: u8, active: bool) -> AccessPoint {
        AccessPoint {
            ssid: String::from(ssid),
            strength,
            secured: true,
            active,
            saved: active,
            enterprise: false,
            wpa3_only: false,
        }
    }

    fn radio(path: &str, ssid: &str, strength: u8) -> Radio {
        Radio {
            path: String::from(path),
            ssid: String::from(ssid),
            strength,
        }
    }

    // joined to the nearer of two access points of one network, with a cafe and a hidden network in sight
    fn joined() -> (Network, Radios) {
        let network = Network {
            link: Link::Wifi,
            ssid: String::from("home"),
            strength: 70,
            wifi_enabled: true,
            internet: true,
            connecting: false,
            access_points: vec![listed("home", 70, true), listed("cafe", 55, false)],
        };

        let radios = Radios {
            seen: vec![
                radio(HOME_NEAR, "home", 70),
                radio(HOME_FAR, "home", 40),
                radio(CAFE, "cafe", 55),
                radio(HIDDEN, "", 20),
            ],
            joined: String::from(HOME_NEAR),
        };

        (network, radios)
    }

    #[test]
    fn a_change_of_strength_alone_is_read_from_the_signal() {
        let strength = Value::Number(61.0);

        let read = change(&access_point_changed(
            ACCESS_POINT,
            &[
                ("Strength", strength.clone()),
                ("LastSeen", Value::Number(3.0)),
            ],
            &[],
        ));

        assert_eq!(read, Change::Strength(String::from(HOME_NEAR), 61));

        // anything more, from another kind of object, or with properties invalidated is fetched
        let more = [
            ("Strength", strength.clone()),
            ("Ssid", Value::List(Vec::new())),
        ];

        assert_eq!(
            change(&access_point_changed(ACCESS_POINT, &more, &[])),
            Change::Other
        );

        let device = "org.freedesktop.NetworkManager.Device.Wireless";

        assert_eq!(
            change(&access_point_changed(
                device,
                &[("Strength", strength.clone())],
                &[]
            )),
            Change::Other
        );
        assert_eq!(
            change(&access_point_changed(
                ACCESS_POINT,
                &[("Strength", strength)],
                &["Flags"]
            )),
            Change::Other
        );
    }

    #[test]
    fn strengths_apply_as_a_fetch_would_show_them() {
        let (network, radios) = joined();

        let burst = [Change::Strength(String::from(HOME_NEAR), 30)];

        let (applied, updated) =
            with_strengths(&network, &radios, &burst).expect("failed to apply a strength");

        // the joined access point's own strength, while the list shows the network's best
        assert_eq!(applied.strength, 30);
        assert_eq!(
            applied.access_points,
            [listed("cafe", 55, false), listed("home", 40, true)]
        );
        assert_eq!(updated.seen[0].strength, 30);

        // a hidden network changes nothing shown, and needs no fetch either
        let hidden = [Change::Strength(String::from(HIDDEN), 10)];

        let (unchanged, _) =
            with_strengths(&network, &radios, &hidden).expect("failed to apply a hidden strength");

        assert!(unchanged == network);
    }

    #[test]
    fn a_new_access_point_or_any_other_change_fetches() {
        let (network, radios) = joined();

        let unknown = [Change::Strength(
            String::from("/org/freedesktop/NetworkManager/AccessPoint/9"),
            50,
        )];
        let mixed = [Change::Strength(String::from(CAFE), 50), Change::Other];

        assert!(with_strengths(&network, &radios, &unknown).is_none());
        assert!(with_strengths(&network, &radios, &mixed).is_none());
    }

    fn changed(names: &[&str]) -> Signal {
        let properties = names
            .iter()
            .map(|name| (String::from(*name), Value::Nothing))
            .collect::<BTreeMap<_, _>>();

        Signal {
            sender: String::from(":1.4"),
            path: String::from("/org/freedesktop/NetworkManager/Devices/3"),
            arguments: vec![
                Value::Text(String::from(
                    "org.freedesktop.NetworkManager.Device.Wireless",
                )),
                Value::Map(properties),
                Value::List(Vec::new()),
            ],
        }
    }

    #[test]
    fn a_change_nothing_shows_is_left_alone() {
        assert!(!shown(&changed(&["Bitrate"])));
        assert!(!shown(&changed(&["Bitrate", "LastSeen"])));
    }

    #[test]
    fn a_change_to_anything_shown_fetches_again() {
        assert!(shown(&changed(&["Strength"])));
        assert!(shown(&changed(&["Bitrate", "State"])));

        // only invalidated properties, which say nothing about what changed
        assert!(shown(&changed(&[])));
    }
}
