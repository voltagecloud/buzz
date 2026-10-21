//! What an agent's attention handle asks of the service
//! ([`super::super::runtime::AgentAttention`]): each call reads the saved
//! record as it is now, so it is current however long the run lasts.
//! Calls run one at a time on the service's thread, so a write's token is
//! checked against the record it replaces and nothing races it.

use super::{AgentAttention, AgentRecord, Agents2, now_secs};
use crate::agents2::objects::{
    Shown, WatchFilter, WatchSummary, WriteOptions, checked_write, interest_ids, show,
    watch_summaries,
};
use gpui_kit::Context;
use serde_json::Value;

impl Agents2 {
    /// The agent's attention as its handle offers it.
    pub(crate) fn attention(&self, pubkey: &str) -> AgentAttention {
        AgentAttention {
            pubkey: pubkey.into(),
            calls: self.attention_calls.clone(),
        }
    }

    fn attention_record(
        &mut self,
        pubkey: &str,
        cx: &Context<Self>,
    ) -> Result<AgentRecord, String> {
        let identity = self
            .live(pubkey)
            .ok_or_else(|| "No such agent on this device".to_string())?;
        Ok(self.record_of(&identity.summary, cx))
    }

    pub(crate) fn attention_enabled(
        &mut self,
        pubkey: &str,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        Ok(self.attention_record(pubkey, cx)?.attention_on())
    }

    pub(crate) fn attention_show(
        &mut self,
        pubkey: &str,
        slug: &str,
        interest: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<Shown, String> {
        let record = self.attention_record(pubkey, cx)?;
        show(&record, slug, now_secs(), interest).map_err(|error| error.to_string())
    }

    pub(crate) fn attention_interests(
        &mut self,
        pubkey: &str,
        search: &str,
        cx: &mut Context<Self>,
    ) -> Result<Vec<String>, String> {
        Ok(interest_ids(&self.attention_record(pubkey, cx)?, search))
    }

    pub(crate) fn attention_watches(
        &mut self,
        pubkey: &str,
        filter: &WatchFilter,
        cx: &mut Context<Self>,
    ) -> Result<Vec<WatchSummary>, String> {
        let record = self.attention_record(pubkey, cx)?;
        Ok(watch_summaries(&record, filter, now_secs()))
    }

    pub(crate) fn attention_write(
        &mut self,
        pubkey: &str,
        slug: &str,
        value: Option<&Value>,
        options: &WriteOptions,
        cx: &mut Context<Self>,
    ) -> Result<Shown, String> {
        let record = self.attention_record(pubkey, cx)?;
        let after = checked_write(&record, slug, value, options, now_secs())
            .map_err(|error| error.to_string())?;
        self.write(after.clone())?;
        self.update(cx);
        cx.notify();
        show(&after, slug, now_secs(), None).map_err(|error| error.to_string())
    }
}
