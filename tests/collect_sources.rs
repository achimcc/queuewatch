//! Each source against a fake of its service: what it reads, what it makes
//! of it, and that a service that says no costs only itself.

mod collect_common;

use collect_common as common;
use queuewatch::collect::Collector;
use queuewatch::collect::arr::ArrKind;
use queuewatch::snapshot::{
    ItemState, MediaKind, Origin, RequestState, Snapshot, Transport, UNNAMED,
};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn one(source: queuewatch::collect::Source) -> Snapshot {
    Collector::new(vec![source], 7, 72).round().await
}

fn titles(s: &Snapshot) -> Vec<&str> {
    s.queue.iter().map(|q| q.title.as_str()).collect()
}

#[tokio::test]
async fn radarr_builds_titles_from_the_movie_and_picks_the_release_in_the_window() {
    let server = common::arr(ArrKind::Radarr).await;
    let s = one(common::arr_source(ArrKind::Radarr, "Movies", &server)).await;
    assert!(s.sources[0].available);
    // The entry without a movie falls back to its release title, which is
    // the grab URL: withheld.
    assert_eq!(
        titles(&s),
        ["Dune: Part Two (2024)", UNNAMED, "Heat (1995)"]
    );
    let dune = &s.queue[0];
    assert_eq!(dune.kind, MediaKind::Movie);
    assert_eq!(dune.ids.tmdb, Some(693134));
    assert_eq!(dune.transport, Transport::Torrent);
    // No client answered, so the manager's own view stands: downloading
    // with a warning flag.
    assert_eq!(dune.state, ItemState::Warning);
    assert_eq!(dune.origin, Origin::Manager);
    assert!((dune.progress - 0.65).abs() < 0.01, "{}", dune.progress);
    assert!(dune.eta.is_some());
    assert_eq!(s.queue[2].state, ItemState::Importing);

    // In cinemas long ago, digital in two days, physical in five: the
    // calendar shows the digital release. The far-future film is outside.
    assert_eq!(s.calendar.len(), 1);
    let mickey = &s.calendar[0];
    assert_eq!(mickey.title, "Mickey 17 (2025)");
    let days = (mickey.airs_at.as_second() - s.taken_at.as_second()) / 3600;
    assert!((47..=49).contains(&days), "{days}");

    assert_eq!(s.recent.len(), 1);
    assert_eq!(s.recent[0].title, "Arrival (2016)");
    assert_eq!(s.recent[0].ids.tmdb, Some(329865));
}

#[tokio::test]
async fn sonarr_names_episodes_and_leaves_out_tba() {
    let server = common::arr(ArrKind::Sonarr).await;
    let s = one(common::arr_source(ArrKind::Sonarr, "Series", &server)).await;
    assert!(s.sources[0].available);
    assert_eq!(
        titles(&s),
        [
            "Severance – S02E03 – Who Is Alive?",
            "Severance – S02E04 – Woe's Hollow"
        ]
    );
    assert_eq!(s.queue[0].ids.tvdb, Some(371980));
    assert_eq!(s.queue[0].ids.tmdb, Some(95396));
    assert_eq!(s.calendar[0].title, "Andor – S01E05");
    assert_eq!(s.calendar[0].ids.tvdb, Some(393189));
    assert_eq!(
        s.recent[0].title,
        "Severance – S02E02 – Goodbye, Mrs. Selvig"
    );
}

#[tokio::test]
async fn lidarr_names_artist_and_album() {
    let server = common::arr(ArrKind::Lidarr).await;
    let s = one(common::arr_source(ArrKind::Lidarr, "Music", &server)).await;
    assert!(s.sources[0].available);
    assert_eq!(titles(&s), ["Björk – Fossora"]);
    assert_eq!(s.queue[0].kind, MediaKind::Album);
    assert_eq!(s.queue[0].state, ItemState::Downloading);
    assert_eq!(s.queue[0].transport, Transport::Usenet);
    assert_eq!(
        s.queue[0].ids.musicbrainz.as_deref(),
        Some("a7d4b6a4-6f8e-4b0e-9c1f-6f1e2a3b4c5d")
    );
    assert_eq!(s.calendar[0].title, "The Cure – Songs of a Lost World");
    assert!(!s.calendar[0].has_file);
    assert_eq!(s.recent[0].title, "Björk – Vespertine");
}

