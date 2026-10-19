//! slskd: Soulseek downloads, one item per album folder.
//!
//! `GET /api/v0/transfers/downloads` (slskd 0.24.5,
//! `Transfers/API/Controllers/TransfersController.cs`) answers users →
//! directories → files. A person thinks in albums, so the files of one
//! directory become one item, named after the folder.
//!
//! The remote user name is grouped over and dropped: it says who shares
//! what, which is nobody's business on a status page. Of the remote path
//! only the last folder name survives. `exception` and `stateDescription`
//! (free text) are not read.

use reqwest::Client;
use serde::Deserialize;
use url::Url;

use super::http::{self, FetchError};
use super::merge::{Contribution, Tracked};
use super::title::client_title;
use super::{Round, eta_in, fraction};
use crate::snapshot::{ItemState, MediaIds, MediaKind, Origin, QueueItem, Transport};

pub struct Slskd {
    pub label: String,
    base: Url,
    key: String,
    http: Client,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct User {
    directories: Vec<Directory>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Directory {
    directory: Option<String>,
    files: Vec<File>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct File {
    size: Option<i64>,
    /// A flags enum written as names: "Queued, Remotely", "InProgress",
    /// "Completed, Succeeded", "Completed, Errored".
    state: Option<String>,
    bytes_transferred: Option<i64>,
    average_speed: Option<f64>,
}

fn flags(state: &Option<String>) -> Vec<&str> {
    state
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .collect()
}

/// The last component of a Soulseek path, which uses `\` as often as `/`.
fn folder_name(directory: &str) -> Option<&str> {
    directory
        .split(['\\', '/'])
        .map(str::trim)
        .rfind(|p| !p.is_empty())
}

fn group(directory: &Directory, round: &Round) -> Option<Tracked> {
    let title = client_title(folder_name(directory.directory.as_deref()?))?;
    let files: Vec<(&File, Vec<&str>)> = directory
        .files
        .iter()
        .map(|f| (f, flags(&f.state)))
        .collect();
    // A folder that is done — every file completed, well or not — is no
    // longer a download, however long slskd keeps listing it.
    if files.iter().all(|(_, s)| s.contains(&"Completed")) {
        return None;
    }
    let in_progress = files.iter().any(|(_, s)| s.contains(&"InProgress"));
    let failed = files.iter().any(|(_, s)| {
        s.iter()
            .any(|f| matches!(*f, "Errored" | "Rejected" | "TimedOut"))
    });
    let starting = files.iter().any(|(_, s)| s.contains(&"Initializing"));
    let state = if in_progress || starting {
        ItemState::Downloading
    } else if failed {
        ItemState::Warning
    } else {
        ItemState::Queued
    };
    let size: i64 = files.iter().map(|(f, _)| f.size.unwrap_or(0).max(0)).sum();
    let done: i64 = files
        .iter()
        .map(|(f, _)| f.bytes_transferred.unwrap_or(0).max(0))
        .sum();
    let left = (size - done).max(0);
    let speed: f64 = files
        .iter()
        .filter(|(_, s)| s.contains(&"InProgress"))
        .map(|(f, _)| f.average_speed.unwrap_or(0.0))
        .sum();
    let eta = if in_progress && speed > 0.0 {
        eta_in(round.now, left as f64 / speed)
    } else {
        None
    };
    Some(Tracked {
        item: QueueItem {
            kind: MediaKind::Other,
            title,
            ids: MediaIds::default(),
            transport: Transport::Soulseek,
            state,
            origin: Origin::Client,
            progress: fraction(done as f64, size as f64),
            size_bytes: Some(size as u64).filter(|s| *s > 0),
            left_bytes: Some(left as u64),
            eta,
        },
        download_id: None,
    })
}

impl Slskd {
    pub fn new(label: &str, url: &str, key: String, http: Client) -> anyhow::Result<Self> {
        Ok(Slskd {
            label: label.to_string(),
            base: http::base_url(url)?,
            key,
            http,
        })
    }

    pub async fn fetch(&self, round: &Round) -> Result<Contribution, FetchError> {
        let url = http::endpoint(&self.base, "/api/v0/transfers/downloads", &[]);
        let users: Vec<User> =
            http::get_json(self.http.get(url).header("X-API-Key", &self.key)).await?;
        let client = users
            .iter()
            .flat_map(|u| u.directories.iter())
            .filter_map(|d| group(d, round))
            .collect();
        Ok(Contribution {
            client,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_name_is_the_last_component() {
        assert_eq!(folder_name(r"@@abcde\Music\Björk\Fossora"), Some("Fossora"));
        assert_eq!(folder_name("music/Björk/Fossora/"), Some("Fossora"));
        assert_eq!(folder_name(""), None);
    }
}
