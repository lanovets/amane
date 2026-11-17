mod helper;
mod identities;
mod subject;
mod types;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;

use crate::Service;
use crate::dbus::{Argument, Bus, Method, Value};

pub use types::{
    Identity, PolkitError, PolkitMessage, PolkitMessageKind, PolkitRequest, PolkitResult,
};

const NAME: &str = "org.freedesktop.PolicyKit1";

const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";

const AUTHORITY: &str = "org.freedesktop.PolicyKit1.Authority";

// the object polkitd calls into, and remembers from the registration
const PATH: &str = "/org/amane/PolicyKit1/AuthenticationAgent";

const INTERFACE: &str = "org.freedesktop.PolicyKit1.AuthenticationAgent";

const FAILED: &str = "org.freedesktop.PolicyKit1.Error.Failed";

const CANCELLED: &str = "org.freedesktop.PolicyKit1.Error.Cancelled";

/*
 * polkitd answers only a helper running as root, so the setuid wrapper is the
 * helper worth starting; the one in the store is not setuid and its answer is refused
 */
const HELPER: &str = "/run/wrappers/bin/polkit-agent-helper-1";

// the authentication agent of the session, one request at a time
#[derive(Default)]
pub struct Polkit {
    // set by set_helper_path, for a test that answers the protocol itself
    helper: Option<PathBuf>,

    registered: bool,

    // the request being answered, if any
    active: Option<Active>,

    // requests waiting for the active one, oldest first
    queue: VecDeque<Request>,

    messages: Vec<PolkitMessage>,

    // the prompt waiting for an answer, and who answers it
    prompt: Option<(u64, Sender<String>)>,

    result: Option<PolkitResult>,

    identities: Vec<Identity>,

    selected: usize,

    next_message: u64,
}

struct Request {
    cookie: String,

    // taken by whoever replies, and none in a test that has no caller to answer
    method: Option<Method>,

    action: PolkitRequest,

    identities: Vec<Identity>,
}

struct Active {
    request: Request,

    conversation: Arc<Conversation>,

    // whether the helper runs already, so an identity picked first is the one it uses
    started: bool,
}

#[derive(Default)]
struct Conversation {
    aborted: AtomicBool,
}

// what the worker needs to start a helper, taken out from under the service's lock
struct Start {
    conversation: Arc<Conversation>,
    cookie: String,
    user: String,
    helper: PathBuf,
}

impl Conversation {
    fn is_current(self: &Arc<Self>, active: &Option<Active>) -> bool {
        active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(&active.conversation, self))
    }

    fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
    }

    fn aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }
}

impl Service for Polkit {
    fn new() -> Self {
        Self::default()
    }

    /*
     * waits here for polkitd across unregister and register, so it never takes the
     * session's agent slot on its own: another agent may be the one answering
     */
    fn listen() {
        for method in Bus::system().methods(PATH, INTERFACE) {
            match method.name() {
                "BeginAuthentication" => begin(method),
                "CancelAuthentication" => cancel_authentication(&method),

                // every call waits for an answer, even one amane does not know
                _ => method.reply(&[]),
            }
        }
    }
}

impl Polkit {
    pub fn registered(&self) -> bool {
        self.registered
    }

    // a request is being answered, or waits for its identity to be picked
    pub fn active(&self) -> bool {
        self.active.is_some()
    }

    pub fn messages(&self) -> &[PolkitMessage] {
        &self.messages
    }

    // only the current unanswered prompt; its id must accompany an answer
    pub fn prompt(&self) -> Option<&PolkitMessage> {
        let (id, _) = self.prompt.as_ref()?;

        self.messages.last().filter(|message| message.id == *id)
    }

    pub fn result(&self) -> Option<&PolkitResult> {
        self.result.as_ref()
    }

    // the action behind the request being answered
    pub fn request(&self) -> Option<&PolkitRequest> {
        self.active.as_ref().map(|active| &active.request.action)
    }

    pub fn identities(&self) -> &[Identity] {
        &self.identities
    }

    // which of identities() the helper runs for, or will run for once one is picked
    pub fn identity(&self) -> usize {
        self.selected
    }

    /// Registers amane as this session's agent, replacing the one that had the slot.
    pub fn register() -> Result<(), PolkitError> {
        let helper = Self::read().helper_path();

        if !helper.exists() {
            return Err(PolkitError::HelperUnavailable);
        }

        let arguments = [
            subject::current(),
            Argument::from(locale()),
            Argument::from(PATH),
        ];

        Bus::system()
            .call_result(
                NAME,
                AUTHORITY_PATH,
                AUTHORITY,
                "RegisterAuthenticationAgent",
                &arguments,
            )
            .map_err(PolkitError::Bus)?;

        Self::write().registered = true;

        Ok(())
    }

