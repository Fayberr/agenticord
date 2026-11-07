//! What the gateway task shares with the tools: connection status, the
//! account, and a short ring of recent incoming messages.

use serde_json::Value;
use std::{collections::VecDeque, sync::Mutex};

/// Most recent events kept in memory.
const EVENT_CAP: usize = 200;

#[derive(Default)]
pub struct Inner {
    pub status: String,
    pub last_error: Option<String>,
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub guilds: usize,
    pub connected_at: u64,
    pub reconnects: u32,
    /// online, idle, dnd or invisible: what the gateway presents.
    pub presence: String,
    pub events: VecDeque<Value>,
    pub next_event: u64,
}

pub struct Shared {
    inner: Mutex<Inner>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                status: "not started".into(),
                presence: "online".into(),
                ..Inner::default()
            }),
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard)
    }

    pub fn set_status(&self, status: &str) {
        self.with(|s| s.status = status.to_owned());
    }

    pub fn push_event(&self, mut event: Value) {
        self.with(|s| {
            s.next_event += 1;
            event["n"] = s.next_event.into();
            if s.events.len() >= EVENT_CAP {
                s.events.pop_front();
            }
            s.events.push_back(event);
        });
    }
}
