//! Turning what each source reported into one snapshot.
//!
//! A download shows up twice: Radarr knows it is "Dune: Part Two (2024)",
//! qBittorrent knows it is at 63 % with two hours left. Neither alone is what
//! a person wants to read. They meet over the download id — the client's
//! torrent hash or nzo id, which the manager stores — and become one item.

use std::cmp::Ordering;
use std::collections::HashMap;

use jiff::Timestamp;

use crate::snapshot::{
    CalendarItem, IndexerSummary, ItemState, MediaKind, Origin, QueueItem, RecentItem, Request,
    SCHEMA_VERSION, Snapshot, SourceKind, SourceStatus, Transport,
};

/// A queue item with the id that ties it to the other side. The id never
/// reaches the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Tracked {
    pub item: QueueItem,
    pub download_id: Option<String>,
}

/// What one source contributed to a round.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Contribution {
    /// Queue entries of a manager (Radarr, Sonarr, Lidarr, bindery).
    pub managed: Vec<Tracked>,
    /// Entries of a download client (SABnzbd, qBittorrent, pyLoad, slskd).
    pub client: Vec<Tracked>,
    pub calendar: Vec<CalendarItem>,
    pub recent: Vec<RecentItem>,
    pub requests: Vec<Request>,
    pub indexers: Option<IndexerSummary>,
}

/// Builds the snapshot. `sources` is in configuration order and says who
/// answered; `parts` holds what those who answered contributed.
pub fn assemble(
    taken_at: Timestamp,
    sources: Vec<SourceStatus>,
    parts: Vec<Contribution>,
) -> Snapshot {
    let mut managed = Vec::new();
    let mut client = Vec::new();
    let mut calendar = Vec::new();
    let mut recent = Vec::new();
    let mut requests = Vec::new();
    let mut indexers: Option<IndexerSummary> = None;
    for part in parts {
        managed.extend(part.managed);
        client.extend(part.client);
        calendar.extend(part.calendar);
        recent.extend(part.recent);
        requests.extend(part.requests);
        if let Some(summary) = part.indexers {
            let total = indexers.get_or_insert_with(IndexerSummary::default);
            total.enabled += summary.enabled;
            total.healthy += summary.healthy;
            total.known = true;
        }
    }
    // One indexer manager that did not answer makes the sum a lie — it
    // would read as "fewer indexers", not as "we do not know".
    let prowlarr_down = sources
        .iter()
        .any(|s| s.kind == SourceKind::Prowlarr && !s.available);
    let indexers = match indexers {
        Some(summary) if !prowlarr_down => summary,
        _ => IndexerSummary::default(),
    };

    let mut queue = merge_queue(managed, client);
    sort_queue(&mut queue);
    join_requests(&mut requests, &queue);
    requests.sort_by_key(|r| std::cmp::Reverse(r.requested_at));
    calendar.sort_by(|a, b| {
        a.airs_at
            .cmp(&b.airs_at)
            .then_with(|| a.title.cmp(&b.title))
    });
    recent.sort_by_key(|r| std::cmp::Reverse(r.imported_at));

    Snapshot {
        version: SCHEMA_VERSION,
        taken_at,
        sources,
        queue,
        indexers,
        calendar,
        recent,
        requests,
    }
}

/// Folds client entries into the manager entries that claim them. A season
/// pack is one torrent and several Sonarr entries: every one of them gets the
/// client's numbers, and the torrent does not show up a second time.
pub fn merge_queue(managed: Vec<Tracked>, client: Vec<Tracked>) -> Vec<QueueItem> {
    // Sonarr and Radarr store a qBittorrent hash uppercased, qBittorrent
    // reports it lowercased.
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for (i, c) in client.iter().enumerate() {
        if let Some(id) = &c.download_id {
            by_id.entry(id.to_lowercase()).or_insert(i);
        }
    }
    let mut claimed = vec![false; client.len()];
    let mut queue = Vec::with_capacity(managed.len() + client.len());
    for m in managed {
        let mut item = m.item;
        item.origin = Origin::Manager;
        let found = m
            .download_id
            .as_ref()
            .and_then(|id| by_id.get(&id.to_lowercase()).copied());
        if let Some(i) = found {
            claimed[i] = true;
            fold_client(&mut item, &client[i].item);
        }
        queue.push(item);
    }
    for (i, c) in client.into_iter().enumerate() {
        if !claimed[i] {
            let mut item = c.item;
            item.origin = Origin::Client;
            queue.push(item);
        }
    }
    queue
}

