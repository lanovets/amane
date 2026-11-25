mod image_hint;
mod notification;
mod server;

pub use notification::{Action, Notification, Urgency};

pub(crate) use notification::read_hints;

use crate::Service;

pub struct Notifications {
    list: Vec<Notification>,

    // the spec keeps 0 for "no notification", so ids start at 1
    next_id: u32,

    running: bool,
}

// the numbers the spec gives each way a notification can close
#[derive(Clone, Copy)]
pub enum Reason {
    Dismissed = 2,

    // the sender asked with CloseNotification
    Closed = 3,
}

/*
 * amane is the notification server here: programs send
 * their notifications to it, instead of amane asking anyone
 */
impl Service for Notifications {
    fn new() -> Self {
        Self {
            list: Vec::new(),
            next_id: 1,
            running: false,
        }
    }

    fn listen() {
        server::run();
    }
}

impl Notifications {
    // oldest first
    pub fn list(&self) -> &[Notification] {
        &self.list
    }

    // false when another daemon like mako or dunst already owns the name
    pub fn running(&self) -> bool {
        self.running
    }

    pub fn dismiss(id: u32) {
        Self::write().close(id, Reason::Dismissed);
    }

    pub fn clear() {
        let mut notifications = Self::write();

        let mut ids = Vec::new();

        for notification in &notifications.list {
            ids.push(notification.id);
        }

        for id in ids {
            notifications.close(id, Reason::Dismissed);
        }
    }

    // a button was pressed, pass the key from notification.actions()
    pub fn invoke(id: u32, key: &str) {
        Self::write().run_action(id, key);
    }

    // the notification itself was clicked: its default action, or a dismiss when it has none
    pub fn click(id: u32) {
        let mut notifications = Self::write();

        let has_default = notifications
            .list
            .iter()
            .any(|notification| notification.id == id && notification.has_default);

        if has_default {
            notifications.run_action(id, server::DEFAULT_ACTION);
        } else {
            notifications.close(id, Reason::Dismissed);
        }
    }

    // the spec closes a notification once an action ran, unless it asked to stay
    fn run_action(&mut self, id: u32, key: &str) {
        let Some(notification) = self.list.iter().find(|notification| notification.id == id) else {
            return;
        };

        let resident = notification.resident;

        server::announce_action(id, key);

        if !resident {
            self.close(id, Reason::Dismissed);
        }
    }

    // a sender may replace its own earlier notification, which keeps its id and place
    fn add(&mut self, mut notification: Notification, replaces_id: u32) -> u32 {
        let replaced = self.list.iter_mut().find(|old| old.id == replaces_id);

        if let Some(old) = replaced {
            notification.id = replaces_id;
            notification.replaced = true;

            image_hint::remove(&old.image);

            *old = notification;

            return replaces_id;
        }

        let id = self.next_id;

        self.next_id += 1;

        notification.id = id;

        self.list.push(notification);

        id
    }

    fn close(&mut self, id: u32, reason: Reason) {
        let Some(index) = self
            .list
            .iter()
            .position(|notification| notification.id == id)
        else {
            return;
        };

        let closed = self.list.remove(index);

        image_hint::remove(&closed.image);

        server::announce_closed(id, reason);
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn sent(summary: &str) -> Notification {
        Notification {
            id: 0,
            app_name: String::new(),
            summary: String::from(summary),
            body: String::new(),
            icon: String::new(),
            image: String::new(),
            urgency: Urgency::Normal,
            expires: -1,
            transient: false,
            category: None,
            replaced: false,
            actions: Vec::new(),
            has_default: false,
            resident: false,
            received: SystemTime::now(),
        }
    }

    #[test]
    fn only_a_notification_that_took_another_ones_place_is_replaced() {
        let mut notifications = Notifications::new();

        let first = notifications.add(sent("first"), 0);
        let update = notifications.add(sent("update"), first);

        // an id nobody has, like one already closed, makes a new notification
        let stale = notifications.add(sent("stale"), 99);

        assert_eq!(update, first);
        assert_ne!(stale, 99);

        let replaced = |id| {
            notifications
                .list()
                .iter()
                .find(|n| n.id == id)
                .map(Notification::replaced)
        };

        assert_eq!(replaced(first), Some(true));
        assert_eq!(replaced(stale), Some(false));
        assert_eq!(notifications.list().len(), 2);
    }
}
