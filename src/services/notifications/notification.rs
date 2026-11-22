use std::time::SystemTime;

use crate::Value;

#[derive(Debug, Clone)]
pub struct Notification {
    pub(crate) id: u32,

    pub(crate) app_name: String,

    pub(crate) summary: String,

    pub(crate) body: String,

    pub(crate) icon: String,

    pub(crate) image: String,

    pub(crate) urgency: Urgency,

    // milliseconds the sender asked it to live: -1 leaves it to the shell, 0 until it is dismissed
    pub(crate) expires: i32,

    pub(crate) transient: bool,

    pub(crate) category: Option<String>,

    // set by Notifications::add when it took the place of one already shown
    pub(crate) replaced: bool,

    // the "default" action is kept apart, it has no button of its own
    pub(crate) actions: Vec<Action>,

    pub(crate) has_default: bool,

    // stays open after an action, until it is dismissed or the sender closes it
    pub(crate) resident: bool,

    pub(crate) received: SystemTime,
}

// a button a notification asks for, like "reply" or "open"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub(crate) key: String,

    pub(crate) label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Urgency {
    Low,

    // what a notification without the hint gets
    #[default]
    Normal,

    Critical,
}

impl Notification {
    // pass this to Notifications::dismiss
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn app_name(&self) -> &str {
        &self.app_name
    }

    // the one-line title
    pub fn summary(&self) -> &str {
        &self.summary
    }

    // may be empty, and may hold simple markup like <b>
    pub fn body(&self) -> &str {
        &self.body
    }

    // an icon name like "firefox", a file path, or empty
    pub fn icon(&self) -> &str {
        &self.icon
    }

    // a picture for this one notification, like a sender's avatar: a file path, a file url, or empty
    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn urgency(&self) -> Urgency {
        self.urgency
    }

    // the timeout the sender asked for, see Notifications::notify in the spec
    pub fn expire_timeout(&self) -> i32 {
        self.expires
    }

    // it should be shown but not kept once it closes
    pub fn transient(&self) -> bool {
        self.transient
    }

    pub fn category(&self) -> Option<&str> {
        self.category.as_deref()
    }

    // whether it updated one already shown, which keeps the same id
    pub fn replaced(&self) -> bool {
        self.replaced
    }

    // the buttons to show, in the order the sender gave them
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    // whether clicking the notification itself does something, see Notifications::click
    pub fn has_default_action(&self) -> bool {
        self.has_default
    }

    // when it arrived, or when the sender last replaced it
    pub fn received(&self) -> SystemTime {
        self.received
    }
}

impl Action {
    // pass this to Notifications::invoke
    pub fn key(&self) -> &str {
        &self.key
    }

    // the text to show on the button
    pub fn label(&self) -> &str {
        &self.label
    }
}

// the hints a notification carries, read once when it arrives
#[derive(Debug, Clone, Default)]
pub(crate) struct Hints {
    pub(crate) urgency: Urgency,

    pub(crate) transient: bool,

    pub(crate) category: Option<String>,
}

pub(crate) fn read_hints(hints: &Value) -> Hints {
    let category = hints.get("category").text();

    Hints {
        // a sender that leaves urgency out gets the spec's normal, not low
        urgency: match hints.get("urgency") {
            Value::Number(level) => Urgency::from_level(*level),
            _ => Urgency::default(),
        },

        transient: hints.get("transient").bool(),

        category: (!category.is_empty()).then(|| String::from(category)),
    }
}

impl Urgency {
    // the spec sends urgency as a byte: 0 low, 1 normal, 2 critical
    pub(crate) fn from_level(level: f64) -> Self {
        match level as u8 {
            0 => Self::Low,
            2 => Self::Critical,
            _ => Self::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::Value;

    use super::*;

    fn hint_map(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Map(
            pairs
                .into_iter()
                .map(|(key, value)| (String::from(key), value))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn a_hint_map_reads_every_field_a_shell_needs() {
        let read = read_hints(&hint_map([
            ("urgency", Value::Number(2.0)),
            ("transient", Value::Bool(true)),
            ("category", Value::Text(String::from("im.received"))),
        ]));

        assert_eq!(read.urgency, Urgency::Critical);
        assert!(read.transient);
        assert_eq!(read.category.as_deref(), Some("im.received"));
    }

    #[test]
    fn an_empty_hint_map_gives_the_defaults() {
        let read = read_hints(&hint_map([]));

        assert_eq!(read.urgency, Urgency::Normal);
        assert!(!read.transient);
        assert!(read.category.is_none());
    }
}