#[tokio::test]
async fn prowlarr_counts_and_never_names() {
    let server = common::prowlarr().await;
    let s = one(common::prowlarr_source(&server)).await;
    assert_eq!(s.indexers.enabled, 2);
    assert_eq!(s.indexers.healthy, 1);
    assert!(s.indexers.known);
}

#[tokio::test]
async fn sabnzbd_reports_slots_with_sizes_and_eta() {
    let server = common::sabnzbd().await;
    let s = one(common::sabnzbd_source(&server)).await;
    assert!(s.sources[0].available);
    assert_eq!(s.queue.len(), 3);
    let fossora = &s.queue[0];
    assert_eq!(fossora.title, "Björk - Fossora (2022) [FLAC]");
    assert_eq!(fossora.transport, Transport::Usenet);
    assert_eq!(fossora.origin, Origin::Client);
    assert_eq!(fossora.state, ItemState::Downloading);
    assert_eq!(fossora.size_bytes, Some(500 * 1024 * 1024));
    assert!((fossora.progress - 0.5).abs() < 1e-6);
    assert!(fossora.eta.is_some());
    // Sorted by title among the waiting: the job added by URL is withheld.
    assert_eq!(titles(&s)[1..], [UNNAMED, "Some.Manual.Download.2026"]);
    assert_eq!(s.queue[2].state, ItemState::Queued);
    assert_eq!(s.queue[2].eta, None);
}

#[tokio::test]
async fn sabnzbd_with_a_wrong_key_is_unavailable() {
    let server = common::sabnzbd().await;
    let source = queuewatch::collect::Source::Sabnzbd(
        queuewatch::collect::sabnzbd::Sabnzbd::new(
            "Usenet",
            &server.uri(),
            "wrong".into(),
            common::client(),
        )
        .unwrap(),
    );
    let s = one(source).await;
    assert!(!s.sources[0].available);
    assert!(s.queue.is_empty());
}

#[tokio::test]
async fn qbittorrent_signs_in_and_skips_what_is_seeding() {
    let server = common::qbittorrent("sid-1").await;
    let s = one(common::qbittorrent_source(&server, common::PASSWORD)).await;
    assert!(s.sources[0].available);
    assert_eq!(s.queue.len(), 4, "{:?}", titles(&s));
    assert!(
        titles(&s).contains(&UNNAMED),
        "the magnet named after its tracker"
    );
    assert!(!titles(&s).contains(&"Already.Seeding.Release"));
    let dune = s
        .queue
        .iter()
        .find(|q| q.title.starts_with("Dune"))
        .unwrap();
    assert_eq!(dune.state, ItemState::Stalled);
    assert_eq!(dune.transport, Transport::Torrent);
    // 8640000 is qBittorrent's "never".
    let iso = s
        .queue
        .iter()
        .find(|q| q.title.starts_with("debian"))
        .unwrap();
    assert_eq!(iso.eta, None);
}

#[tokio::test]
async fn qbittorrent_signs_in_again_when_the_session_expired() {
    let server = common::qbittorrent("sid-1").await;
    let collector = Collector::new(
        vec![common::qbittorrent_source(&server, common::PASSWORD)],
        7,
        72,
    );
    assert!(collector.round().await.sources[0].available);
    // The service restarts: the old session is gone, a login gives a new one.
    server.reset().await;
    common::mount_qbittorrent(&server, "sid-2").await;
    let s = collector.round().await;
    assert!(s.sources[0].available);
    assert_eq!(s.queue.len(), 4);
    let logins = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/v2/auth/login")
        .count();
    assert_eq!(logins, 1);
}

#[tokio::test]
async fn qbittorrent_with_a_wrong_password_is_unavailable() {
    let server = common::qbittorrent("sid-1").await;
    let s = one(common::qbittorrent_source(&server, "wrong")).await;
    assert!(!s.sources[0].available);
}

