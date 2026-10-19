//! The page, as maud. One page with four sections, plus the few small pages
//! around the sign-in.
//!
//! The look is a terminal's: a prompt line, a green wordmark with a blinking
//! block cursor, `//`-commented mono section headings, square status lamps
//! and ASCII progress readouts (`style.css`). No JavaScript anywhere; the
//! page refreshes itself with a `<meta>` tag, and progress is a native
//! `<progress>` element — the Content-Security-Policy forbids inline
//! `style=""`, so a bar drawn with a CSS width would need one rule per
//! percent. The ASCII bar next to it is decoration and hidden from screen
//! readers; the `<progress>` carries the label.

use jiff::Zoned;
use jiff::tz::TimeZone;
use maud::{DOCTYPE, Markup, html};

use crate::serve::fetch::View;
use crate::serve::i18n::Strings;
use crate::serve::session::Session;
use crate::snapshot::{
    CalendarItem, ItemState, QueueItem, RecentItem, Request, RequestState, Snapshot, UNNAMED,
};

/// Everything the page needs besides the snapshot.
pub struct Context<'a> {
    pub s: &'static Strings,
    pub tz: &'a TimeZone,
    pub host: &'a str,
    /// `serve.home_url`: the header links back to it when set.
    pub home: Option<&'a str>,
    pub now: jiff::Timestamp,
}

impl Context<'_> {
    fn zoned(&self, t: jiff::Timestamp) -> Zoned {
        t.to_zoned(self.tz.clone())
    }
    fn now_zoned(&self) -> Zoned {
        self.zoned(self.now)
    }
    fn secs_since(&self, t: jiff::Timestamp) -> i64 {
        self.now.as_second() - t.as_second()
    }
    fn secs_until(&self, t: jiff::Timestamp) -> i64 {
        t.as_second() - self.now.as_second()
    }
}

/// Is this request the signed-in person's?
///
/// By name OR by address, each compared case-insensitively — but only in
/// ASCII. Unicode case folding maps the Kelvin sign U+212A onto an ASCII
/// `k` (treff, homeserver audit 3, B116), so a full fold would let
/// `\u{212A}onrad` see Konrad's requests. An address counts only when both
/// sides have one: two missing addresses are not the same address. The
/// signed-in side only has one when the provider marked it verified
/// (`oidc::claims_to_identity`); the name side relies on `oidc.user_claim`
/// being something people cannot change at the provider themselves.
pub fn is_mine(request: &Request, who: &Session) -> bool {
    let same = |a: Option<&str>, b: Option<&str>| match (a.map(str::trim), b.map(str::trim)) {
        (Some(a), Some(b)) => !a.is_empty() && !b.is_empty() && a.eq_ignore_ascii_case(b),
        _ => false,
    };
    same(
        request.requested_by.username.as_deref(),
        Some(who.username.as_str()),
    ) || same(request.requested_by.email.as_deref(), who.email.as_deref())
}

/// Where the source of this program lives. AGPL-3.0: whoever uses the page
/// over a network is owed a way to read it.
const SOURCE_URL: &str = "https://github.com/achimcc/queuewatch";

/// Cells in the ASCII progress bar.
const BAR_CELLS: usize = 16;

/// `[########--------]` for 52 %. Rounds down, like the percentage: a bar
/// is only full when the download is.
pub fn ascii_bar(fraction: f32) -> String {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = ((fraction * BAR_CELLS as f32).floor() as usize).min(BAR_CELLS);
    format!("[{}{}]", "#".repeat(filled), "-".repeat(BAR_CELLS - filled))
}

/// What the header shows besides the wordmark.
struct Top<'a> {
    who: Option<&'a str>,
    /// "updated 12 s ago", when there is a snapshot.
    age: Option<String>,
    /// A warning line under the tagline.
    notice: Markup,
}

