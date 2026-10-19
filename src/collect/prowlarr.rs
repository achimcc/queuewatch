//! Prowlarr: how many indexers there are and how many are working.
//!
//! Counts only. An indexer's name is read by nobody here — the struct below
//! has no field for it — because a private tracker's name is the thing its
//! members are asked not to spread. Fields from Prowlarr 2.6.5.5623's
//! OpenAPI file (`IndexerResource`, `IndexerStatusResource`).

use std::collections::HashSet;

use jiff::Timestamp;
use reqwest::Client;
use serde::Deserialize;
use url::Url;

use super::http::{self, FetchError};
use super::merge::Contribution;
use super::parse_time;
use crate::snapshot::IndexerSummary;

pub struct Prowlarr {
    pub label: String,
    base: Url,
    key: String,
    http: Client,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Indexer {
    id: Option<i64>,
    enable: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct IndexerStatus {
    indexer_id: Option<i64>,
    disabled_till: Option<String>,
}

impl Prowlarr {
    pub fn new(label: &str, url: &str, key: String, http: Client) -> anyhow::Result<Self> {
        Ok(Prowlarr {
            label: label.to_string(),
            base: http::base_url(url)?,
            key,
            http,
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, FetchError> {
        let url = http::endpoint(&self.base, path, &[]);
        http::get_json(self.http.get(url).header("X-Api-Key", &self.key)).await
    }

    pub async fn fetch(&self) -> Result<Contribution, FetchError> {
        let (indexers, statuses) = tokio::try_join!(
            self.get::<Vec<Indexer>>("/api/v1/indexer"),
            self.get::<Vec<IndexerStatus>>("/api/v1/indexerstatus"),
        )?;
        Ok(Contribution {
            indexers: Some(summarize(&indexers, &statuses, Timestamp::now())),
            ..Default::default()
        })
    }
}

/// An indexer is unhealthy while Prowlarr has it backed off
/// (`disabledTill` in the future). A past failure it has recovered from is
/// history, not a problem.
fn summarize(indexers: &[Indexer], statuses: &[IndexerStatus], now: Timestamp) -> IndexerSummary {
    let backed_off: HashSet<i64> = statuses
        .iter()
        .filter(|s| parse_time(s.disabled_till.as_deref()).is_some_and(|t| t > now))
        .filter_map(|s| s.indexer_id)
        .collect();
    let enabled: Vec<&Indexer> = indexers.iter().filter(|i| i.enable == Some(true)).collect();
    let healthy = enabled
        .iter()
        .filter(|i| i.id.is_none_or(|id| !backed_off.contains(&id)))
        .count();
    IndexerSummary {
        enabled: enabled.len() as u32,
        healthy: healthy as u32,
        known: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backed_off_indexer_is_enabled_but_not_healthy() {
        let now: Timestamp = "2026-10-19T08:00:00Z".parse().unwrap();
        let indexers = [
            Indexer {
                id: Some(1),
                enable: Some(true),
            },
            Indexer {
                id: Some(2),
                enable: Some(true),
            },
            Indexer {
                id: Some(3),
                enable: Some(false),
            },
            Indexer {
                id: Some(4),
                enable: Some(true),
            },
        ];
        let statuses = [
            IndexerStatus {
                indexer_id: Some(2),
                disabled_till: Some("2026-10-19T09:00:00Z".into()),
            },
            IndexerStatus {
                indexer_id: Some(4),
                disabled_till: Some("2026-10-18T09:00:00Z".into()),
            },
        ];
        let s = summarize(&indexers, &statuses, now);
        assert_eq!(
            s,
            IndexerSummary {
                enabled: 3,
                healthy: 2,
                known: true
            }
        );
    }
}
