//! The security boundary, tested from the outside: every source at once,
//! fed fixtures whose error messages, status messages, trackers and paths
//! carry a passkey and a path — and none of it may come out the other side.

mod collect_common;

use collect_common as common;
use queuewatch::collect::Collector;
use queuewatch::collect::arr::ArrKind;
use queuewatch::snapshot::{ItemState, Origin, UNNAMED};

#[tokio::test]
async fn no_message_path_tracker_or_name_reaches_the_snapshot() {
    let radarr = common::arr(ArrKind::Radarr).await;
    let sonarr = common::arr(ArrKind::Sonarr).await;
    let lidarr = common::arr(ArrKind::Lidarr).await;
    let prowlarr = common::prowlarr().await;
    let sab = common::sabnzbd().await;
    let qbit = common::qbittorrent("sid-1").await;
    let pyload = common::pyload().await;
    let slskd = common::slskd().await;
    let seerr = common::seerr().await;
    let collector = Collector::new(
        vec![
            common::arr_source(ArrKind::Radarr, "Movies", &radarr),
            common::arr_source(ArrKind::Sonarr, "Series", &sonarr),
            common::arr_source(ArrKind::Lidarr, "Music", &lidarr),
            common::prowlarr_source(&prowlarr),
            common::sabnzbd_source(&sab),
            common::qbittorrent_source(&qbit, common::PASSWORD),
            common::pyload_source(&pyload, common::PASSWORD),
            common::slskd_source(&slskd),
            common::seerr_source(&seerr),
        ],
        7,
        72,
    );
    let s = collector.round().await;
    assert!(s.sources.iter().all(|x| x.available), "{:?}", s.sources);

    // The positive control: the fixtures DO carry the markers, so an empty
    // result below means "filtered", not "never there".
    for name in [
        "radarr/queue.json",
        "qbittorrent/torrents.json",
        "slskd/downloads.json",
    ] {
        assert!(common::fixture(name).contains("SEKRET123"), "{name}");
    }
    // The same for the names: a SABnzbd job added by URL, a magnet named
    // after its tracker, a pyLoad link not yet checked, and a Radarr entry
    // with no movie whose release title is the grab URL. outputPath, indexer
    // and downloadClient of that entry carry markers too.
    for (name, carrier) in [
        (
            "sabnzbd/queue.json",
            "\"filename\": \"Trying https://idx.example/getnzb?apikey=IDXKEY123",
        ),
        (
            "qbittorrent/torrents.json",
            "\"name\": \"https://tracker.example/announce?passkey=SEKRET123",
        ),
        (
            "pyload/status_downloads.json",
            "\"name\": \"https://SecretHoster.example/f/IDXKEY123",
        ),
        (
            "radarr/queue.json",
            "\"title\": \"https://idx.example/getnzb?apikey=IDXKEY123",
        ),
        (
            "radarr/queue.json",
            "\"outputPath\": \"/data/usenet/incomplete/IDXKEY123",
        ),
        ("radarr/queue.json", "\"indexer\": \"NZBSecret IDXKEY123"),
        (
            "radarr/queue.json",
            "\"downloadClient\": \"SABnzbd private-box",
        ),
    ] {
        assert!(common::fixture(name).contains(carrier), "{name}: {carrier}");
    }
    let json = serde_json::to_string(&s).unwrap();
    for marker in common::MARKERS {
        assert!(
            !json.contains(marker),
            "{marker} leaked into the snapshot: {json}"
        );
    }
    // Withheld, not dropped: the four downloads are still on the page, under
    // the neutral marker.
    let unnamed = s.queue.iter().filter(|q| q.title == UNNAMED).count();
    assert_eq!(unnamed, 4, "{:?}", s.queue);
    assert!(!json.contains(common::KEY));
    assert!(!json.contains(common::PASSWORD));

    // And the merge saw it all: the torrent folded into Radarr's entry, the
    // season pack into both episodes, the Usenet album into Lidarr's.
    let dune = s
        .queue
        .iter()
        .find(|q| q.title == "Dune: Part Two (2024)")
        .unwrap();
    assert_eq!(dune.origin, Origin::Manager);
    // Radarr flags the download with a warning; qBittorrent only knows it
    // is stalled. The manager's warning is the one a person can act on.
    assert_eq!(dune.state, ItemState::Warning);
    assert!((dune.progress - 0.65).abs() < 1e-6);
    let episodes: Vec<_> = s
        .queue
        .iter()
        .filter(|q| q.title.starts_with("Severance"))
        .collect();
    assert_eq!(episodes.len(), 2);
    assert!(episodes.iter().all(|e| (e.progress - 0.25).abs() < 1e-6));
    let fossora = s
        .queue
        .iter()
        .find(|q| q.title == "Björk – Fossora")
        .unwrap();
    assert_eq!(fossora.origin, Origin::Manager);
    assert!((fossora.progress - 0.5).abs() < 1e-6);
    assert!(
        !s.queue
            .iter()
            .any(|q| q.title.contains("Fossora (2022) [FLAC]"))
    );
    let iso = s
        .queue
        .iter()
        .find(|q| q.title.starts_with("debian"))
        .unwrap();
    assert_eq!(iso.origin, Origin::Client);
    assert_eq!(s.queue[0].state, ItemState::Downloading);

    // Requests learn their progress from the queue.
    let severance = s.requests.iter().find(|r| r.title == "Severance").unwrap();
    assert!((severance.progress.unwrap() - 0.25).abs() < 1e-6);
    let dune_request = s
        .requests
        .iter()
        .find(|r| r.title.starts_with("Dune"))
        .unwrap();
    assert!((dune_request.progress.unwrap() - 0.65).abs() < 1e-6);
    assert!(s.indexers.known);
}