    /// Gives the slot back, so another agent answers the next request.
    pub fn unregister() {
        let arguments = [subject::current(), Argument::from(PATH)];

        let _ = Bus::system().call_result(
            NAME,
            AUTHORITY_PATH,
            AUTHORITY,
            "UnregisterAuthenticationAgent",
            &arguments,
        );

        Self::write().registered = false;
    }

    /// Starts the helper from another path, for a test that speaks the protocol itself.
    pub fn set_helper_path(path: impl Into<PathBuf>) {
        Self::write().helper = Some(path.into());
    }

    /// Answers the prompt with this id. A stale id, or one answered already, is refused.
    pub fn respond(id: u64, answer: &str) -> Result<(), PolkitError> {
        // the helper reads one line per answer, so an answer cannot carry a line break
        if answer.contains(['\0', '\n']) {
            return Err(PolkitError::InvalidInput("answer"));
        }

        let mut polkit = Self::write();

        if !polkit
            .prompt
            .as_ref()
            .is_some_and(|(current, _)| *current == id)
        {
            polkit.quiet();

            return Err(PolkitError::NoPrompt);
        }

        let (_, sender) = polkit.prompt.take().ok_or(PolkitError::NoPrompt)?;

        sender
            .send(String::from(answer))
            .map_err(|_| PolkitError::NoPrompt)
    }

    /// Runs the helper for one of identities(), while no prompt waits for an answer yet.
    pub fn select_identity(index: usize) -> Result<(), PolkitError> {
        let start = {
            let mut polkit = Self::write();

            if index >= polkit.identities.len() {
                polkit.quiet();

                return Err(PolkitError::InvalidInput("identity"));
            }

            let start = polkit.start(index);

            if start.is_none() {
                polkit.quiet();
            }

            start
        };

        let Some(start) = start else {
            return Err(PolkitError::NoIdentity);
        };

        spawn(start);

        Ok(())
    }

    /// Ends the request being answered, and drops its prompt.
    pub fn cancel() {
        let method = {
            let mut polkit = Self::write();

            let Some(active) = polkit.active.take() else {
                polkit.quiet();

                return;
            };

            active.conversation.abort();

            polkit.prompt = None;
            polkit.result = Some(PolkitResult::Cancelled);

            active.request.method
        };

        if let Some(method) = method {
            method.reply_error(CANCELLED, "the user cancelled the authentication");
        }

        Self::advance();
    }

    fn helper_path(&self) -> PathBuf {
        self.helper.clone().unwrap_or_else(|| PathBuf::from(HELPER))
    }

    // hands the active request to the helper, and answers nothing once one runs already
    fn start(&mut self, index: usize) -> Option<Start> {
        let user = self.identities.get(index)?.name().to_string();
        let helper = self.helper_path();

        if self.active.as_ref()?.started {
            return None;
        }

        self.selected = index;

        let active = self.active.as_mut()?;

        active.started = true;

        Some(Start {
            conversation: active.conversation.clone(),
            cookie: active.request.cookie.clone(),
            user,
            helper,
        })
    }

    // the entry polkitd's calls end in, and the one a test drives with no caller to answer
    fn submit(
        cookie: &str,
        action: PolkitRequest,
        identities: Vec<Identity>,
        method: Option<Method>,
    ) {
        {
            let mut polkit = Self::write();

            polkit.queue.push_back(Request {
                cookie: String::from(cookie),
                method,
                action,
                identities,
            });

            // a request behind another one waits without showing anything of its own
            if polkit.active.is_some() {
                polkit.quiet();

                return;
            }
        }

        Self::advance();
    }

    // starts the oldest queued request while nothing is being answered
    fn advance() {
        let start = {
            let mut polkit = Self::write();

            if polkit.active.is_some() {
                polkit.quiet();

                return;
            }

            let Some(request) = polkit.queue.pop_front() else {
                polkit.quiet();

                return;
            };

            polkit.messages.clear();
            polkit.prompt = None;
            polkit.result = None;
            polkit.identities = request.identities.clone();
            polkit.selected = 0;

            polkit.active = Some(Active {
                request,
                conversation: Arc::new(Conversation::default()),
                started: false,
            });

            // one user needs nobody to pick them, and the prompt can come up right away
            if polkit.identities.len() == 1 {
                polkit.start(0)
            } else {
                None
            }
        };

        if let Some(start) = start {
            spawn(start);
        }
    }

