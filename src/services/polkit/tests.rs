use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::dbus::Value;

use super::{
    Identity, Polkit, PolkitError, PolkitMessageKind, PolkitRequest, PolkitResult, Service,
};

static SERIAL: Mutex<()> = Mutex::new(());

static NEXT: AtomicUsize = AtomicUsize::new(0);

// the cookie line comes first, then every prompt is written out and its answer read back
const TWO_PROMPTS: &str = r#"#!/bin/sh
printf '%s' "$1" > "$0.user"
read cookie
printf 'PAM_PROMPT_ECHO_OFF Password:\n'
read password
printf 'PAM_TEXT_INFO Welcome\n'
printf 'PAM_PROMPT_ECHO_ON Code:\n'
read code
if [ "$password" = "test-password" ] && [ "$code" = "246810" ]; then
    printf 'SUCCESS\n'
else
    printf 'FAILURE\n'
fi
"#;

const REFUSED: &str = r#"#!/bin/sh
printf '%s' "$1" > "$0.user"
read cookie
printf 'PAM_ERROR_MSG Password is wrong\n'
printf 'FAILURE\n'
"#;

const WAITING: &str = r#"#!/bin/sh
printf '%s' "$1" > "$0.user"
read cookie
printf 'PAM_PROMPT_ECHO_OFF Password:\n'
read password
printf 'SUCCESS\n'
"#;

// every request names itself, so a test can tell whose prompt is up
const NAMED: &str = r#"#!/bin/sh
printf '%s' "$1" > "$0.user"
read cookie
printf 'PAM_PROMPT_ECHO_OFF Password for %s:\n' "$cookie"
read password
printf 'SUCCESS\n'
"#;

// Every test starts its own helper and talks to the service through its internal entry point.
struct Fixture {
    directory: PathBuf,

    _serial: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new(script: &str) -> Self {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);

        // the clock keeps a run from a recycled pid, so no helper left over from an earlier one holds the same script
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or(0);