fn frame(ctx: &Context, refresh: bool, top: Top, body: Markup) -> Markup {
    let s = ctx.s;
    let user = top.who.unwrap_or("queuewatch");
    html! {
        (DOCTYPE)
        html lang=(s.lang.code()) {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="dark";
                @if refresh {
                    meta http-equiv="refresh" content="30";
                }
                title { "queuewatch" }
                link rel="stylesheet" href=(concat!("/style.css?v=", env!("CARGO_PKG_VERSION")));
            }
            body {
                header.top {
                    div.wrap {
                        @if ctx.home.is_some() || top.age.is_some() || top.who.is_some() {
                            div.session {
                                @if let Some(home) = ctx.home {
                                    // The way back, in the page's own voice. The
                                    // visible text is a command; what a screen
                                    // reader says is the sentence behind it.
                                    a.home href=(home) aria-label={ (s.back_to) " " (super::url_host(home)) } {
                                        "cd ~/" (super::url_host(home))
                                    }
                                }
                                @if let Some(age) = &top.age {
                                    span.age { (age) }
                                }
                                @if let Some(name) = top.who {
                                    span.who { (s.signed_in_as) " " strong { (name) } }
                                    form.signout method="post" action="/auth/logout" {
                                        button.btn.btn-out type="submit" { (s.logout) }
                                    }
                                }
                            }
                        }
                        p.prompt aria-hidden="true" {
                            b { (user) "@" (ctx.host) } ":~$ watch -n30 queue --all"
                        }
                        h1.wordmark { "QUEUEWATCH" span.cursor aria-hidden="true" {} }
                        p.tagline { (s.tagline) }
                        (top.notice)
                    }
                }
                main { div.wrap { (body) } }
                footer.foot {
                    div.wrap {
                        p.prompt { b { "$" } " echo \"" (s.footer_echo) "\"" }
                        p {
                            a href=(SOURCE_URL) rel="noopener" { (s.source_link) }
                            span.faint { " · AGPL-3.0" }
                        }
                    }
                }
            }
        }
    }
}

/// The page itself.
pub fn status_page(ctx: &Context, who: &Session, view: &View) -> Markup {
    let s = ctx.s;
    let notice = html! {
        @if !view.reachable {
            p.alert role="status" {
                span aria-hidden="true" { "! " }
                (s.unreachable) " — "
                @if let Some(snap) = &view.snapshot {
                    (s.last_good(ctx.secs_since(snap.taken_at)))
                } @else {
                    (s.no_data_yet)
                }
            }
        }
    };
    let body = match &view.snapshot {
        Some(snap) => sections(ctx, who, snap),
        None => empty(s.no_data_yet),
    };
    let top = Top {
        who: Some(&who.username),
        age: view
            .snapshot
            .as_ref()
            .map(|snap| s.updated(ctx.secs_since(snap.taken_at))),
        notice,
    };
    frame(ctx, true, top, body)
}

fn empty(text: &str) -> Markup {
    html! { p.empty { "-- " (text) " --" } }
}

fn section(id: &str, title: &str, content: Markup) -> Markup {
    html! {
        section.section id=(id) aria-labelledby=(format!("{id}-title")) {
            h2.section-title id=(format!("{id}-title")) { (title) }
            (content)
        }
    }
}

fn sections(ctx: &Context, who: &Session, snap: &Snapshot) -> Markup {
    let s = ctx.s;
    let mine: Vec<&Request> = snap.requests.iter().filter(|r| is_mine(r, who)).collect();
    html! {
        (section("mine", s.my_requests, html! {
            @if mine.is_empty() {
                (empty(s.my_requests_empty))
            } @else {
                ul.cards { @for r in &mine { (request(ctx, r)) } }
            }
        }))
        (section("queue", s.downloading, html! {
            @if snap.queue.is_empty() {
                (empty(s.downloading_empty))
            } @else {
                ul.cards { @for q in &snap.queue { (queue_item(ctx, q)) } }
            }
        }))
        div.pair {
            (section("calendar", s.coming_up, html! {
                @if snap.calendar.is_empty() {
                    (empty(s.coming_up_empty))
                } @else {
                    ul.rows { @for c in &snap.calendar { (calendar_item(ctx, c)) } }
                }
            }))
            (section("recent", s.recent, html! {
                @if snap.recent.is_empty() {
                    (empty(s.recent_empty))
                } @else {
                    ul.rows { @for r in &snap.recent { (recent_item(ctx, r)) } }
                }
            }))
        }
        (section("services", s.services, services(ctx, snap)))
    }
}

