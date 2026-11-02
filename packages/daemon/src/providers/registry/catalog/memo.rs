//! Provider-owned, single-flight memo for discovery a provider repeats while
//! opening native sessions (models, config, skills, capability probes).
//!
//! The catalog service above already shares picker snapshots. This memo covers
//! the per-session copy of that discovery: concurrent opens with the same
//! launch identity wait for one load, different identities never wait on each
//! other, and only complete results are kept, for at most `TTL`. A failed,
//! partial or dropped load leaves nothing behind, so the next caller retries
//! with its own process. Every caller, waiting or loading, still stops on its
//! own cancellation. Each provider instance owns its memo, so a provider
//! configuration change discards it with the old instance; a forced catalog
//! refresh clears it explicitly.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};
use tokio::{sync::OnceCell, time::Instant};
use tokio_util::sync::CancellationToken;

/// How long a complete discovery stays reusable. Files the key fingerprints
/// invalidate sooner; this bounds staleness for everything a provider cannot
/// cheaply observe (server-side model lists, nested skill files).
pub(crate) const TTL: Duration = Duration::from_secs(60);

/// What one load produced. Partial results reach their caller but are never
/// shared, because a later open can still obtain the complete answer.
pub(crate) enum Loaded<T> {
    Complete(T),
    Partial(T),
}

impl<T> Loaded<T> {
    pub(crate) fn into_value(self) -> T {
        match self {
            Self::Complete(value) | Self::Partial(value) => value,
        }
    }
}

struct Stored<T> {
    at: Instant,
    value: T,
}

enum Unshared<T> {
    Partial(T),
    Failed(anyhow::Error),
}

pub(crate) struct Memo<T> {
    slots: Mutex<HashMap<String, Arc<OnceCell<Stored<T>>>>>,
}

impl<T> Default for Memo<T> {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

impl<T: Clone> Memo<T> {
    pub(crate) async fn get_or_load<F, Fut>(
        &self,
        key: String,
        cancel: &CancellationToken,
        load: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Loaded<T>>>,
    {
        let slot = {
            let mut slots = self.slots.lock().unwrap();
            let now = Instant::now();
            slots.retain(|_, slot| match slot.get() {
                Some(stored) => now.duration_since(stored.at) < TTL,
                // An unfinished slot nobody awaits belongs to a dropped load.
                None => Arc::strong_count(slot) > 1,
            });
            slots.entry(key).or_default().clone()
        };
        let stored = slot.get_or_try_init(|| async move {
            match load().await {
                Ok(Loaded::Complete(value)) => Ok(Stored {
                    at: Instant::now(),
                    value,
                }),
                Ok(Loaded::Partial(value)) => Err(Unshared::Partial(value)),
                Err(error) => Err(Unshared::Failed(error)),
            }
        });
        let stored = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(anyhow!("Provider discovery canceled")),
            stored = stored => stored,
        };
        match stored {
            Ok(stored) => Ok(stored.value.clone()),
            Err(Unshared::Partial(value)) => Ok(value),
            Err(Unshared::Failed(error)) => Err(error),
        }
    }

    /// Loads already in flight finish for their own waiters only.
    pub(crate) fn clear(&self) {
        self.slots.lock().unwrap().clear();
    }
}

/// Stable digest of a launch identity, so cached keys never retain secrets
/// from the environment they were derived from.
pub(crate) fn key(identity: &Value) -> String {
    hex::encode(Sha256::digest(identity.to_string().as_bytes()))
}

/// The environment that can change what a provider discovers. Paseo names the
/// agent in `PASEO_AGENT_ID`; providers cannot observe it during discovery,
/// and keeping it would give every agent a private entry.
pub(crate) fn shared_env(env: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    env.iter()
        .filter(|(name, _)| name.as_str() != "PASEO_AGENT_ID")
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// Size and modification time of each path (null when absent), so editing a
/// provider's config, credentials or skill directory changes the key at once.
pub(crate) async fn fingerprint(paths: &[PathBuf]) -> Value {
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        let entry = match tokio::fs::metadata(path).await {
            Ok(metadata) => {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|elapsed| elapsed.as_nanos().to_string());
                json!([path.to_string_lossy(), metadata.len(), modified])
            }
            Err(_) => json!([path.to_string_lossy(), null, null]),
        };
        entries.push(entry);
    }
    Value::Array(entries)
}

