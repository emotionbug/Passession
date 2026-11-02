//! Read-only native GET seam shared by child discovery and opened-child replay.
//! Live sessions read through their runtime; tests substitute a recording fake.

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Method;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::runtime::Runtime;

#[async_trait]
pub(super) trait NativeReader: Send + Sync {
    async fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value>;
}

pub(super) struct RuntimeReader<'a> {
    runtime: &'a Runtime,
    cancel: CancellationToken,
}

impl<'a> RuntimeReader<'a> {
    pub fn new(runtime: &'a Runtime, cancel: CancellationToken) -> Self {
        Self { runtime, cancel }
    }
}

#[async_trait]
impl<'a> NativeReader for RuntimeReader<'a> {
    async fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value> {
        self.runtime
            .request(Method::GET, path, query, None, self.cancel.clone())
            .await
    }
}

/// Answers by `path?query` (or bare path) and records every read in order.
/// Unknown routes fail the way a missing native session does.
#[cfg(test)]
#[derive(Default)]
pub(super) struct FakeNative {
    replies: std::collections::HashMap<String, Value>,
    reads: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeNative {
    pub fn reply(mut self, route: &str, value: Value) -> Self {
        self.replies.insert(route.to_owned(), value);
        self
    }

    pub fn reads(&self) -> Vec<String> {
        self.reads.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl NativeReader for FakeNative {
    async fn get(&self, path: &str, query: &[(String, String)]) -> Result<Value> {
        let route = if query.is_empty() {
            path.to_owned()
        } else {
            let query = query
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("&");
            format!("{path}?{query}")
        };
        self.reads.lock().unwrap().push(route.clone());
        tokio::task::yield_now().await;
        self.replies
            .get(&route)
            .or_else(|| self.replies.get(path))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("OpenCode GET {route} failed with HTTP 404"))
    }
}
