//! A small in-memory ring of recently-published clips - the *publish* half of
//! the family's live-messaging primitive (the *subscribe* half is
//! [`super::events`]). A client `POST`s a clip; the host stores it here and
//! broadcasts it through the [`EventHub`](super::EventHub) so every connected
//! device sees it at once. A newly-connected device can backfill the recent
//! history from `GET /clips`.
//!
//! This is deliberately generic (an id + a text payload): Zulu uses it for
//! clipboard/link/snippet sync, and it stays presentation-free like the rest of
//! the core.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// How many recent clips the host retains for backfill. Small on purpose - Zulu
/// is "my clipboard follows me", not a store.
const MAX_CLIPS: usize = 50;

/// One published clip: a monotonically-increasing id and its text payload.
#[derive(Clone, Debug)]
pub struct Clip {
    pub id: u64,
    pub text: String,
}

/// A capped, thread-safe ring of the most recent clips.
#[derive(Default)]
pub struct ClipStore {
    next_id: AtomicU64,
    items: Mutex<VecDeque<Clip>>,
}

impl ClipStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `text` as the newest clip, evicting the oldest past [`MAX_CLIPS`],
    /// and return the stored [`Clip`] (with its freshly-assigned id).
    pub fn push(&self, text: String) -> Clip {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let clip = Clip { id, text };
        if let Ok(mut q) = self.items.lock() {
            q.push_back(clip.clone());
            while q.len() > MAX_CLIPS {
                q.pop_front();
            }
        }
        clip
    }

    /// The retained clips, oldest first (i.e. in the order they arrived).
    pub fn recent(&self) -> Vec<Clip> {
        self.items
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigns_increasing_ids() {
        let store = ClipStore::new();
        assert_eq!(store.push("a".into()).id, 0);
        assert_eq!(store.push("b".into()).id, 1);
        let recent = store.recent();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].text, "a"); // oldest first
        assert_eq!(recent[1].text, "b");
    }

    #[test]
    fn caps_at_max_and_evicts_oldest() {
        let store = ClipStore::new();
        for i in 0..(MAX_CLIPS + 5) {
            store.push(format!("clip-{i}"));
        }
        let recent = store.recent();
        assert_eq!(recent.len(), MAX_CLIPS, "ring is capped");
        assert_eq!(recent.first().unwrap().text, "clip-5", "oldest evicted");
        assert_eq!(recent.last().unwrap().text, format!("clip-{}", MAX_CLIPS + 4));
    }
}
