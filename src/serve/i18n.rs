//! Every word on the page, in English and German.
//!
//! The readers are friends, not operators: the German texts use the
//! familiar "du", and nothing here names a service, a tracker or a host.
//! Durations and dates are written out by hand rather than through a locale
//! library — two languages do not justify one, and the abbreviations are
//! the ones people actually read on a phone.

use jiff::Zoned;

use crate::snapshot::{ItemState, MediaKind, RequestState, SourceKind, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    De,
}

impl Lang {
    /// The configuration has already refused anything but `en` and `de`
    /// (`Config::load`); anything else here falls back to English rather than
    /// failing a page.
    pub fn from_code(code: &str) -> Self {
        match code {
            "de" => Lang::De,
            _ => Lang::En,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::De => "de",
        }
    }
}

/// The fixed texts of one language. Formatted texts are methods below.
pub struct Strings {
    pub lang: Lang,
    pub my_requests: &'static str,
    pub my_requests_empty: &'static str,
    pub downloading: &'static str,
    pub downloading_empty: &'static str,
    pub coming_up: &'static str,
    pub coming_up_empty: &'static str,
    pub recent: &'static str,
    pub recent_empty: &'static str,
    pub services: &'static str,
    pub signed_in_as: &'static str,
    pub logout: &'static str,
    pub unreachable: &'static str,
    pub no_data_yet: &'static str,
    pub reachable: &'static str,
    pub not_reachable: &'static str,
    pub already_there: &'static str,
    pub eta_soon: &'static str,
    pub just_now: &'static str,
    pub today: &'static str,
    pub tomorrow: &'static str,
    pub yesterday: &'static str,
    pub search_unknown: &'static str,
    pub search_none: &'static str,
    pub forbidden_title: &'static str,
    pub forbidden_text: &'static str,
    pub login_failed_title: &'static str,
    pub login_failed_text: &'static str,
    pub login_unavailable_text: &'static str,
    pub try_again: &'static str,
    pub not_found: &'static str,
    pub progress_label: &'static str,
    pub sources_label: &'static str,
    /// Stands in for [`crate::snapshot::UNNAMED`]: a download whose name the
    /// collector would not pass on.
    pub unnamed_download: &'static str,
    /// The one line under the wordmark.
    pub tagline: &'static str,
    /// The argument of the `echo` in the foot.
    pub footer_echo: &'static str,
    /// The link to the source code (AGPL: whoever uses the page over the
    /// network gets to read what it runs).
    pub source_link: &'static str,
}

pub const EN: Strings = Strings {
    lang: Lang::En,
    my_requests: "My requests",
    my_requests_empty: "no open requests",
    downloading: "Downloading now",
    downloading_empty: "nothing downloading",
    coming_up: "Coming up",
    coming_up_empty: "nothing scheduled",
    recent: "Recently added",
    recent_empty: "nothing new lately",
    services: "Search & services",
    signed_in_as: "Signed in as",
    logout: "Sign out",
    unreachable: "collector unreachable",
    no_data_yet: "no data yet",
    reachable: "reachable",
    not_reachable: "not reachable",
    already_there: "already there",
    eta_soon: "any moment",
    just_now: "just now",
    today: "today",
    tomorrow: "tomorrow",
    yesterday: "yesterday",
    search_unknown: "unknown",
    search_none: "no sources set up",
    forbidden_title: "Not for this account",
    forbidden_text: "You are signed in, but this page is not open for your account. \
                     Ask whoever runs it to let you in.",
    login_failed_title: "Sign-in did not work",
    login_failed_text: "The sign-in could not be completed. Please try again.",
    login_unavailable_text: "The sign-in service cannot be reached right now. \
                             Please try again in a minute.",
    try_again: "Try again",
    not_found: "There is nothing here.",
    progress_label: "Progress",
    sources_label: "Services",
    unnamed_download: "(unnamed download)",
    tagline: "What your server is fetching, right now.",
    footer_echo: "read-only. no keys on this box.",
    source_link: "source",
};