#[cfg(test)]
mod tests {
    use super::{Loaded, Memo, TTL, fingerprint, key, shared_env};
    use anyhow::anyhow;
    use serde_json::json;
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::{Barrier, Notify};
    use tokio_util::sync::CancellationToken;

    fn counted(loads: &Arc<AtomicUsize>, value: &str) -> Result<Loaded<String>, anyhow::Error> {
        loads.fetch_add(1, Ordering::SeqCst);
        Ok(Loaded::Complete(value.to_owned()))
    }

    #[tokio::test]
    async fn concurrent_opens_with_one_identity_share_one_load() {
        let memo = Arc::new(Memo::<String>::default());
        let loads = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let started = Arc::new(Barrier::new(9));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (memo, loads, release, started) = (
                memo.clone(),
                loads.clone(),
                release.clone(),
                started.clone(),
            );
            tasks.push(tokio::spawn(async move {
                started.wait().await;
                memo.get_or_load("same".into(), &CancellationToken::new(), || async {
                    release.notified().await;
                    counted(&loads, "models")
                })
                .await
            }));
        }
        started.wait().await;
        tokio::task::yield_now().await;
        release.notify_one();
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), "models");
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let reused = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                counted(&loads, "other")
            })
            .await
            .unwrap();
        assert_eq!(reused, "models");
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_identities_neither_share_nor_wait_for_each_other() {
        let memo = Arc::new(Memo::<String>::default());
        let blocked = Arc::new(Notify::new());
        let slow = {
            let (memo, blocked) = (memo.clone(), blocked.clone());
            tokio::spawn(async move {
                memo.get_or_load("cwd-a".into(), &CancellationToken::new(), || async move {
                    blocked.notified().await;
                    Ok(Loaded::Complete("a".to_owned()))
                })
                .await
            })
        };
        tokio::task::yield_now().await;
        let other = tokio::time::timeout(
            Duration::from_secs(5),
            memo.get_or_load("cwd-b".into(), &CancellationToken::new(), || async {
                Ok(Loaded::Complete("b".to_owned()))
            }),
        )
        .await
        .expect("an unrelated identity must not wait for another load")
        .unwrap();
        assert_eq!(other, "b");
        blocked.notify_one();
        assert_eq!(slow.await.unwrap().unwrap(), "a");
    }

    #[tokio::test]
    async fn failure_is_returned_once_and_the_next_open_retries() {
        let memo = Memo::<String>::default();
        let loads = Arc::new(AtomicUsize::new(0));
        let failed = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                loads.fetch_add(1, Ordering::SeqCst);
                Err(anyhow!("app-server exited"))
            })
            .await;
        assert_eq!(failed.unwrap_err().to_string(), "app-server exited");
        let retried = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                counted(&loads, "models")
            })
            .await
            .unwrap();
        assert_eq!(retried, "models");
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn waiters_retry_with_their_own_load_when_the_shared_one_fails() {
        let memo = Arc::new(Memo::<String>::default());
        let release = Arc::new(Notify::new());
        let first = {
            let (memo, release) = (memo.clone(), release.clone());
            tokio::spawn(async move {
                memo.get_or_load("same".into(), &CancellationToken::new(), || async move {
                    release.notified().await;
                    Err::<Loaded<String>, _>(anyhow!("first process died"))
                })
                .await
            })
        };
        tokio::task::yield_now().await;
        let second = {
            let memo = memo.clone();
            tokio::spawn(async move {
                memo.get_or_load("same".into(), &CancellationToken::new(), || async {
                    Ok(Loaded::Complete("second".to_owned()))
                })
                .await
            })
        };
        tokio::task::yield_now().await;
        release.notify_one();
        assert!(first.await.unwrap().is_err());
        assert_eq!(second.await.unwrap().unwrap(), "second");
    }

    #[tokio::test]
    async fn a_dropped_load_does_not_poison_the_identity() {
        let memo = Memo::<String>::default();
        let canceled = tokio::time::timeout(
            Duration::from_millis(10),
            memo.get_or_load("same".into(), &CancellationToken::new(), || {
                std::future::pending()
            }),
        )
        .await;
        assert!(canceled.is_err());
        let next = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                Ok(Loaded::Complete("models".to_owned()))
            })
            .await
            .unwrap();
        assert_eq!(next, "models");
    }

    #[tokio::test]
    async fn a_canceled_waiter_stops_without_disturbing_the_shared_load() {
        let memo = Arc::new(Memo::<String>::default());
        let release = Arc::new(Notify::new());
        let loader = {
            let (memo, release) = (memo.clone(), release.clone());
            tokio::spawn(async move {
                memo.get_or_load("same".into(), &CancellationToken::new(), || async move {
                    release.notified().await;
                    Ok(Loaded::Complete("models".to_owned()))
                })
                .await
            })
        };
        tokio::task::yield_now().await;
        let canceled = CancellationToken::new();
        canceled.cancel();
        let waiter = memo
            .get_or_load("same".into(), &canceled, || async {
                Ok(Loaded::Complete("unused".to_owned()))
            })
            .await;
        assert!(waiter.is_err());
        release.notify_one();
        assert_eq!(loader.await.unwrap().unwrap(), "models");
        let reused = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                Ok(Loaded::Complete("unused".to_owned()))
            })
            .await
            .unwrap();
        assert_eq!(reused, "models");
    }

    #[tokio::test]
    async fn partial_results_reach_their_caller_but_are_not_shared() {
        let memo = Memo::<String>::default();
        let loads = Arc::new(AtomicUsize::new(0));
        let partial = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(Loaded::Partial("without skills".to_owned()))
            })
            .await
            .unwrap();
        assert_eq!(partial, "without skills");
        let complete = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                counted(&loads, "with skills")
            })
            .await
            .unwrap();
        assert_eq!(complete, "with skills");
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn entries_expire_after_the_ttl() {
        let memo = Memo::<String>::default();
        let loads = Arc::new(AtomicUsize::new(0));
        memo.get_or_load("same".into(), &CancellationToken::new(), || async {
            counted(&loads, "old")
        })
        .await
        .unwrap();
        tokio::time::advance(TTL - Duration::from_secs(1)).await;
        let cached = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                counted(&loads, "new")
            })
            .await
            .unwrap();
        assert_eq!(cached, "old");
        tokio::time::advance(Duration::from_secs(1)).await;
        let refreshed = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                counted(&loads, "new")
            })
            .await
            .unwrap();
        assert_eq!(refreshed, "new");
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn clear_forces_the_next_open_to_discover_again() {
        let memo = Memo::<String>::default();
        let loads = Arc::new(AtomicUsize::new(0));
        memo.get_or_load("same".into(), &CancellationToken::new(), || async {
            counted(&loads, "old")
        })
        .await
        .unwrap();
        memo.clear();
        let refreshed = memo
            .get_or_load("same".into(), &CancellationToken::new(), || async {
                counted(&loads, "new")
            })
            .await
            .unwrap();
        assert_eq!(refreshed, "new");
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn agent_identity_is_not_part_of_the_shared_environment() {
        let first = BTreeMap::from([
            ("PASEO_AGENT_ID".to_owned(), "agent-a".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "key-a".to_owned()),
        ]);
        let mut second = first.clone();
        second.insert("PASEO_AGENT_ID".into(), "agent-b".into());
        assert_eq!(
            key(&json!(shared_env(&first))),
            key(&json!(shared_env(&second)))
        );
        second.insert("OPENAI_API_KEY".into(), "key-b".into());
        assert_ne!(
            key(&json!(shared_env(&first))),
            key(&json!(shared_env(&second)))
        );
        assert!(!key(&json!(shared_env(&second))).contains("key-b"));
    }

    #[tokio::test]
    async fn fingerprint_changes_when_a_watched_file_appears_or_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let paths = [path.clone()];
        let absent = fingerprint(&paths).await;
        std::fs::write(&path, "model = \"a\"").unwrap();
        let present = fingerprint(&paths).await;
        assert_ne!(absent, present);
        std::fs::write(&path, "model = \"bb\"").unwrap();
        assert_ne!(present, fingerprint(&paths).await);
    }
}