fn kind_tag(ctx: &Context, kind: crate::snapshot::MediaKind) -> Markup {
    html! { span.tag { (ctx.s.kind(kind)) } }
}

/// A square lamp and its word. `level` is `up`, `down` or `idle`.
fn lamp(level: &str, text: &str) -> Markup {
    html! {
        span class={ "lamp " (level) } {
            span.dot aria-hidden="true" {}
            span.lamp-text { (text) }
        }
    }
}

fn progress(
    ctx: &Context,
    title: &str,
    fraction: f32,
    eta: Option<jiff::Timestamp>,
    size: Option<u64>,
) -> Markup {
    let s = ctx.s;
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    html! {
        div.progress {
            progress value=(format!("{fraction:.3}")) max="1"
                aria-label=(format!("{}: {title}", s.progress_label)) {
                (s.percent(fraction))
            }
            p.readout {
                span.ascii aria-hidden="true" { (ascii_bar(fraction)) }
                span.pct { (s.percent(fraction)) }
                @if let Some(eta) = eta {
                    span.faint { (s.eta(ctx.secs_until(eta))) }
                }
                @if let Some(size) = size {
                    span.faint { (s.size(size)) }
                }
            }
        }
    }
}

fn item_level(state: ItemState) -> &'static str {
    match state {
        ItemState::Downloading | ItemState::Importing | ItemState::Completed => "up",
        ItemState::Queued | ItemState::Paused => "idle",
        ItemState::Stalled | ItemState::Warning | ItemState::Failed => "down",
    }
}

fn request_level(state: RequestState) -> &'static str {
    match state {
        RequestState::Available | RequestState::Processing | RequestState::Partial => "up",
        RequestState::Pending => "idle",
        RequestState::Declined | RequestState::Failed => "down",
    }
}

fn request(ctx: &Context, r: &Request) -> Markup {
    let s = ctx.s;
    let requested = s.day(&ctx.zoned(r.requested_at), &ctx.now_zoned());
    html! {
        li.card {
            div.card-head {
                (kind_tag(ctx, r.kind))
                (lamp(request_level(r.state), s.request_state(r.state)))
            }
            h3.title { (r.title) }
            p.meta { (s.requested(&requested)) }
            @if let Some(p) = r.progress {
                (progress(ctx, &r.title, p, r.eta, None))
            }
        }
    }
}

/// A queue title as the page shows it. The collector's marker for a name it
/// withheld is English by construction; here it becomes the page's language.
fn queue_title<'a>(ctx: &Context, q: &'a QueueItem) -> &'a str {
    if q.title == UNNAMED {
        ctx.s.unnamed_download
    } else {
        &q.title
    }
}

fn queue_item(ctx: &Context, q: &QueueItem) -> Markup {
    let s = ctx.s;
    let title = queue_title(ctx, q);
    html! {
        li.card {
            div.card-head {
                span.tags {
                    (kind_tag(ctx, q.kind))
                    span.tag { (s.transport(q.transport)) }
                }
                (lamp(item_level(q.state), s.item_state(q.state)))
            }
            h3.title { (title) }
            (progress(ctx, title, q.progress, q.eta, q.size_bytes))
        }
    }
}

fn calendar_item(ctx: &Context, c: &CalendarItem) -> Markup {
    let s = ctx.s;
    let when = ctx.zoned(c.airs_at);
    html! {
        li.row {
            time.when datetime=(c.airs_at.to_string()) { (s.day_time(&when, &ctx.now_zoned())) }
            (kind_tag(ctx, c.kind))
            h3.title { (c.title) }
            @if c.has_file {
                (lamp("up", s.already_there))
            }
        }
    }
}

fn recent_item(ctx: &Context, r: &RecentItem) -> Markup {
    let s = ctx.s;
    html! {
        li.row {
            time.when datetime=(r.imported_at.to_string()) { (s.ago(ctx.secs_since(r.imported_at))) }
            (kind_tag(ctx, r.kind))
            h3.title { (r.title) }
        }
    }
}