pub const DE: Strings = Strings {
    lang: Lang::De,
    my_requests: "Meine Wünsche",
    my_requests_empty: "keine offenen Wünsche",
    downloading: "Lädt gerade",
    downloading_empty: "gerade lädt nichts",
    coming_up: "Demnächst",
    coming_up_empty: "nichts geplant",
    recent: "Neu da",
    recent_empty: "nichts Neues",
    services: "Suche & Dienste",
    signed_in_as: "Angemeldet als",
    logout: "Abmelden",
    unreachable: "Sammeldienst nicht erreichbar",
    no_data_yet: "noch keine Daten",
    reachable: "erreichbar",
    not_reachable: "nicht erreichbar",
    already_there: "schon da",
    eta_soon: "gleich",
    just_now: "gerade eben",
    today: "heute",
    tomorrow: "morgen",
    yesterday: "gestern",
    search_unknown: "unbekannt",
    search_none: "keine Quellen eingerichtet",
    forbidden_title: "Nicht für dieses Konto",
    forbidden_text: "Du bist angemeldet, aber diese Seite ist für dein Konto nicht freigegeben. \
                     Frag am besten die Person, die sie betreibt.",
    login_failed_title: "Anmeldung hat nicht geklappt",
    login_failed_text: "Die Anmeldung konnte nicht abgeschlossen werden. Probier es bitte noch einmal.",
    login_unavailable_text: "Der Anmeldedienst ist gerade nicht erreichbar. \
                             Probier es bitte in einer Minute noch einmal.",
    try_again: "Noch einmal versuchen",
    not_found: "Hier gibt es nichts.",
    progress_label: "Fortschritt",
    sources_label: "Dienste",
    unnamed_download: "(Download ohne Namen)",
    tagline: "Was der Server gerade holt — live.",
    footer_echo: "nur lesend. hier liegt kein schlüssel.",
    source_link: "Quelltext",
};

