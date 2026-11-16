use std::any::TypeId;
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use crate::timing;

// the backend sets this once, so services never name the event loop's types
static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

// where a service's thread sleeps while it waits for another service to change
static WAITED: OnceLock<Condvar> = OnceLock::new();

/*
 * which services changed since the windows last looked, and whether
 * something changed that no service names, which every window may show
 */
static CHANGED: Mutex<Changes> = Mutex::new(Changes {
    services: None,
    everything: false,
    stamps: Vec::new(),
});

struct Changes {
    services: Option<HashSet<TypeId>>,
    everything: bool,

    // how many times each service changed, so a thread that waits for one can
    // tell whether it changed since it last looked
    stamps: Vec<(TypeId, u64)>,
}

// what the window being drawn right now has read
thread_local! {
    static READ: RefCell<HashSet<TypeId>> = RefCell::new(HashSet::new());
}

pub fn set_waker(wake: impl Fn() + Send + Sync + 'static) {
    if WAKE.set(Box::new(wake)).is_err() {
        panic!("failed to set wake: already set");
    }
}

// asks the event loop to draw every window again, from any thread
pub fn mark_all() {
    if timing::enabled() {
        eprintln!("change everything");
    }

    CHANGED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .everything = true;

    ping();
}

// only the windows that read this service are drawn again
pub fn mark(service: TypeId) {
    let mut changes = CHANGED.lock().unwrap_or_else(PoisonError::into_inner);

    changes
        .services
        .get_or_insert_with(HashSet::new)
        .insert(service);

    bump(&mut changes, service);

    drop(changes);

    // a service's thread waiting on this one was waiting for exactly this
    if let Some(waited) = WAITED.get() {
        waited.notify_all();
    }

    ping();
}

fn bump(changes: &mut Changes, service: TypeId) {
    match changes.stamps.iter_mut().find(|(id, _)| *id == service) {
        Some((_, stamp)) => *stamp += 1,
        None => changes.stamps.push((service, 1)),
    }
}

fn stamp_of(changes: &Changes, service: TypeId) -> u64 {
    changes
        .stamps
        .iter()
        .find(|(id, _)| *id == service)
        .map(|(_, stamp)| *stamp)
        .unwrap_or(0)
}

// how many times this service changed so far, which is what a later wait
// compares against
pub fn stamp(service: TypeId) -> u64 {
    let changes = CHANGED.lock().unwrap_or_else(PoisonError::into_inner);

    stamp_of(&changes, service)
}

/*
 * blocks until this service changes after `seen`, or `timeout` passes, and
 * answers the stamp either way: the one it was given means nothing changed
 * while it waited
 */
pub fn wait(service: TypeId, seen: u64, timeout: Duration) -> u64 {
    let waited = WAITED.get_or_init(Condvar::new);

    let started = Instant::now();

    let mut changes = CHANGED.lock().unwrap_or_else(PoisonError::into_inner);

    loop {
        let stamp = stamp_of(&changes, service);

        if stamp != seen {
            return stamp;
        }

        let left = timeout.saturating_sub(started.elapsed());

        if left.is_zero() {
            return stamp;
        }

        let (guard, _) = waited
            .wait_timeout(changes, left)
            .unwrap_or_else(PoisonError::into_inner);

        changes = guard;
    }
}

// none means every window should draw
pub fn take() -> Option<HashSet<TypeId>> {
    let mut changes = CHANGED.lock().unwrap_or_else(PoisonError::into_inner);

    let services = changes.services.take().unwrap_or_default();

    if std::mem::take(&mut changes.everything) {
        return None;
    }

    Some(services)
}

pub fn note_read(service: TypeId) {
    READ.with_borrow_mut(|read| read.insert(service));
}

// what was read since the last call, which starts a fresh list
pub fn take_read() -> HashSet<TypeId> {
    READ.take()
}

// wakes the event loop from any thread, which by itself draws nothing again
pub fn ping() {
    // a write before the loop exists is picked up by the first frame anyway
    let Some(wake) = WAKE.get() else {
        return;
    };

    wake();
}