fn services(ctx: &Context, snap: &Snapshot) -> Markup {
    let s = ctx.s;
    let i = snap.indexers;
    let (level, text) = if !i.known {
        ("idle", s.search_unknown.to_string())
    } else if i.enabled == 0 {
        ("idle", s.search_none.to_string())
    } else if i.healthy < i.enabled {
        ("down", s.sources_up(i.healthy, i.enabled))
    } else {
        ("up", s.sources_up(i.healthy, i.enabled))
    };
    html! {
        p.term {
            span.cmd aria-hidden="true" { "$ search --health" }
            span.arrow aria-hidden="true" { " → " }
            span class={ "result " (level) } { (text) }
        }
        @if !snap.sources.is_empty() {
            ul.sources aria-label=(s.sources_label) {
                @for src in &snap.sources {
                    li {
                        span.source-name { (src.label) }
                        span.faint { " · " (s.source_kind(src.kind)) }
                        (lamp(
                            if src.available { "up" } else { "down" },
                            if src.available { s.reachable } else { s.not_reachable },
                        ))
                    }
                }
            }
        }
    }
}

/// The page for "signed in, but not let in", and for sign-in trouble.
pub fn message_page(
    ctx: &Context,
    who: Option<&str>,
    title: &str,
    text: &str,
    retry: bool,
) -> Markup {
    let body = html! {
        section.section.message {
            h2.section-title { (title) }
            p { (text) }
            @if retry {
                p { a.btn href="/auth/login" { (ctx.s.try_again) } }
            }
        }
    };
    let top = Top {
        who,
        age: None,
        notice: html! {},
    };
    frame(ctx, false, top, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{MediaIds, MediaKind, Requester};

    fn who(username: &str, email: Option<&str>) -> Session {
        Session {
            username: username.into(),
            email: email.map(String::from),
            groups: vec![],
            expires_at: i64::MAX,
        }
    }

    fn by(username: Option<&str>, email: Option<&str>) -> Request {
        Request {
            kind: MediaKind::Movie,
            title: "T".into(),
            ids: MediaIds::default(),
            requested_by: Requester {
                username: username.map(String::from),
                email: email.map(String::from),
            },
            requested_at: jiff::Timestamp::UNIX_EPOCH,
            state: RequestState::Pending,
            progress: None,
            eta: None,
        }
    }

    #[test]
    fn a_request_is_mine_by_name_or_by_address() {
        let ada = who("Ada", Some("Ada@Example.org"));
        assert!(is_mine(&by(Some("ada"), None), &ada));
        assert!(is_mine(&by(Some(" ADA "), None), &ada));
        assert!(is_mine(&by(Some("someone"), Some("ada@example.org")), &ada));
        assert!(!is_mine(&by(Some("konrad"), Some("k@example.org")), &ada));
    }

    #[test]
    fn two_missing_addresses_are_not_the_same_address() {
        let ada = who("ada", None);
        assert!(!is_mine(&by(Some("konrad"), None), &ada));
        let ada_blank = who("ada", Some(""));
        assert!(!is_mine(&by(Some("konrad"), Some("")), &ada_blank));
        assert!(!is_mine(&by(None, None), &ada));
    }

    /// NOTHING OUTSIDE ASCII IS FOLDED (treff, homeserver audit 3, B116):
    /// the Kelvin sign lower-cases to an ASCII `k` under Unicode rules.
    #[test]
    fn a_letter_that_only_folds_into_ascii_does_not_match() {
        let konrad_impostor = who("\u{212A}onrad", None);
        assert!(!is_mine(&by(Some("konrad"), None), &konrad_impostor));
    }

    #[test]
    fn the_ascii_bar_rounds_down_and_stays_in_its_cells() {
        assert_eq!(ascii_bar(0.0), "[----------------]");
        assert_eq!(ascii_bar(0.52), "[########--------]");
        assert_eq!(ascii_bar(0.999), "[###############-]");
        assert_eq!(ascii_bar(1.0), "[################]");
        assert_eq!(ascii_bar(7.0), "[################]");
        assert_eq!(ascii_bar(f32::NAN), "[----------------]");
    }
}
