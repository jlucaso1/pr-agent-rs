use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use tokio::sync::Notify;

use crate::config::loader::get_settings;

/// Global push deduplicator instance.
static PUSH_DEDUP: LazyLock<Arc<PushDeduplicator>> =
    LazyLock::new(|| Arc::new(PushDeduplicator::new()));

/// Result of attempting to acquire a push slot.
pub enum AcquireResult {
    /// Slot acquired — proceed immediately.
    Proceed(PushGuard),
    /// Slot acquired but must wait for the first task to finish.
    Wait(PushGuard, Arc<Notify>),
    /// Rejected — too many active tasks for this URL.
    Rejected,
}

/// RAII guard that decrements the active task count and notifies waiters on drop.
pub struct PushGuard {
    api_url: String,
    dedup: Arc<PushDeduplicator>,
}

impl Drop for PushGuard {
    fn drop(&mut self) {
        self.dedup.release(&self.api_url);
    }
}

/// Per-URL tracking entry.
struct Entry {
    active_count: u32,
    notify: Arc<Notify>,
    last_access: Instant,
}

/// Deduplicates concurrent push triggers for the same PR URL.
///
/// Mirrors Python's `DefaultDictWithTimeout` + `asyncio.Condition` pattern:
/// - First push trigger proceeds immediately
/// - Second push trigger waits until the first finishes (if backlog enabled)
/// - Further push triggers are rejected (discarded)
/// - Entries expire after TTL seconds
pub struct PushDeduplicator {
    entries: Mutex<HashMap<String, Entry>>,
}

impl PushDeduplicator {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Try to acquire a slot for the given PR API URL.
    fn try_acquire(
        self: &Arc<Self>,
        api_url: &str,
        max_tasks: u32,
        ttl_secs: u64,
    ) -> AcquireResult {
        let mut map = self.entries.lock().unwrap();

        // Clean expired entries opportunistically
        let now = Instant::now();
        map.retain(|_, e| now.duration_since(e.last_access).as_secs() < ttl_secs);

        let entry = map.entry(api_url.to_string()).or_insert_with(|| Entry {
            active_count: 0,
            notify: Arc::new(Notify::new()),
            last_access: now,
        });

        entry.last_access = now;

        if entry.active_count < max_tasks {
            let current = entry.active_count;
            entry.active_count += 1;
            let notify = entry.notify.clone();
            let guard = PushGuard {
                api_url: api_url.to_string(),
                dedup: Arc::clone(self),
            };

            if current == 0 {
                AcquireResult::Proceed(guard)
            } else {
                // Second task — must wait
                AcquireResult::Wait(guard, notify)
            }
        } else {
            AcquireResult::Rejected
        }
    }

    /// Release a slot: decrement count and notify one waiter.
    fn release(&self, api_url: &str) {
        let mut map = self.entries.lock().unwrap();
        if let Some(entry) = map.get_mut(api_url) {
            entry.active_count = entry.active_count.saturating_sub(1);
            let notify = entry.notify.clone();
            drop(map);
            // Wake one waiting task
            notify.notify_one();
        }
    }
}

/// Try to acquire a push dedup slot for the given PR URL.
///
/// Returns `Some(guard)` if the task should proceed (after optionally waiting),
/// or `None` if the task should be discarded.
///
/// The caller must hold the returned `PushGuard` for the duration of processing.
/// When the guard is dropped, the slot is released and waiting tasks are notified.
pub async fn acquire_push_slot(api_url: &str) -> Option<PushGuard> {
    let settings = get_settings();
    let max_tasks = if settings.github_app.push_trigger_pending_tasks_backlog {
        2
    } else {
        1
    };
    let ttl_secs = settings.github_app.push_trigger_pending_tasks_ttl;

    match PUSH_DEDUP.try_acquire(api_url, max_tasks, ttl_secs) {
        AcquireResult::Proceed(guard) => {
            tracing::info!(api_url, "push dedup: proceeding (first task)");
            Some(guard)
        }
        AcquireResult::Wait(guard, notify) => {
            tracing::info!(api_url, "push dedup: waiting for first task to complete");
            notify.notified().await;
            tracing::info!(api_url, "push dedup: wait finished, proceeding");
            Some(guard)
        }
        AcquireResult::Rejected => {
            tracing::info!(api_url, "push dedup: rejected (too many active tasks)");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: u64 = 300;

    fn make_dedup() -> Arc<PushDeduplicator> {
        Arc::new(PushDeduplicator::new())
    }

    #[test]
    fn test_first_task_proceeds() {
        let dedup = make_dedup();
        let result = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 2, TTL);
        assert!(matches!(result, AcquireResult::Proceed(_)));
    }

    #[test]
    fn test_second_task_waits_with_backlog() {
        let dedup = make_dedup();
        let _g1 = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 2, TTL);
        let result = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 2, TTL);
        assert!(matches!(result, AcquireResult::Wait(_, _)));
    }

    #[test]
    fn test_third_task_rejected_with_backlog() {
        let dedup = make_dedup();
        let _g1 = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 2, TTL);
        let _g2 = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 2, TTL);
        let result = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 2, TTL);
        assert!(matches!(result, AcquireResult::Rejected));
    }

    #[test]
    fn test_second_task_rejected_without_backlog() {
        let dedup = make_dedup();
        let _g1 = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 1, TTL);
        let result = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 1, TTL);
        assert!(matches!(result, AcquireResult::Rejected));
    }

    #[test]
    fn test_different_urls_independent() {
        let dedup = make_dedup();
        let _g1 = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 1, TTL);
        let result = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/2", 1, TTL);
        assert!(matches!(result, AcquireResult::Proceed(_)));
    }

    #[test]
    fn test_release_allows_new_task() {
        let dedup = make_dedup();
        {
            let _g1 = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 1, TTL);
            // g1 dropped here → release called
        }
        let result = dedup.try_acquire("https://api.github.com/repos/o/r/pulls/1", 1, TTL);
        assert!(matches!(result, AcquireResult::Proceed(_)));
    }

    #[tokio::test]
    async fn test_wait_and_proceed() {
        let dedup = make_dedup();
        let url = "https://api.github.com/repos/o/r/pulls/99";

        // First task acquires
        let g1 = match dedup.try_acquire(url, 2, TTL) {
            AcquireResult::Proceed(g) => g,
            _ => panic!("expected Proceed"),
        };

        let dedup2 = dedup.clone();
        let url2 = url.to_string();

        // Second task should wait, then proceed after first releases
        let handle = tokio::spawn(async move {
            match dedup2.try_acquire(&url2, 2, TTL) {
                AcquireResult::Wait(_g, notify) => {
                    notify.notified().await;
                    true
                }
                _ => false,
            }
        });

        // Give the spawned task time to start waiting
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Release first task
        drop(g1);

        let waited = handle.await.unwrap();
        assert!(waited, "second task should have waited and then proceeded");
    }

    #[test]
    fn test_ttl_expires_entries() {
        let dedup = make_dedup();
        let url = "https://api.github.com/repos/o/r/pulls/1";

        // Acquire with TTL=0 (already expired)
        let _g1 = dedup.try_acquire(url, 1, 0);
        // g1 is still held, but TTL=0 means the entry expires on next access
        drop(_g1);

        // Next acquire with TTL=0 should clean up and succeed
        let result = dedup.try_acquire(url, 1, 0);
        assert!(matches!(result, AcquireResult::Proceed(_)));
    }
}