impl Strings {
    pub fn for_lang(lang: Lang) -> &'static Strings {
        match lang {
            Lang::En => &EN,
            Lang::De => &DE,
        }
    }

    pub fn updated(&self, secs_ago: i64) -> String {
        match self.lang {
            Lang::En => format!("updated {}", self.ago(secs_ago)),
            Lang::De => format!("aktualisiert {}", self.ago(secs_ago)),
        }
    }

    pub fn last_good(&self, secs_ago: i64) -> String {
        match self.lang {
            Lang::En => format!("showing data from {}", self.ago(secs_ago)),
            Lang::De => format!("du siehst den Stand von {}", self.ago(secs_ago)),
        }
    }

    pub fn requested(&self, when: &str) -> String {
        match self.lang {
            Lang::En => format!("requested {when}"),
            Lang::De => format!("gewünscht {when}"),
        }
    }

    /// The answer to `search --health`: "7/8 sources up".
    pub fn sources_up(&self, healthy: u32, enabled: u32) -> String {
        match self.lang {
            Lang::En => format!("{healthy}/{enabled} sources up"),
            Lang::De => format!("{healthy}/{enabled} Quellen erreichbar"),
        }
    }

    /// "12 s ago", "vor 2 Std. 10 Min." — and "just now" for the first
    /// seconds, where a number only looks like jitter.
    pub fn ago(&self, secs: i64) -> String {
        let secs = secs.max(0);
        if secs < 5 {
            return self.just_now.to_string();
        }
        match self.lang {
            Lang::En => format!("{} ago", self.duration(secs)),
            Lang::De => format!("vor {}", self.duration(secs)),
        }
    }

    /// "in 2 h 10 min". A time that has already passed is "any moment":
    /// an estimate that ran out is not a download that finished.
    pub fn eta(&self, secs: i64) -> String {
        if secs < 30 {
            return self.eta_soon.to_string();
        }
        format!("in {}", self.duration(secs))
    }

    /// Two units at most: "2 h 10 min", "3 d 4 h", "45 s".
    pub fn duration(&self, secs: i64) -> String {
        let (s, min, h, d) = match self.lang {
            Lang::En => ("s", "min", "h", "d"),
            Lang::De => ("Sek.", "Min.", "Std.", "Tg."),
        };
        let secs = secs.max(0);
        let days = secs / 86_400;
        let hours = (secs % 86_400) / 3_600;
        let minutes = (secs % 3_600) / 60;
        if days > 0 {
            if hours > 0 {
                format!("{days} {d} {hours} {h}")
            } else {
                format!("{days} {d}")
            }
        } else if hours > 0 {
            if minutes > 0 {
                format!("{hours} {h} {minutes} {min}")
            } else {
                format!("{hours} {h}")
            }
        } else if minutes > 0 {
            format!("{minutes} {min}")
        } else {
            format!("{secs} {s}")
        }
    }

    /// A day as people say it: "today", "tomorrow", otherwise
    /// "Mon 20 Oct" / "Mo., 20. Okt.".
    pub fn day(&self, when: &Zoned, now: &Zoned) -> String {
        let days = (when.date() - now.date()).get_days();
        match days {
            0 => self.today.to_string(),
            1 => self.tomorrow.to_string(),
            -1 => self.yesterday.to_string(),
            _ => self.date(when),
        }
    }

    pub fn date(&self, when: &Zoned) -> String {
        let weekday = when.weekday().to_monday_zero_offset() as usize;
        let month = (when.month() - 1) as usize;
        match self.lang {
            Lang::En => {
                const D: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
                const M: [&str; 12] = [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ];
                format!("{} {} {}", D[weekday], when.day(), M[month])
            }
            Lang::De => {
                const D: [&str; 7] = ["Mo.", "Di.", "Mi.", "Do.", "Fr.", "Sa.", "So."];
                const M: [&str; 12] = [
                    "Jan.", "Feb.", "März", "Apr.", "Mai", "Juni", "Juli", "Aug.", "Sep.", "Okt.",
                    "Nov.", "Dez.",
                ];
                format!("{} {}. {}", D[weekday], when.day(), M[month])
            }
        }
    }

    /// The day and the time: "today, 21:00", "Fr. 24. Okt., 20:15".
    pub fn day_time(&self, when: &Zoned, now: &Zoned) -> String {
        format!(
            "{}, {:02}:{:02}",
            self.day(when, now),
            when.hour(),
            when.minute()
        )
    }

    /// "4.2 GB" / "4,2 GB". Decimal units: what people see on a shop shelf.
    pub fn size(&self, bytes: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1000.0 && unit < UNITS.len() - 1 {
            value /= 1000.0;
            unit += 1;
        }
        let text = if unit == 0 || value >= 100.0 {
            format!("{value:.0}")
        } else {
            format!("{value:.1}")
        };
        let text = match self.lang {
            Lang::En => text,
            Lang::De => text.replace('.', ","),
        };
        format!("{text} {}", UNITS[unit])
    }

    pub fn percent(&self, fraction: f32) -> String {
        let p = (fraction.clamp(0.0, 1.0) * 100.0).floor() as u32;
        match self.lang {
            Lang::En => format!("{p}%"),
            Lang::De => format!("{p} %"),
        }
    }

    pub fn kind(&self, kind: MediaKind) -> &'static str {
        match (self.lang, kind) {
            (Lang::En, MediaKind::Movie) => "Movie",
            (Lang::En, MediaKind::Episode) => "Episode",
            (Lang::En, MediaKind::Album) => "Album",
            (Lang::En, MediaKind::Other) => "Download",
            (Lang::De, MediaKind::Movie) => "Film",
            (Lang::De, MediaKind::Episode) => "Folge",
            (Lang::De, MediaKind::Album) => "Album",
            (Lang::De, MediaKind::Other) => "Download",
        }
    }

    pub fn transport(&self, transport: Transport) -> &'static str {
        match transport {
            Transport::Usenet => "Usenet",
            Transport::Torrent => "Torrent",
            Transport::Hoster => "Hoster",
            Transport::Soulseek => "Soulseek",
            Transport::Unknown => match self.lang {
                Lang::En => "unknown",
                Lang::De => "unbekannt",
            },
        }
    }

    pub fn item_state(&self, state: ItemState) -> &'static str {
        match (self.lang, state) {
            (Lang::En, ItemState::Queued) => "waiting",
            (Lang::En, ItemState::Downloading) => "downloading",
            (Lang::En, ItemState::Paused) => "paused",
            (Lang::En, ItemState::Importing) => "being filed away",
            (Lang::En, ItemState::Stalled) => "stuck",
            (Lang::En, ItemState::Warning) => "needs a look",
            (Lang::En, ItemState::Failed) => "failed",
            (Lang::En, ItemState::Completed) => "done",
            (Lang::De, ItemState::Queued) => "wartet",
            (Lang::De, ItemState::Downloading) => "lädt",
            (Lang::De, ItemState::Paused) => "pausiert",
            (Lang::De, ItemState::Importing) => "wird einsortiert",
            (Lang::De, ItemState::Stalled) => "hängt",
            (Lang::De, ItemState::Warning) => "braucht einen Blick",
            (Lang::De, ItemState::Failed) => "fehlgeschlagen",
            (Lang::De, ItemState::Completed) => "fertig",
        }
    }

    pub fn request_state(&self, state: RequestState) -> &'static str {
        match (self.lang, state) {
            (Lang::En, RequestState::Pending) => "waiting for approval",
            (Lang::En, RequestState::Declined) => "declined",
            (Lang::En, RequestState::Processing) => "on its way",
            (Lang::En, RequestState::Partial) => "partly there",
            (Lang::En, RequestState::Available) => "available",
            (Lang::En, RequestState::Failed) => "failed",
            (Lang::De, RequestState::Pending) => "wartet auf Freigabe",
            (Lang::De, RequestState::Declined) => "abgelehnt",
            (Lang::De, RequestState::Processing) => "unterwegs",
            (Lang::De, RequestState::Partial) => "teilweise da",
            (Lang::De, RequestState::Available) => "ist da",
            (Lang::De, RequestState::Failed) => "fehlgeschlagen",
        }
    }

    /// What a source does, in words — never the product's own name alone,
    /// which means nothing to a friend.
    pub fn source_kind(&self, kind: SourceKind) -> &'static str {
        match (self.lang, kind) {
            (Lang::En, SourceKind::Radarr) => "movies",
            (Lang::En, SourceKind::Sonarr) => "series",
            (Lang::En, SourceKind::Lidarr) => "music",
            (Lang::En, SourceKind::Prowlarr) => "search",
            (Lang::En, SourceKind::Sabnzbd)
            | (Lang::En, SourceKind::Qbittorrent)
            | (Lang::En, SourceKind::Pyload)
            | (Lang::En, SourceKind::Slskd) => "downloads",
            (Lang::En, SourceKind::Seerr) => "requests",
            (Lang::De, SourceKind::Radarr) => "Filme",
            (Lang::De, SourceKind::Sonarr) => "Serien",
            (Lang::De, SourceKind::Lidarr) => "Musik",
            (Lang::De, SourceKind::Prowlarr) => "Suche",
            (Lang::De, SourceKind::Sabnzbd)
            | (Lang::De, SourceKind::Qbittorrent)
            | (Lang::De, SourceKind::Pyload)
            | (Lang::De, SourceKind::Slskd) => "Downloads",
            (Lang::De, SourceKind::Seerr) => "Wünsche",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_keep_two_units_at_most() {
        assert_eq!(EN.duration(45), "45 s");
        assert_eq!(EN.duration(60 * 45), "45 min");
        assert_eq!(EN.duration(2 * 3600 + 10 * 60 + 7), "2 h 10 min");
        assert_eq!(EN.duration(3 * 3600), "3 h");
        assert_eq!(EN.duration(3 * 86_400 + 4 * 3600 + 59), "3 d 4 h");
        assert_eq!(DE.duration(2 * 3600 + 10 * 60), "2 Std. 10 Min.");
    }

    #[test]
    fn relative_times_read_naturally() {
        assert_eq!(EN.eta(2 * 3600 + 10 * 60), "in 2 h 10 min");
        assert_eq!(EN.eta(-100), "any moment", "an estimate that ran out");
        assert_eq!(EN.ago(12), "12 s ago");
        assert_eq!(DE.ago(12), "vor 12 Sek.");
        assert_eq!(EN.ago(1), "just now");
        assert_eq!(
            EN.ago(-3),
            "just now",
            "a clock a little ahead is not the future"
        );
    }

    #[test]
    fn sizes_use_the_decimal_mark_of_the_language() {
        assert_eq!(EN.size(4_200_000_000), "4.2 GB");
        assert_eq!(DE.size(4_200_000_000), "4,2 GB");
        assert_eq!(EN.size(512), "512 B");
        assert_eq!(EN.size(123_000_000), "123 MB");
    }

    #[test]
    fn days_are_relative_when_close_and_dates_otherwise() {
        let tz = jiff::tz::TimeZone::UTC;
        let now = jiff::civil::date(2026, 10, 20)
            .at(12, 0, 0, 0)
            .to_zoned(tz.clone())
            .unwrap();
        let tomorrow = jiff::civil::date(2026, 10, 21)
            .at(21, 5, 0, 0)
            .to_zoned(tz.clone())
            .unwrap();
        let later = jiff::civil::date(2026, 10, 24)
            .at(20, 15, 0, 0)
            .to_zoned(tz)
            .unwrap();
        assert_eq!(EN.day_time(&tomorrow, &now), "tomorrow, 21:05");
        assert_eq!(EN.day(&later, &now), "Sat 24 Oct");
        assert_eq!(DE.day_time(&later, &now), "Sa. 24. Okt., 20:15");
    }

    #[test]
    fn percent_never_rounds_up_to_done() {
        assert_eq!(EN.percent(0.999), "99%");
        assert_eq!(EN.percent(1.5), "100%");
        assert_eq!(DE.percent(0.42), "42 %");
    }
}
