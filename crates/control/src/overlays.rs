//! Overlay pages open in OBS, and what each says about itself: whether OBS
//! shows it on stream. The count of those that do decides whether the slate can
//! cover a dump (see [`crate::AppState::dump`]).
//!
//! OBS tells a browser source when it goes on or off stream (becomes active in
//! the program output), but not the state it is in when the page loads: after
//! OBS starts, or the page is refreshed, a page on stream does not know it is
//! until it is hidden and shown once. Until then it counts as connected, not as
//! on stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Overlay pages connected.
    pub count: usize,
    /// Of them, those OBS said are on stream (in the program output).
    pub active: usize,
}

pub(crate) struct Overlays {
    next: AtomicU64,
    /// For each page: on stream, off stream, or not said yet.
    pages: Mutex<HashMap<u64, Option<bool>>>,
    counts: watch::Sender<Counts>,
}

impl Overlays {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next: AtomicU64::new(0),
            pages: Mutex::new(HashMap::new()),
            counts: watch::channel(Counts::default()).0,
        })
    }

    pub fn counts(&self) -> Counts {
        *self.counts.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<Counts> {
        self.counts.subscribe()
    }

    /// Counts a page for as long as the returned guard lives.
    pub fn join(self: &Arc<Self>) -> Page {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.update(|pages| {
            pages.insert(id, None);
        });
        Page {
            id,
            overlays: self.clone(),
        }
    }

    fn update(&self, change: impl FnOnce(&mut HashMap<u64, Option<bool>>)) {
        let mut pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut pages);
        let counts = Counts {
            count: pages.len(),
            active: pages.values().filter(|&&a| a == Some(true)).count(),
        };
        self.counts.send_if_modified(|c| {
            let changed = *c != counts;
            *c = counts;
            changed
        });
    }
}

/// A connected overlay page (see [`Overlays::join`]).
pub(crate) struct Page {
    id: u64,
    overlays: Arc<Overlays>,
}

impl Page {
    /// OBS said whether the page is on stream.
    pub fn set_active(&self, active: bool) {
        self.overlays.update(|pages| {
            pages.insert(self.id, Some(active));
        });
    }

    /// Whether the slate the page shows may count as covering the stream:
    /// not once OBS said it is off stream, where viewers do not see it. Before
    /// OBS says anything (after it starts), the page is most likely on stream.
    pub fn may_confirm(&self) -> bool {
        let pages = self
            .overlays
            .pages
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        pages.get(&self.id) != Some(&Some(false))
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.overlays.update(|pages| {
            pages.remove(&self.id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(count: usize, active: usize) -> Counts {
        Counts { count, active }
    }

    #[test]
    fn only_pages_obs_said_are_on_stream_count_as_active() {
        let o = Overlays::new();
        let a = o.join();
        let b = o.join();
        assert_eq!(o.counts(), counts(2, 0));
        a.set_active(true);
        assert_eq!(o.counts(), counts(2, 1));
        b.set_active(false);
        assert_eq!(o.counts(), counts(2, 1));
        a.set_active(false);
        assert_eq!(o.counts(), counts(2, 0));
        a.set_active(true);
        drop(a);
        assert_eq!(o.counts(), counts(1, 0));
        drop(b);
        assert_eq!(o.counts(), Counts::default());
    }

    #[test]
    fn a_page_obs_said_is_off_stream_cannot_confirm_the_slate() {
        let o = Overlays::new();
        let page = o.join();
        // Not said yet (after OBS starts): most likely on stream.
        assert!(page.may_confirm());
        page.set_active(false);
        assert!(!page.may_confirm());
        page.set_active(true);
        assert!(page.may_confirm());
    }
}
