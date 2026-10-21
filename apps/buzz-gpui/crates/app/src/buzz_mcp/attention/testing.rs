//! An agent's attention over one in-memory record, as the service builds
//! it, for tests.

use super::Attention;
use crate::agents2::objects::{
    ClassifierAvailability, Shown, WatchFilter, WatchSummary, WriteOptions, checked_write,
    interest_ids, show, watch_summaries,
};
use crate::agents2::store::{AgentRecord, set_attention};
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub(crate) struct MemoryAttention {
    pub record: Mutex<AgentRecord>,
    pub now: AtomicU64,
    pub enabled: AtomicBool,
    pub classifier: bool,
}

impl MemoryAttention {
    pub fn new(now: u64) -> Self {
        Self {
            record: Mutex::new(AgentRecord::blank(&"c".repeat(64))),
            now: AtomicU64::new(now),
            enabled: AtomicBool::new(true),
            classifier: false,
        }
    }

    pub fn record(&self) -> AgentRecord {
        self.record.lock().unwrap().clone()
    }

    /// Sets `slug` as the owner would, unchecked.
    pub fn set(&self, slug: &str, value: Option<Value>) {
        let mut record = self.record.lock().unwrap();
        *record = set_attention(&record, slug, value.as_ref(), self.now(), None).unwrap();
    }
}

impl Attention for MemoryAttention {
    fn enabled(&self) -> BoxFuture<'static, Result<bool, String>> {
        let enabled = self.enabled.load(Ordering::SeqCst);
        async move { Ok(enabled) }.boxed()
    }
    fn classifier(&self) -> ClassifierAvailability {
        if self.classifier {
            ClassifierAvailability::Available
        } else {
            ClassifierAvailability::Unavailable
        }
    }
    fn show(
        &self,
        slug: String,
        interest: Option<String>,
    ) -> BoxFuture<'static, Result<Shown, String>> {
        let shown = show(&self.record(), &slug, self.now(), interest.as_deref())
            .map_err(|error| error.to_string());
        async move { shown }.boxed()
    }
    fn interests(&self, search: String) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let ids = interest_ids(&self.record(), &search);
        async move { Ok(ids) }.boxed()
    }
    fn watches(
        &self,
        filter: WatchFilter,
    ) -> BoxFuture<'static, Result<Vec<WatchSummary>, String>> {
        let watches = watch_summaries(&self.record(), &filter, self.now());
        async move { Ok(watches) }.boxed()
    }
    fn write(
        &self,
        slug: String,
        value: Option<Value>,
        options: WriteOptions,
    ) -> BoxFuture<'static, Result<Shown, String>> {
        let now = self.now();
        let mut record = self.record.lock().unwrap();
        let written = checked_write(&record, &slug, value.as_ref(), &options, now)
            .map_err(|error| error.to_string())
            .map(|after| {
                *record = after;
                show(&record, &slug, now, None).expect("unscoped")
            });
        async move { written }.boxed()
    }
    fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}