#[tokio::test]
async fn pyload_signs_in_through_the_form_with_its_csrf_token() {
    let server = common::pyload().await;
    let s = one(common::pyload_source(&server, common::PASSWORD)).await;
    assert!(s.sources[0].available);
    assert_eq!(
        titles(&s),
        ["Big.Archive.part1.rar", UNNAMED, "Waiting.File.part2.rar"]
    );
    assert_eq!(s.queue[0].transport, Transport::Hoster);
    assert_eq!(s.queue[0].state, ItemState::Downloading);
    assert!((s.queue[0].progress - 0.4).abs() < 1e-6);
    assert!(s.queue[0].eta.is_some());
    assert_eq!(s.queue[2].state, ItemState::Queued);
}

#[tokio::test]
async fn pyload_with_a_wrong_password_is_unavailable() {
    let server = common::pyload().await;
    let s = one(common::pyload_source(&server, "wrong")).await;
    assert!(!s.sources[0].available);
}

#[tokio::test]
async fn slskd_makes_one_item_per_folder_and_drops_finished_ones() {
    let server = common::slskd().await;
    let s = one(common::slskd_source(&server)).await;
    assert!(s.sources[0].available);
    assert_eq!(titles(&s), ["Disintegration (1989)"]);
    let album = &s.queue[0];
    assert_eq!(album.transport, Transport::Soulseek);
    assert_eq!(album.state, ItemState::Downloading);
    assert!((album.progress - 0.3).abs() < 1e-6);
    assert_eq!(album.left_bytes, Some(70_000_000));
    assert!(album.eta.is_some());
}

#[tokio::test]
async fn seerr_requests_get_titles_once_and_people_from_jellyfin() {
    let server = common::seerr().await;
    let collector = Collector::new(vec![common::seerr_source(&server)], 7, 72);
    let s = collector.round().await;
    assert!(s.sources[0].available);
    let r: Vec<&str> = s.requests.iter().map(|r| r.title.as_str()).collect();
    // Newest first; an id TMDB does not know keeps its request visible.
    assert_eq!(r, ["Severance", "Dune: Part Two (2024)", "TMDB 424242"]);
    let severance = &s.requests[0];
    assert_eq!(severance.kind, MediaKind::Episode);
    assert_eq!(severance.ids.tvdb, Some(371980));
    assert_eq!(severance.requested_by.username.as_deref(), Some("alice"));
    assert_eq!(
        severance.requested_by.email.as_deref(),
        Some("alice@example.org")
    );
    assert_eq!(severance.state, RequestState::Processing);
    assert_eq!(s.requests[1].requested_by.username.as_deref(), Some("bob"));
    assert_eq!(s.requests[2].state, RequestState::Pending);

    collector.round().await;
    let lookups = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/v1/movie/693134")
        .count();
    assert_eq!(lookups, 1, "titles are cached across rounds");
}

#[tokio::test]
async fn a_failing_source_costs_only_itself() {
    let radarr = common::arr(ArrKind::Radarr).await;
    let sab = common::sabnzbd().await;
    let wrong = queuewatch::collect::Source::Arr(
        queuewatch::collect::arr::Arr::new(
            ArrKind::Radarr,
            "4K",
            &radarr.uri(),
            "wrong".into(),
            common::client(),
        )
        .unwrap(),
    );
    let s = Collector::new(vec![wrong, common::sabnzbd_source(&sab)], 7, 72)
        .round()
        .await;
    assert_eq!(s.sources.len(), 2);
    assert_eq!(s.sources[0].label, "4K");
    assert!(!s.sources[0].available);
    assert!(s.sources[1].available);
    assert_eq!(s.queue.len(), 3);
    // Prowlarr is not configured: the counts are not known, not zero.
    assert!(!s.indexers.known);
}

