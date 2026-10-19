//! The one filter every service-provided name passes before it becomes a
//! title.
//!
//! Most titles are built from a manager's metadata ("Heat (1995)"), and
//! those say nothing about the setup. But a download no manager claims has
//! only the name its client gives it, and the client does not promise that
//! this is a release name: SABnzbd shows a job added by URL as
//! `Trying https://indexer/getnzb?apikey=…` until it has fetched the NZB,
//! pyLoad shows the link until it has checked it, and qBittorrent names a
//! magnet without metadata after whatever the magnet carried. Each of those
//! is an indexer key or a tracker passkey on the page.
//!
//! So the rule is a deny-list on shape, not an allow-list on content: a name
//! that looks like a URL, a query, a key or an absolute path is not shown at
//! all, and becomes [`UNNAMED`]. Losing a harmless name that happens to
//! contain a `?` costs a line of text; letting one URL through costs a key.
//!
//! It is ONE function, applied at every place a client or release name is
//! read (SABnzbd, qBittorrent, pyLoad, slskd, and the managers' release-name
//! fallback), so that a new source has one obvious thing to call.

use crate::snapshot::UNNAMED;

/// Longer than any real release name; a cap so that a client cannot fill the
/// page (and the snapshot) with one entry.
const MAX_CHARS: usize = 200;

/// Substrings that mark a name as something other than a name. Compared in
/// lower case: `ApiKey=` and `PASSKEY` are as common as their lower-case
/// spellings.
const SUSPICIOUS: &[&str] = &[
    "://", "?", "apikey", "api_key", "passkey", "token=", "magnet:",
];

/// The title for a name a client or a release provided.
///
/// `None` for no name at all (empty or blank) — the caller skips such an
/// entry, as before. [`UNNAMED`] for a name that looks like it carries a URL,
/// a key or a path. Otherwise the name, trimmed, at most 200 characters.
pub fn client_title(raw: Option<&str>) -> Option<String> {
    let name = raw.map(str::trim).filter(|n| !n.is_empty())?;
    let lower = name.to_lowercase();
    // A separator ANYWHERE, not only in front: `C:\Downloads\…` and
    // `incoming/release` are paths as much as `/data/…`. A release name with
    // a slash in it (AC/DC) becomes the marker too — rare, and the cheaper
    // mistake.
    let suspicious =
        SUSPICIOUS.iter().any(|s| lower.contains(s)) || name.contains('/') || name.contains('\\');
    if suspicious {
        return Some(UNNAMED.to_string());
    }
    Some(name.chars().take(MAX_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_name_passes_unchanged_apart_from_whitespace() {
        assert_eq!(
            client_title(Some("  Dune.Part.Two.2024.2160p-GRP ")).as_deref(),
            Some("Dune.Part.Two.2024.2160p-GRP")
        );
        assert_eq!(
            client_title(Some("Björk - Fossora (2022) [FLAC]")).as_deref(),
            Some("Björk - Fossora (2022) [FLAC]")
        );
    }

    #[test]
    fn no_name_is_no_title() {
        assert_eq!(client_title(None), None);
        assert_eq!(client_title(Some("   ")), None);
    }

    /// EVERY ONE OF THESE IS A KEY OR A PATH ON THE PAGE without the filter.
    #[test]
    fn urls_keys_and_paths_become_the_neutral_marker() {
        for raw in [
            "Trying https://idx.example/getnzb?apikey=K",
            "https://tracker.example/announce?passkey=P",
            "magnet:?xt=urn:btih:abc&tr=udp%3A%2F%2Ft",
            "getnzb?id=1",
            "release ApiKey=K",
            "a PASSKEY b",
            "x api_key y",
            "session token=abc",
            "/data/torrents/x/release",
            r"\\nas\share\release",
            "ftp://mirror/file.iso",
            r"C:\Downloads\release",
            "D:/incoming/release",
            "incoming/Heat.1995.1080p",
        ] {
            assert_eq!(client_title(Some(raw)).as_deref(), Some(UNNAMED), "{raw}");
        }
    }

    #[test]
    fn a_long_name_is_cut_at_200_characters_not_bytes() {
        let long = "ä".repeat(300);
        let title = client_title(Some(&long)).unwrap();
        assert_eq!(title.chars().count(), 200);
    }
}
