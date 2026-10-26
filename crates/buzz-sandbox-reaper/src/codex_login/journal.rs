//! Strongly consistent, metadata-only pointer to the committed secret version.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use aws_config::timeout::TimeoutConfig;
use aws_config::{defaults, BehaviorVersion};
use aws_sdk_dynamodb::{types::AttributeValue, Client};

use super::Error;
use crate::manager::Developer;

pub(super) type JournalResult<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

pub(super) trait Journal: Send + Sync {
    fn head<'a>(&'a self, developer: &'a Developer) -> JournalResult<'a, Option<String>>;
    fn advance<'a>(
        &'a self,
        developer: &'a Developer,
        expected: Option<&'a str>,
        revision: &'a str,
    ) -> JournalResult<'a, ()>;
}

pub(super) struct DynamoJournal {
    client: Client,
    table: String,
}

impl DynamoJournal {
    pub async fn new(table: String) -> Result<Self, Error> {
        if table.is_empty() {
            return Err(Error::NotConfigured);
        }
        let config = defaults(BehaviorVersion::latest())
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Duration::from_secs(5))
                    .operation_attempt_timeout(Duration::from_secs(10))
                    .operation_timeout(Duration::from_secs(25))
                    .build(),
            )
            .load()
            .await;
        Ok(Self {
            client: Client::new(&config),
            table,
        })
    }
}

impl Journal for DynamoJournal {
    fn head<'a>(&'a self, developer: &'a Developer) -> JournalResult<'a, Option<String>> {
        Box::pin(async move {
            let output = self
                .client
                .get_item()
                .table_name(&self.table)
                .key("developer", AttributeValue::S(developer.id.clone()))
                .consistent_read(true)
                .send()
                .await
                .map_err(|_| Error::Storage)?;
            let Some(item) = output.item else {
                return Ok(None);
            };
            item.get("revision")
                .and_then(|v| v.as_s().ok())
                .cloned()
                .map(Some)
                .ok_or(Error::Storage)
        })
    }

    fn advance<'a>(
        &'a self,
        developer: &'a Developer,
        expected: Option<&'a str>,
        revision: &'a str,
    ) -> JournalResult<'a, ()> {
        Box::pin(async move {
            let mut request = self
                .client
                .update_item()
                .table_name(&self.table)
                .key("developer", AttributeValue::S(developer.id.clone()))
                .update_expression("SET revision = :next")
                .expression_attribute_values(":next", AttributeValue::S(revision.to_owned()));
            request = if let Some(expected) = expected {
                request
                    .condition_expression("revision = :previous")
                    .expression_attribute_values(
                        ":previous",
                        AttributeValue::S(expected.to_owned()),
                    )
            } else {
                request.condition_expression("attribute_not_exists(revision)")
            };
            // A lost success response is safe: the caller never contacts OAuth.
            // A fresh consistent read recovers the committed version.
            request.send().await.map_err(|_| Error::Storage)?;
            Ok(())
        })
    }
}
