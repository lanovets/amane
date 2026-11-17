use std::fmt;

use crate::dbus::DbusError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub(super) name: String,
}

impl Identity {
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolkitMessageKind {
    Info,
    Error,
    Prompt { visible: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolkitMessage {
    pub(super) id: u64,
    pub(super) text: String,
    pub(super) kind: PolkitMessageKind,
}

impl PolkitMessage {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn kind(&self) -> PolkitMessageKind {
        self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolkitResult {
    Success,

    // the helper's PAM conversation refused the user
    Rejected,

    // the user closed the prompt
    Cancelled,

    // polkitd took the request back, like when the action got authorized another way
    CancelledByPolkit,

    Error(PolkitError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolkitError {
    // no setuid helper, so polkitd would refuse the answer anyway
    HelperUnavailable,

    NoPrompt,

    // the helper is already running for a user, or there is no other one to pick
    NoIdentity,

    InvalidInput(&'static str),

    Worker(String),

    Bus(DbusError),
}

impl fmt::Display for PolkitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HelperUnavailable => f.write_str("no setuid polkit agent helper to start"),
            Self::NoPrompt => f.write_str("this prompt is not waiting for an answer"),
            Self::NoIdentity => f.write_str("no identity to choose, or the helper already runs"),
            Self::InvalidInput(field) => write!(f, "invalid polkit {field}"),
            Self::Worker(message) => write!(f, "polkit helper: {message}"),
            Self::Bus(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for PolkitError {}