#[tokio::test]
async fn a_service_slower_than_the_timeout_is_unavailable() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(3)))
        .mount(&server)
        .await;
    let client = queuewatch::collect::http::client(std::time::Duration::from_millis(300)).unwrap();
    let source = queuewatch::collect::Source::Slskd(
        queuewatch::collect::slskd::Slskd::new(
            "Soulseek",
            &server.uri(),
            common::KEY.into(),
            client,
        )
        .unwrap(),
    );
    let s = one(source).await;
    assert!(!s.sources[0].available);
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "http://192.0.2.1/login"),
        )
        .mount(&server)
        .await;
    let s = one(common::slskd_source(&server)).await;
    assert!(!s.sources[0].available);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// A SERVICE THAT ANSWERS WITH TOO MUCH costs only itself. The body is
/// valid JSON a byte past the limit, so nothing but its size can be the
/// reason it is refused.
#[tokio::test]
async fn an_answer_larger_than_the_limit_makes_the_source_unavailable() {
    let limit = queuewatch::collect::http::MAX_BODY;
    let mut body = String::with_capacity(limit + 1);
    body.push('[');
    body.push_str(&" ".repeat(limit - 1));
    body.push(']');
    assert_eq!(body.len(), limit + 1);
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let s = one(common::slskd_source(&server)).await;
    assert!(!s.sources[0].available);

    // And the same JSON at the limit is an empty, available source.
    server.reset().await;
    let mut body = String::from("[");
    body.push_str(&" ".repeat(limit - 2));
    body.push(']');
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let s = one(common::slskd_source(&server)).await;
    assert!(s.sources[0].available);
}

fn radarr_record(n: u64) -> serde_json::Value {
    serde_json::json!({
        "movie": { "title": format!("Film {n}"), "year": 2000, "tmdbId": n },
        "size": 100, "sizeleft": 50, "status": "downloading",
        "trackedDownloadStatus": "ok", "trackedDownloadState": "downloading",
        "downloadId": format!("id{n}"), "protocol": "usenet"
    })
}

/// A Radarr whose queue is `pages` long, `per_page` entries each, and that
/// claims `total` entries in all.
async fn paged_radarr(per_page: u64, pages: u64, total: u64) -> MockServer {
    use wiremock::matchers::{method, path, query_param};
    let server = MockServer::start().await;
    for page in 1..=pages {
        let records: Vec<_> = (0..per_page)
            .map(|i| radarr_record((page - 1) * per_page + i + 1))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v3/queue"))
            .and(query_param("page", page.to_string()))
            .and(query_param("pageSize", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "page": page, "pageSize": 1000, "totalRecords": total, "records": records
            })))
            .mount(&server)
            .await;
    }
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .with_priority(10)
        .mount(&server)
        .await;
    server
}

fn queue_requests(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.url.path() == "/api/v3/queue")
        .count()
}

#[tokio::test]
async fn a_queue_longer_than_one_page_is_read_to_the_end() {
    let server = paged_radarr(2, 2, 4).await;
    let s = one(common::arr_source(ArrKind::Radarr, "Movies", &server)).await;
    assert!(s.sources[0].available);
    assert_eq!(s.queue.len(), 4, "{:?}", titles(&s));
    assert!(titles(&s).contains(&"Film 4 (2000)"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(queue_requests(&requests), 2, "and not a page more");
}

/// A COUNT THAT NEVER ADDS UP does not loop: ten pages, then stop.
#[tokio::test]
async fn following_the_queue_stops_after_ten_pages() {
    let server = paged_radarr(1, 12, 1_000_000).await;
    let s = one(common::arr_source(ArrKind::Radarr, "Movies", &server)).await;
    assert!(s.sources[0].available);
    assert_eq!(s.queue.len(), 10);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(queue_requests(&requests), 10);
}

/// A Seerr with `full` full pages of 100 requests and then a short one of
/// `rest`. Every request is for the same film, so one title lookup serves
/// them all.
async fn paged_seerr(full: u64, rest: u64) -> MockServer {
    use wiremock::matchers::{method, path, query_param};
    let server = MockServer::start().await;
    let request = |n: u64| {
        serde_json::json!({
            "id": n, "status": 2, "type": "movie",
            "createdAt": format!("2026-10-01T00:00:{:02}.000Z", n % 60),
            "media": { "tmdbId": 693134, "status": 3 },
            "requestedBy": { "username": format!("user{n}") }
        })
    };
    for page in 0..=full {
        let count = if page < full { 100 } else { rest };
        let results: Vec<_> = (0..count).map(|i| request(page * 100 + i)).collect();
        Mock::given(method("GET"))
            .and(path("/api/v1/request"))
            .and(query_param("take", "100"))
            .and(query_param("skip", (page * 100).to_string()))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": results })),
            )
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/api/v1/movie/693134"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "title": "Dune: Part Two", "releaseDate": "2024-02-27"
        })))
        .mount(&server)
        .await;
    // Past the last page Seerr answers with an empty one.
    Mock::given(method("GET"))
        .and(path("/api/v1/request"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": [] })),
        )
        .with_priority(10)
        .mount(&server)
        .await;
    server
}

