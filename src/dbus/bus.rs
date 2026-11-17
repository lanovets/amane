use std::fmt;
use std::sync::LazyLock;

use zbus::blocking::Connection;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

use super::{Argument, Value, convert};

// a call that failed, with the name the far side chose so a caller can tell them apart
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbusError {
    pub(crate) name: String,
    pub(crate) message: String,
}

impl DbusError {
    // like "org.freedesktop.PolicyKit1.Error.Cancelled"
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DbusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.name, self.message)
    }
}

impl std::error::Error for DbusError {}

/*
 * one connection per bus for the whole program, every service thread shares
 * it; none when the bus could not be reached, and then every call answers
 * with nothing. it is tried once, so a bus that starts later needs a restart
 */
static SESSION: LazyLock<Option<Connection>> = LazyLock::new(|| Connection::session().ok());

static SYSTEM: LazyLock<Option<Connection>> = LazyLock::new(|| Connection::system().ok());

#[derive(Clone, Copy)]
pub struct Bus {
    pub(crate) connection: Option<&'static Connection>,
}

impl Bus {
    // the user's own programs: media players, notifications, the tray
    pub fn session() -> Self {
        Self {
            connection: SESSION.as_ref(),
        }
    }

    // programs shared by every user: NetworkManager, UPower, logind
    pub fn system() -> Self {
        Self {
            connection: SYSTEM.as_ref(),
        }
    }

    /*
     * a failed call gives Value::Nothing instead of panicking,
     * because a player or network service that is not running is normal
     */
    pub fn call(
        &self,
        destination: &str,
        path: &str,
        interface: &str,
        method: &str,
        arguments: &[Argument],
    ) -> Value {
        let Some(connection) = self.connection else {
            return Value::Nothing;
        };

        let reply = if arguments.is_empty() {
            connection.call_method(Some(destination), path, Some(interface), method, &())
        } else {
            let body = convert::body(arguments);

            connection.call_method(Some(destination), path, Some(interface), method, &body)
        };

        let Ok(reply) = reply else {
            return Value::Nothing;
        };

        let mut values = convert::arguments(&reply);

        // most methods answer with one value, so it comes back on its own
        match values.len() {
            0 => Value::Nothing,
            1 => values.remove(0),
            _ => Value::List(values),
        }
    }

    /*
     * the same call for a caller that has to tell a refusal from a service that
     * is not there, so the error name and message survive instead of Value::Nothing
     */
    pub fn call_result(
        &self,
        destination: &str,
        path: &str,
        interface: &str,
        method: &str,
        arguments: &[Argument],
    ) -> Result<Value, DbusError> {
        let Some(connection) = self.connection else {
            return Err(DbusError {
                name: String::from("org.freedesktop.DBus.Error.Disconnected"),
                message: String::from("the bus is not reachable"),
            });
        };

        let reply = if arguments.is_empty() {
            connection.call_method(Some(destination), path, Some(interface), method, &())
        } else {
            let body = convert::body(arguments);

            connection.call_method(Some(destination), path, Some(interface), method, &body)
        };

        let reply = reply.map_err(failure)?;

        let mut values = convert::arguments(&reply);

        Ok(match values.len() {
            0 => Value::Nothing,
            1 => values.remove(0),
            _ => Value::List(values),
        })
    }

    pub fn property(&self, destination: &str, path: &str, interface: &str, name: &str) -> Value {
        let arguments = [Argument::from(interface), Argument::from(name)];

        self.call(
            destination,
            path,
            "org.freedesktop.DBus.Properties",
            "Get",
            &arguments,
        )
    }

    pub fn set_property(
        &self,
        destination: &str,
        path: &str,
        interface: &str,
        name: &str,
        value: Argument,
    ) {
        // a property can be of any kind, so the new value travels as a variant
        let arguments = [
            Argument::from(interface),
            Argument::from(name),
            Argument::Variant(Box::new(value)),
        ];

        self.call(
            destination,
            path,
            "org.freedesktop.DBus.Properties",
            "Set",
            &arguments,
        );
    }

    // sent to everyone watching, and a failed send only means nobody hears it
    pub fn emit(&self, path: &str, interface: &str, name: &str, arguments: &[Argument]) {
        let Some(connection) = self.connection else {
            return;
        };

        if arguments.is_empty() {
            let _ = connection.emit_signal(None::<&str>, path, interface, name, &());

            return;
        }

        let body = convert::body(arguments);

        let _ = connection.emit_signal(None::<&str>, path, interface, name, &body);
    }

    // false when another program already owns the name, amane never takes it from them
    pub fn own(&self, name: &str) -> bool {
        let Some(connection) = self.connection else {
            return false;
        };

        let flags = RequestNameFlags::DoNotQueue.into();

        let reply = connection.request_name_with_flags(name, flags);

        matches!(
            reply,
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner)
        )
    }
}

fn failure(error: zbus::Error) -> DbusError {
    match error {
        zbus::Error::MethodError(name, message, _) => DbusError {
            name: name.to_string(),
            message: message.unwrap_or_default(),
        },

        other => DbusError {
            name: String::from("org.freedesktop.DBus.Error.Failed"),
            message: other.to_string(),
        },
    }
}
