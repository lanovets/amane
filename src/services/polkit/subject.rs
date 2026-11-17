use std::collections::BTreeMap;
use std::ffi::c_int;

use crate::dbus::{Argument, Bus};

use super::identities;

const LOGIN: &str = "org.freedesktop.login1";

const LOGIN_PATH: &str = "/org/freedesktop/login1";

const MANAGER: &str = "org.freedesktop.login1.Manager";

const SESSION: &str = "org.freedesktop.login1.Session";

// _SC_CLK_TCK, the clock ticks a second of process time is counted in
const CLOCK_TICKS: c_int = 2;

#[link(name = "c")]
unsafe extern "C" {
    fn sysconf(name: c_int) -> i64;
}

/*
 * the subject polkitd checks a registration against: the session this
 * program runs in, or its process when no session can be named
 */
pub(super) fn current() -> Argument {
    if let Some(id) = session() {
        let details = BTreeMap::from([(String::from("session-id"), Argument::from(id))]);

        return Argument::Struct(vec![Argument::from("unix-session"), Argument::Map(details)]);
    }

    process()
}

// from the environment first, then from logind, which knows the session of a pid
fn session() -> Option<String> {
    if let Some(id) = std::env::var("XDG_SESSION_ID")
        .ok()
        .filter(|id| !id.is_empty())
    {
        return Some(id);
    }

    let bus = Bus::system();

    let path = bus.call(
        LOGIN,
        LOGIN_PATH,
        MANAGER,
        "GetSessionByPID",
        &[Argument::Unsigned(std::process::id())],
    );

    if path.text().is_empty() {
        return None;
    }

    // the id, not the object path, is what a session subject carries
    let id = bus.property(LOGIN, path.text(), SESSION, "Id");

    (!id.text().is_empty()).then(|| String::from(id.text()))
}

fn process() -> Argument {
    let details = BTreeMap::from([
        (String::from("pid"), Argument::Unsigned(std::process::id())),
        (String::from("start-time"), Argument::from(start_time())),
        (
            String::from("uid"),
            Argument::Int(identities::euid() as i32),
        ),
    ]);

    Argument::Struct(vec![Argument::from("unix-process"), Argument::Map(details)])
}

// milliseconds since boot, in the unit polkit's own unix-process subjects carry
fn start_time() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();

    // the command name sits in parentheses and can hold spaces, so the fields after it are counted from the last ')'
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields.split_whitespace().collect())
        .unwrap_or_default();

    // the start time is the twentieth field after the command name
    let ticks: u64 = fields
        .get(19)
        .and_then(|ticks| ticks.parse().ok())
        .unwrap_or(0);

    ticks * 1000 / ticks_per_second()
}

fn ticks_per_second() -> u64 {
    let ticks = unsafe { sysconf(CLOCK_TICKS) };

    if ticks <= 0 { 100 } else { ticks as u64 }
}