fn request_pages(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.url.path() == "/api/v1/request")
        .count()
}

#[tokio::test]
async fn seerr_requests_are_read_past_the_first_hundred() {
    let server = paged_seerr(2, 5).await;
    let s = one(common::seerr_source(&server)).await;
    assert!(s.sources[0].available);
    assert_eq!(s.requests.len(), 205);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(request_pages(&requests), 3, "the short page ends it");
}

#[tokio::test]
async fn seerr_paging_stops_after_ten_pages() {
    let server = paged_seerr(12, 0).await;
    let s = one(common::seerr_source(&server)).await;
    assert!(s.sources[0].available);
    assert_eq!(s.requests.len(), 1000);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(request_pages(&requests), 10);
}

#[tokio::test]
async fn bindery_names_author_and_book_and_tells_books_from_audiobooks() {
    let server = common::bindery().await;
    let s = one(common::bindery_source(&server)).await;
    assert!(s.sources[0].available);
    // The imported download is left to "recently added"; downloading first,
    // soonest done first, then the rest by title.
    assert_eq!(
        titles(&s),
        [
            "Stephen Fry – Mythos",
            "Andy Weir – Project Hail Mary",
            UNNAMED,
            "Ursula K. Le Guin – The Dispossessed",
        ]
    );
    let mythos = &s.queue[0];
    assert_eq!(mythos.kind, MediaKind::Audiobook);
    assert_eq!(mythos.transport, Transport::Usenet);
    assert!((mythos.progress - 0.1).abs() < 1e-6);
    let mary = &s.queue[1];
    assert_eq!(mary.kind, MediaKind::Book);
    assert_eq!(mary.transport, Transport::Torrent);
    assert_eq!(mary.state, ItemState::Downloading);
    assert_eq!(mary.origin, Origin::Manager);
    assert!((mary.progress - 0.42).abs() < 1e-6);
    assert_eq!(mary.size_bytes, Some(2_000_000));
    assert_eq!(mary.left_bytes, Some(1_160_000));
    let mins = (mary.eta.unwrap().as_second() - s.taken_at.as_second()) / 60;
    assert!((64..=65).contains(&mins), "{mins}");
    // A free-text grab without a book, named by its grab URL: withheld, and
    // its import is blocked.
    assert_eq!(s.queue[2].state, ItemState::Warning);
    assert_eq!(s.queue[3].state, ItemState::Failed);
    assert_eq!(s.queue[3].eta, None);

    // Imports only, inside the window, newest first; the one without a book
    // keeps its release name.
    let recent: Vec<_> = s
        .recent
        .iter()
        .map(|r| (r.title.as_str(), r.kind))
        .collect();
    assert_eq!(
        recent,
        [
            (
                "Becky Chambers – A Psalm for the Wild-Built",
                MediaKind::Book
            ),
            ("Terry Pratchett - Small Gods [MP3]", MediaKind::Audiobook),
        ]
    );
}