        let directory = std::env::temp_dir().join(format!(
            "amane-polkit-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));

        fs::create_dir_all(&directory).expect("create the test helper directory");

        let helper = directory.join("polkit-agent-helper-1");

        fs::write(&helper, script).expect("write the test helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755))
            .expect("make the test helper runnable");

        Polkit::cancel();
        Polkit::set_helper_path(&helper);

        Self {
            directory,
            _serial: serial,
        }
    }

    // the user the helper was started for, or none while it has not started
    fn user(&self) -> Option<String> {
        let path = self.directory.join("polkit-agent-helper-1.user");

        fs::read_to_string(path).ok()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        {
            let mut polkit = Polkit::write();

            polkit.queue.clear();
            polkit.quiet();
        }

        Polkit::cancel();

        let deadline = Instant::now() + Duration::from_secs(5);

        while Polkit::read().active() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }

        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn request() -> PolkitRequest {
    PolkitRequest {
        action_id: String::from("org.example.amane-test"),
        message: String::from("Authentication is required to test"),
        icon: String::from("dialog-password"),
        details: BTreeMap::from([(String::from("program"), String::from("/usr/bin/test"))]),
    }
}

fn identities(names: &[&str]) -> Vec<Identity> {
    names
        .iter()
        .map(|name| Identity {
            name: String::from(*name),
        })
        .collect()
}

fn wait_for(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);

    while !condition() {
        assert!(
            Instant::now() < deadline,
            "the request did not reach the expected state"
        );

        thread::sleep(Duration::from_millis(5));
    }
}

fn prompt(text: &str) -> u64 {
    wait_for(|| {
        Polkit::read()
            .prompt()
            .is_some_and(|message| message.text() == text)
    });

    Polkit::read().prompt().expect("a waiting prompt").id()
}

fn complete() -> PolkitResult {
    wait_for(|| !Polkit::read().active());

    Polkit::read().result().expect("a result").clone()
}

fn queued() -> usize {
    Polkit::read().queue.len()
}

#[test]
fn each_prompt_gets_its_own_answer_and_the_messages_are_kept() {
    let fixture = Fixture::new(TWO_PROMPTS);

    Polkit::submit("cookie", request(), identities(&["amane-test"]), None);

    let password = prompt("Password:");

    assert_eq!(
        Polkit::read().prompt().unwrap().kind(),
        PolkitMessageKind::Prompt { visible: false }
    );

    // the action polkitd asks about is kept for the dialog's own text
    let polkit = Polkit::read();
    let action = polkit.request().expect("the action of the active request");

    assert_eq!(action.action_id(), "org.example.amane-test");
    assert_eq!(action.message(), "Authentication is required to test");
    assert_eq!(action.icon(), "dialog-password");
    assert_eq!(
        action.details().get("program").map(String::as_str),
        Some("/usr/bin/test")
    );

    drop(polkit);

    assert_eq!(
        Polkit::respond(password, "two\nlines"),
        Err(PolkitError::InvalidInput("answer"))
    );

    Polkit::respond(password, "test-password").expect("answer the password");

    let code = prompt("Code:");

    assert_eq!(
        Polkit::read().prompt().unwrap().kind(),
        PolkitMessageKind::Prompt { visible: true }
    );

    assert_eq!(
        Polkit::respond(password, "stale-password"),
        Err(PolkitError::NoPrompt)
    );

    Polkit::respond(code, "246810").expect("answer the code");

    assert_eq!(complete(), PolkitResult::Success);

    let messages: Vec<_> = Polkit::read()
        .messages()
        .iter()
        .map(|message| (String::from(message.text()), message.kind()))
        .collect();

    assert_eq!(
        messages,
        [
            (
                String::from("Password:"),
                PolkitMessageKind::Prompt { visible: false }
            ),
            (String::from("Welcome"), PolkitMessageKind::Info),
            (
                String::from("Code:"),
                PolkitMessageKind::Prompt { visible: true }
            ),
        ]
    );

    assert!(Polkit::read().prompt().is_none());
    assert!(Polkit::read().request().is_none());
    assert_eq!(fixture.user().as_deref(), Some("amane-test"));
}

#[test]
fn a_refusal_keeps_the_message_the_helper_gave() {
    let _fixture = Fixture::new(REFUSED);

    Polkit::submit("cookie", request(), identities(&["amane-test"]), None);

    assert_eq!(complete(), PolkitResult::Rejected);

    let messages: Vec<_> = Polkit::read()
        .messages()
        .iter()
        .map(|message| (String::from(message.text()), message.kind()))
        .collect();

    assert_eq!(
        messages,
        [(String::from("Password is wrong"), PolkitMessageKind::Error)]
    );
}

#[test]
fn cancelling_wakes_a_waiting_prompt_and_allows_another_request() {
    let _fixture = Fixture::new(WAITING);

    Polkit::submit("first", request(), identities(&["amane-test"]), None);

    let old = prompt("Password:");

    Polkit::cancel();

    assert_eq!(Polkit::read().result(), Some(&PolkitResult::Cancelled));
    assert!(!Polkit::read().active());
    assert_eq!(
        Polkit::respond(old, "test-password"),
        Err(PolkitError::NoPrompt)
    );

    Polkit::submit("second", request(), identities(&["amane-test"]), None);

    let current = prompt("Password:");

    assert_ne!(current, old);

    Polkit::respond(current, "test-password").expect("answer the second request");

    assert_eq!(complete(), PolkitResult::Success);
}

#[test]
fn polkit_cancels_a_queued_request_without_touching_the_active_one() {
    let _fixture = Fixture::new(NAMED);

    Polkit::submit("active", request(), identities(&["amane-test"]), None);

    let password = prompt("Password for active:");

    Polkit::submit("queued", request(), identities(&["amane-test"]), None);

    assert_eq!(queued(), 1);

    assert!(Polkit::cancel_cookie("queued"));
    assert_eq!(queued(), 0);
    assert!(Polkit::read().active());

    // the active request still waits for its own answer
    assert_eq!(
        Polkit::read().prompt().map(|prompt| prompt.id()),
        Some(password)
    );

    assert!(Polkit::cancel_cookie("active"));
    assert_eq!(
        Polkit::read().result(),
        Some(&PolkitResult::CancelledByPolkit)
    );
    assert!(!Polkit::read().active());

    assert!(!Polkit::cancel_cookie("unknown"));
}

#[test]
fn requests_wait_their_turn_instead_of_replacing_the_active_one() {
    let _fixture = Fixture::new(NAMED);

    Polkit::submit("first", request(), identities(&["amane-test"]), None);

    let first = prompt("Password for first:");

    Polkit::submit("second", request(), identities(&["amane-test"]), None);

    assert_eq!(queued(), 1);
    assert_eq!(
        Polkit::read().prompt().map(|prompt| prompt.id()),
        Some(first)
    );

    Polkit::respond(first, "test-password").expect("answer the first request");

    let second = prompt("Password for second:");

    assert_ne!(second, first);
    assert_eq!(queued(), 0);

    Polkit::respond(second, "test-password").expect("answer the second request");

    assert_eq!(complete(), PolkitResult::Success);
}

#[test]
fn the_identity_picked_before_the_helper_starts_is_the_one_it_runs_for() {
    let fixture = Fixture::new(WAITING);

    Polkit::submit("cookie", request(), identities(&["first", "second"]), None);

    // nothing is asked until one of them is picked
    assert_eq!(fixture.user(), None);
    assert_eq!(Polkit::read().identity(), 0);
    assert_eq!(
        Polkit::select_identity(2),
        Err(PolkitError::InvalidInput("identity"))
    );

    Polkit::select_identity(1).expect("pick the second user");

    let password = prompt("Password:");

    assert_eq!(Polkit::read().identity(), 1);
    assert_eq!(fixture.user().as_deref(), Some("second"));
    assert_eq!(Polkit::select_identity(0), Err(PolkitError::NoIdentity));

    Polkit::respond(password, "test-password").expect("answer the password");

    assert_eq!(complete(), PolkitResult::Success);
}

#[test]
fn a_view_can_read_the_service_while_the_helper_waits_for_an_answer() {
    let _fixture = Fixture::new(WAITING);

    Polkit::submit("cookie", request(), identities(&["amane-test"]), None);

    let password = prompt("Password:");

    let deadline = Instant::now() + Duration::from_secs(5);

    let mut readable = false;

    while Instant::now() < deadline {
        if crate::services::store::find::<Polkit>().try_read().is_ok() {
            readable = true;

            break;
        }

        thread::sleep(Duration::from_millis(5));
    }

    assert!(
        readable,
        "the helper held Polkit while it waited for an answer"
    );

    Polkit::respond(password, "test-password").expect("answer the password");

    assert_eq!(complete(), PolkitResult::Success);
}

fn identity(kind: &str, key: &str, value: Value) -> Value {
    Value::List(vec![
        Value::Text(String::from(kind)),
        Value::Map(BTreeMap::from([(String::from(key), value)])),
    ])
}

#[test]
fn users_come_from_the_identities_polkit_sends() {
    // uid 0 is root on every machine, so this needs no account of its own
    let identities = Value::List(vec![
        identity("unix-user", "uid", Value::Number(0.0)),
        identity("unix-user", "uid", Value::Number(0.0)),
        identity("unix-group", "gid", Value::Number(0.0)),
        identity("unix-netgroup", "name", Value::Text(String::from("staff"))),
        Value::Text(String::from("not an identity")),
    ]);

    let users = super::identities::users(&identities);

    assert_eq!(users.first().map(|user| user.name()), Some("root"));
    assert_eq!(users.iter().filter(|user| user.name() == "root").count(), 1);
    assert!(users.iter().all(|user| !user.name().is_empty()));
}