    // CancelAuthentication: a queued request is dropped unanswered, the active one is aborted
    fn cancel_cookie(cookie: &str) -> bool {
        let (aborted, method) = {
            let mut polkit = Self::write();

            if let Some(index) = polkit
                .queue
                .iter()
                .position(|request| request.cookie == cookie)
            {
                let request = polkit
                    .queue
                    .remove(index)
                    .expect("a queued request was found");

                polkit.quiet();

                (false, request.method)
            } else if polkit
                .active
                .as_ref()
                .is_some_and(|active| active.request.cookie == cookie)
            {
                let active = polkit.active.take().expect("an active request was found");

                active.conversation.abort();

                polkit.prompt = None;
                polkit.result = Some(PolkitResult::CancelledByPolkit);

                (true, active.request.method)
            } else {
                polkit.quiet();

                return false;
            }
        };

        if let Some(method) = method {
            let message = if aborted {
                "polkitd cancelled the authentication"
            } else {
                "the request was cancelled before it started"
            };

            method.reply_error(CANCELLED, message);
        }

        if aborted {
            Self::advance();
        }

        true
    }
}

// BeginAuthentication(action_id, message, icon_name, details, cookie, identities)
fn begin(method: Method) {
    let arguments = method.arguments();

    let [action, message, icon, details, cookie, identities, ..] = arguments else {
        method.reply_error(FAILED, "unexpected BeginAuthentication arguments");

        return;
    };

    let cookie = String::from(cookie.text());
    let users = identities::users(identities);

    if users.is_empty() {
        method.reply_error(FAILED, "no user in this subject can answer");

        return;
    }

    let action = PolkitRequest {
        action_id: String::from(action.text()),
        message: String::from(message.text()),
        icon: String::from(icon.text()),
        details: strings(details),
    };

    Polkit::submit(&cookie, action, users, Some(method));
}

// the action's details arrive as text pairs, and a value of another kind is left out
fn strings(details: &Value) -> BTreeMap<String, String> {
    let Value::Map(details) = details else {
        return BTreeMap::new();
    };

    details
        .iter()
        .filter_map(|(key, value)| match value {
            Value::Text(text) => Some((key.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

fn cancel_authentication(method: &Method) {
    let cookie = method.arguments().first().map_or("", Value::text);

    if !Polkit::cancel_cookie(cookie) {
        method.reply_error(FAILED, "this cookie is not waiting");

        return;
    }

    method.reply(&[]);
}

fn spawn(start: Start) {
    let conversation = start.conversation.clone();

    let spawned = thread::Builder::new()
        .name(String::from("amane-polkit"))
        .spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                helper::authenticate(
                    &start.conversation,
                    &start.helper,
                    &start.user,
                    &start.cookie,
                )
            }))
            .unwrap_or_else(|_| {
                PolkitResult::Error(PolkitError::Worker(String::from(
                    "the polkit worker panicked",
                )))
            });

            finish(&start.conversation, result);
        });

    if let Err(error) = spawned {
        finish(
            &conversation,
            PolkitResult::Error(PolkitError::Worker(error.to_string())),
        );
    }
}

fn finish(conversation: &Arc<Conversation>, result: PolkitResult) {
    let method = {
        let mut polkit = Polkit::write();

        // a request cancelled while the helper was busy has been answered already
        if !conversation.is_current(&polkit.active) {
            polkit.quiet();

            return;
        }

        polkit.prompt = None;
        polkit.result = Some(result.clone());

        polkit
            .active
            .take()
            .and_then(|active| active.request.method)
    };

    if let Some(method) = method {
        answer(&method, &result);
    }

    Polkit::advance();
}

fn answer(method: &Method, result: &PolkitResult) {
    match result {
        PolkitResult::Success => method.reply(&[]),

        PolkitResult::Cancelled => {
            method.reply_error(CANCELLED, "the user cancelled the authentication");
        }

        PolkitResult::CancelledByPolkit => {
            method.reply_error(CANCELLED, "polkitd cancelled the authentication");
        }

        PolkitResult::Rejected => method.reply_error(FAILED, "authentication failed"),

        PolkitResult::Error(error) => method.reply_error(FAILED, &error.to_string()),
    }
}

/*
 * shows what the helper said, and for a prompt waits for the answer here
 * rather than in the worker, so a view can read the service meanwhile
 */
fn message(
    conversation: &Arc<Conversation>,
    text: String,
    kind: PolkitMessageKind,
) -> Result<Option<String>, ()> {
    let prompt = matches!(kind, PolkitMessageKind::Prompt { .. });

    let (sender, receiver) = mpsc::channel();

    {
        let mut polkit = Polkit::write();

        if conversation.aborted() || !conversation.is_current(&polkit.active) {
            polkit.quiet();

            return Err(());
        }

        polkit.next_message = polkit.next_message.wrapping_add(1);

        let id = polkit.next_message;

        polkit.messages.push(PolkitMessage { id, text, kind });

        if prompt {
            polkit.prompt = Some((id, sender));
        }
    }

    if !prompt {
        return Ok(None);
    }

    receiver.recv().map(Some).map_err(|_| ())
}

// polkitd hands this to PAM, which picks the language of its messages with it
fn locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_default()
}