#[tokio::test]
async fn bindery_merges_with_sabnzbd_over_the_nzo_id() {
    let bindery = common::bindery().await;
    let sab = common::sabnzbd().await;
    let s = Collector::new(
        vec![
            common::bindery_source(&bindery),
            common::sabnzbd_source(&sab),
        ],
        7,
        72,
    )
    .round()
    .await;
    assert!(s.sources.iter().all(|x| x.available));
    // Four from bindery, three from SABnzbd, one of them the same download.
    assert_eq!(s.queue.len(), 6, "{:?}", titles(&s));
    assert!(!titles(&s).contains(&"Some.Manual.Download.2026"));
    let mythos = s
        .queue
        .iter()
        .find(|q| q.title == "Stephen Fry – Mythos")
        .unwrap();
    assert_eq!(mythos.origin, Origin::Manager);
    assert_eq!(mythos.kind, MediaKind::Audiobook);
    // The numbers are SABnzbd's: queued, nothing loaded yet.
    assert_eq!(mythos.state, ItemState::Queued);
    assert_eq!(mythos.progress, 0.0);
    assert_eq!(mythos.size_bytes, Some(1000 * 1024 * 1024));
}

#[tokio::test]
async fn bindery_with_a_wrong_key_is_unavailable() {
    let server = common::bindery().await;
    let wrong = queuewatch::collect::Source::Bindery(
        queuewatch::collect::bindery::Bindery::new(
            "Books",
            &server.uri(),
            "wrong".into(),
            common::client(),
        )
        .unwrap(),
    );
    let s = one(wrong).await;
    assert!(!s.sources[0].available);
    assert!(s.queue.is_empty());
}

fn bindery_import(n: u64, created_at: jiff::Timestamp) -> serde_json::Value {
    serde_json::json!({
        "id": n, "bookId": n, "eventType": "bookImported",
        "sourceTitle": format!("Book.{n}.EPUB"), "data": "{}",
        "createdAt": created_at.to_string(),
        "book": { "id": n, "title": format!("Book {n}"), "authorId": 1, "authorName": "A" }
    })
}

/// A bindery whose import history answers every `offset` with a full page
/// of 500, all `age` old, claiming `total` events in all.
async fn paged_bindery(age: jiff::SignedDuration, total: u64) -> MockServer {
    use wiremock::matchers::{method, path, query_param};
    let server = MockServer::start().await;
    let at = jiff::Timestamp::now() - age;
    for page in 0..12u64 {
        let items: Vec<_> = (0..500)
            .map(|i| bindery_import(page * 500 + i, at))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v1/history"))
            .and(query_param("limit", "500"))
            .and(query_param("offset", (page * 500).to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": items, "total": total, "limit": 500, "offset": page * 500
            })))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/api/v1/queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": [] })))
        .mount(&server)
        .await;
    server
}

fn history_pages(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.url.path() == "/api/v1/history")
        .count()
}

#[tokio::test]
async fn bindery_history_is_read_to_its_total() {
    let server = paged_bindery(jiff::SignedDuration::from_hours(1), 1000).await;
    let s = one(common::bindery_source(&server)).await;
    assert!(s.sources[0].available);
    assert_eq!(s.recent.len(), 1000);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(history_pages(&requests), 2, "and not a page more");
}

/// The history has no date filter: a page that reaches past the window is
/// the last one asked for, however many events bindery has.
#[tokio::test]
async fn bindery_history_stops_at_the_end_of_the_window() {
    let server = paged_bindery(jiff::SignedDuration::from_hours(100), 1_000_000).await;
    let s = one(common::bindery_source(&server)).await;
    assert!(s.sources[0].available);
    assert!(s.recent.is_empty());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(history_pages(&requests), 1);
}

/// A COUNT THAT NEVER ADDS UP does not loop: ten pages, then stop.
#[tokio::test]
async fn bindery_history_stops_after_ten_pages() {
    let server = paged_bindery(jiff::SignedDuration::from_hours(1), 1_000_000).await;
    let s = one(common::bindery_source(&server)).await;
    assert!(s.sources[0].available);
    assert_eq!(s.recent.len(), 5000);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(history_pages(&requests), 10);
}