/// The client measures the download, the manager knows what it is. So the
/// title and ids stay, the numbers come from the client — and so does the
/// state, unless the manager is already past the download (importing, failed,
/// done), which the client cannot know, or has flagged it for a human
/// (warning). A client that is still happily downloading knows nothing of a
/// wrong match or a release the manager will refuse to import; letting its
/// `Downloading` win would hide exactly the line someone needs to act on.
fn fold_client(item: &mut QueueItem, client: &QueueItem) {
    item.progress = client.progress;
    item.size_bytes = client.size_bytes.or(item.size_bytes);
    item.left_bytes = client.left_bytes.or(item.left_bytes);
    item.eta = client.eta.or(item.eta);
    if !matches!(
        item.state,
        ItemState::Importing | ItemState::Warning | ItemState::Failed | ItemState::Completed
    ) {
        item.state = client.state;
    }
    if item.transport == Transport::Unknown {
        item.transport = client.transport;
    }
}

/// Downloading first, then by when it will be done; what has no eta last.
pub fn sort_queue(queue: &mut [QueueItem]) {
    queue.sort_by(|a, b| {
        let rank = |i: &QueueItem| u8::from(i.state != ItemState::Downloading);
        rank(a)
            .cmp(&rank(b))
            .then_with(|| match (a.eta, b.eta) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            .then_with(|| a.title.cmp(&b.title))
    });
}

/// A request is "downloading" when queue items for the same title are. Movies
/// meet over TMDB, series over TVDB — the ids Seerr and the managers share.
/// A series request without a TVDB id (Seerr only learns it from TMDB's
/// external ids, and not every show has one there) falls back to TMDB, which
/// Sonarr also reports: otherwise such a request would never show progress.
pub fn join_requests(requests: &mut [Request], queue: &[QueueItem]) {
    for request in requests.iter_mut() {
        let matching: Vec<&QueueItem> = queue
            .iter()
            .filter(|q| match request.kind {
                MediaKind::Movie => {
                    q.kind == MediaKind::Movie
                        && request.ids.tmdb.is_some()
                        && q.ids.tmdb == request.ids.tmdb
                }
                MediaKind::Episode => {
                    q.kind == MediaKind::Episode
                        && match (request.ids.tvdb, request.ids.tmdb) {
                            (Some(tvdb), _) => q.ids.tvdb == Some(tvdb),
                            (None, Some(tmdb)) => q.ids.tmdb == Some(tmdb),
                            (None, None) => false,
                        }
                }
                _ => false,
            })
            .collect();
        if matching.is_empty() {
            request.progress = None;
            request.eta = None;
            continue;
        }
        let sum: f32 = matching.iter().map(|q| q.progress).sum();
        request.progress = Some(sum / matching.len() as f32);
        request.eta = matching.iter().filter_map(|q| q.eta).max();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{MediaIds, RequestState, Requester};

    fn item(kind: MediaKind, title: &str, state: ItemState, progress: f32) -> QueueItem {
        QueueItem {
            kind,
            title: title.into(),
            ids: MediaIds::default(),
            transport: Transport::Unknown,
            state,
            origin: Origin::Client,
            progress,
            size_bytes: None,
            left_bytes: None,
            eta: None,
        }
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn a_client_item_merges_into_its_manager_item_case_insensitively() {
        let mut manager = item(
            MediaKind::Movie,
            "Dune: Part Two (2024)",
            ItemState::Downloading,
            0.1,
        );
        manager.ids.tmdb = Some(693134);
        manager.transport = Transport::Torrent;
        let mut torrent = item(
            MediaKind::Other,
            "Dune.Part.Two.2024.2160p",
            ItemState::Stalled,
            0.63,
        );
        torrent.transport = Transport::Torrent;
        torrent.eta = Some(ts("2026-10-19T12:00:00Z"));
        torrent.size_bytes = Some(100);
        torrent.left_bytes = Some(37);
        let lone = item(MediaKind::Other, "manual.iso", ItemState::Downloading, 0.5);
        let queue = merge_queue(
            vec![Tracked {
                item: manager,
                download_id: Some("ABCDEF0123".into()),
            }],
            vec![
                Tracked {
                    item: torrent,
                    download_id: Some("abcdef0123".into()),
                },
                Tracked {
                    item: lone,
                    download_id: Some("ffff".into()),
                },
            ],
        );
        assert_eq!(queue.len(), 2);
        let dune = &queue[0];
        assert_eq!(dune.title, "Dune: Part Two (2024)");
        assert_eq!(dune.origin, Origin::Manager);
        assert_eq!(dune.kind, MediaKind::Movie);
        assert_eq!(dune.ids.tmdb, Some(693134));
        assert_eq!(dune.progress, 0.63);
        assert_eq!(dune.state, ItemState::Stalled);
        assert_eq!(dune.left_bytes, Some(37));
        assert_eq!(dune.eta, Some(ts("2026-10-19T12:00:00Z")));
        assert_eq!(queue[1].title, "manual.iso");
        assert_eq!(queue[1].origin, Origin::Client);
    }

    #[test]
    fn a_manager_that_is_already_importing_keeps_its_state() {
        let manager = item(MediaKind::Movie, "Heat (1995)", ItemState::Importing, 1.0);
        let client = item(MediaKind::Other, "Heat", ItemState::Downloading, 1.0);
        let queue = merge_queue(
            vec![Tracked {
                item: manager,
                download_id: Some("x".into()),
            }],
            vec![Tracked {
                item: client,
                download_id: Some("X".into()),
            }],
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].state, ItemState::Importing);
    }

    /// A WARNING IS THE MANAGER'S TO GIVE: the client cannot see a wrong
    /// match, and its `Downloading` must not paper over it.
    #[test]
    fn a_manager_warning_survives_a_client_that_is_downloading() {
        let manager = item(MediaKind::Movie, "Heat (1995)", ItemState::Warning, 0.0);
        let client = item(MediaKind::Other, "Heat", ItemState::Downloading, 0.3);
        let queue = merge_queue(
            vec![Tracked {
                item: manager,
                download_id: Some("h".into()),
            }],
            vec![Tracked {
                item: client,
                download_id: Some("h".into()),
            }],
        );
        assert_eq!(queue[0].state, ItemState::Warning);
        assert_eq!(
            queue[0].progress, 0.3,
            "the numbers still come from the client"
        );
    }

    #[test]
    fn a_series_request_without_tvdb_meets_its_episodes_over_tmdb() {
        let mut e = item(MediaKind::Episode, "e", ItemState::Downloading, 0.5);
        e.ids.tvdb = Some(371980);
        e.ids.tmdb = Some(95396);
        // A movie with the same TMDB number must not count: kinds first.
        let mut m = item(MediaKind::Movie, "m", ItemState::Downloading, 1.0);
        m.ids.tmdb = Some(95396);
        let queue = vec![e, m];
        let mut requests = vec![
            request(
                MediaKind::Episode,
                MediaIds {
                    tmdb: Some(95396),
                    ..Default::default()
                },
            ),
            // With a TVDB id that does not match, TMDB is NOT tried: the
            // request names its series, and that one is not downloading.
            request(
                MediaKind::Episode,
                MediaIds {
                    tvdb: Some(1),
                    tmdb: Some(95396),
                    ..Default::default()
                },
            ),
            request(MediaKind::Episode, MediaIds::default()),
        ];
        join_requests(&mut requests, &queue);
        assert_eq!(requests[0].progress, Some(0.5));
        assert_eq!(requests[1].progress, None);
        assert_eq!(requests[2].progress, None);
    }

    #[test]
    fn a_season_pack_claims_its_torrent_once_for_every_episode() {
        let e1 = item(
            MediaKind::Episode,
            "S – S01E01",
            ItemState::Downloading,
            0.0,
        );
        let e2 = item(
            MediaKind::Episode,
            "S – S01E02",
            ItemState::Downloading,
            0.0,
        );
        let pack = item(MediaKind::Other, "S.S01", ItemState::Downloading, 0.4);
        let queue = merge_queue(
            vec![
                Tracked {
                    item: e1,
                    download_id: Some("h".into()),
                },
                Tracked {
                    item: e2,
                    download_id: Some("h".into()),
                },
            ],
            vec![Tracked {
                item: pack,
                download_id: Some("h".into()),
            }],
        );
        assert_eq!(queue.len(), 2);
        assert!(
            queue
                .iter()
                .all(|q| q.progress == 0.4 && q.origin == Origin::Manager)
        );
    }

    #[test]
    fn downloading_comes_first_then_eta() {
        let mut queued = item(MediaKind::Other, "a", ItemState::Queued, 0.0);
        queued.eta = Some(ts("2026-10-19T09:00:00Z"));
        let mut late = item(MediaKind::Other, "b", ItemState::Downloading, 0.0);
        late.eta = Some(ts("2026-10-19T11:00:00Z"));
        let mut soon = item(MediaKind::Other, "c", ItemState::Downloading, 0.0);
        soon.eta = Some(ts("2026-10-19T10:00:00Z"));
        let unknown = item(MediaKind::Other, "d", ItemState::Downloading, 0.0);
        let mut queue = vec![queued, unknown, late, soon];
        sort_queue(&mut queue);
        let order: Vec<&str> = queue.iter().map(|q| q.title.as_str()).collect();
        assert_eq!(order, ["c", "b", "d", "a"]);
    }

    fn request(kind: MediaKind, ids: MediaIds) -> Request {
        Request {
            kind,
            title: "t".into(),
            ids,
            requested_by: Requester::default(),
            requested_at: ts("2026-10-01T00:00:00Z"),
            state: RequestState::Processing,
            progress: None,
            eta: None,
        }
    }

    #[test]
    fn requests_get_the_average_progress_and_the_latest_eta() {
        let mut e1 = item(MediaKind::Episode, "e1", ItemState::Downloading, 0.2);
        e1.ids.tvdb = Some(371980);
        e1.eta = Some(ts("2026-10-19T10:00:00Z"));
        let mut e2 = item(MediaKind::Episode, "e2", ItemState::Downloading, 0.6);
        e2.ids.tvdb = Some(371980);
        e2.eta = Some(ts("2026-10-19T12:00:00Z"));
        // Same number, but a movie id: must not count for the series.
        let mut m = item(MediaKind::Movie, "m", ItemState::Downloading, 1.0);
        m.ids.tmdb = Some(371980);
        let queue = vec![e1, e2, m];
        let mut requests = vec![
            request(
                MediaKind::Episode,
                MediaIds {
                    tvdb: Some(371980),
                    tmdb: Some(95396),
                    ..Default::default()
                },
            ),
            request(
                MediaKind::Movie,
                MediaIds {
                    tmdb: Some(1),
                    ..Default::default()
                },
            ),
        ];
        join_requests(&mut requests, &queue);
        assert!((requests[0].progress.unwrap() - 0.4).abs() < 1e-6);
        assert_eq!(requests[0].eta, Some(ts("2026-10-19T12:00:00Z")));
        assert_eq!(requests[1].progress, None);
        assert_eq!(requests[1].eta, None);
    }

    #[test]
    fn a_silent_prowlarr_makes_the_indexer_counts_unknown() {
        let sources = vec![SourceStatus {
            kind: SourceKind::Prowlarr,
            label: "Idx".into(),
            available: false,
        }];
        let s = assemble(ts("2026-10-19T00:00:00Z"), sources, vec![]);
        assert!(!s.indexers.known);
        let sources = vec![SourceStatus {
            kind: SourceKind::Prowlarr,
            label: "Idx".into(),
            available: true,
        }];
        let part = Contribution {
            indexers: Some(IndexerSummary {
                enabled: 5,
                healthy: 4,
                known: true,
            }),
            ..Default::default()
        };
        let s = assemble(ts("2026-10-19T00:00:00Z"), sources, vec![part]);
        assert_eq!(
            s.indexers,
            IndexerSummary {
                enabled: 5,
                healthy: 4,
                known: true
            }
        );
        assert_eq!(s.version, SCHEMA_VERSION);
    }
}
