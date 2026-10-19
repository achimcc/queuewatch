//! The page, as maud. One page with four sections, plus the few small pages
//! around the sign-in.
//!
//! The clothes are those of the operator's front page, as reelpick wears
//! them: the prompt line, the green wordmark with its block cursor, the
//! bracketed mono section headings (`style.css`). No JavaScript anywhere; the
//! page refreshes itself with a `<meta>` tag, and progress is a native
//! `<progress>` element — the Content-Security-Policy forbids inline
//! `style=""`, so a bar drawn with a CSS width would need one rule per
//! percent.

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

fn frame(ctx: &Context, refresh: bool, who: Option<&str>, head: Markup, body: Markup) -> Markup {
    let s = ctx.s;
    html! {
        (DOCTYPE)
        html lang=(s.lang.code()) {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="color-scheme" content="dark light";
                @if refresh {
                    meta http-equiv="refresh" content="30";
                }
                title { "queuewatch" }
                link rel="stylesheet" href=(concat!("/style.css?v=", env!("CARGO_PKG_VERSION")));
            }
            body {
                header.kopf {
                    div.bahn {
                        p.prompt { b { "queuewatch@" (ctx.host) } ":~$ status" }
                        div.kopfzeile {
                            p.wortmarke { "QUEUEWATCH" span.kursor aria-hidden="true" {} }
                            @if let Some(name) = who {
                                div.wer {
                                    span.leise { (s.signed_in_as) " " }
                                    strong { (name) }
                                    form.abmelden method="post" action="/auth/logout" {
                                        button.knopf type="submit" { (s.logout) }
                                    }
                                }
                            }
                        }
                        (head)
                    }
                }
                main { div.bahn { (body) } }
                footer {
                    div.bahn {
                        p.prompt { b { "$" } " queuewatch --read-only" }
                    }
                }
            }
        }
    }
}

/// The page itself.
pub fn status_page(ctx: &Context, who: &Session, view: &View) -> Markup {
    let s = ctx.s;
    let head = html! {
        @if let Some(snap) = &view.snapshot {
            p.stand { (s.updated(ctx.secs_since(snap.taken_at))) }
        }
        @if !view.reachable {
            div.hinweis.warn role="status" {
                p { (s.unreachable) }
                @if let Some(snap) = &view.snapshot {
                    p { (s.last_good(ctx.secs_since(snap.taken_at))) }
                } @else {
                    p { (s.no_data_yet) }
                }
            }
        }
    };
    let body = match &view.snapshot {
        Some(snap) => sections(ctx, who, snap),
        None => html! { p.leer { (s.no_data_yet) } },
    };
    frame(ctx, true, Some(&who.username), head, body)
}

fn section(id: &str, title: &str, content: Markup) -> Markup {
    html! {
        section.abschnitt id=(id) aria-labelledby=(format!("{id}-titel")) {
            h2.abschnitt-titel id=(format!("{id}-titel")) { (title) }
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
                p.leer { (s.my_requests_empty) }
            } @else {
                ul.liste { @for r in &mine { (request(ctx, r)) } }
            }
        }))
        (section("queue", s.downloading, html! {
            @if snap.queue.is_empty() {
                p.leer { (s.downloading_empty) }
            } @else {
                ul.liste { @for q in &snap.queue { (queue_item(ctx, q)) } }
            }
        }))
        div.paar {
            (section("calendar", s.coming_up, html! {
                @if snap.calendar.is_empty() {
                    p.leer { (s.coming_up_empty) }
                } @else {
                    ul.liste.knapp { @for c in &snap.calendar { (calendar_item(ctx, c)) } }
                }
            }))
            (section("recent", s.recent, html! {
                @if snap.recent.is_empty() {
                    p.leer { (s.recent_empty) }
                } @else {
                    ul.liste.knapp { @for r in &snap.recent { (recent_item(ctx, r)) } }
                }
            }))
        }
        (section("services", s.services, services(ctx, snap)))
    }
}

fn kind_tag(ctx: &Context, kind: crate::snapshot::MediaKind) -> Markup {
    html! { span.art { (ctx.s.kind(kind)) } }
}

fn progress(
    ctx: &Context,
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
        div.fortschritt {
            progress value=(format!("{fraction:.3}")) max="1" aria-label=(s.progress_label) {
                (s.percent(fraction))
            }
            p.zahlen {
                span { (s.percent(fraction)) }
                @if let Some(eta) = eta {
                    span { (s.eta(ctx.secs_until(eta))) }
                }
                @if let Some(size) = size {
                    span { (s.size(size)) }
                }
            }
        }
    }
}

fn item_badge(state: ItemState) -> &'static str {
    match state {
        ItemState::Downloading | ItemState::Importing | ItemState::Completed => "gut",
        ItemState::Queued | ItemState::Paused => "ruhig",
        ItemState::Stalled | ItemState::Warning => "warn",
        ItemState::Failed => "schlecht",
    }
}

fn request_badge(state: RequestState) -> &'static str {
    match state {
        RequestState::Available => "gut",
        RequestState::Processing | RequestState::Partial => "unterwegs",
        RequestState::Pending => "ruhig",
        RequestState::Declined | RequestState::Failed => "schlecht",
    }
}

fn request(ctx: &Context, r: &Request) -> Markup {
    let s = ctx.s;
    let requested = s.day(&ctx.zoned(r.requested_at), &ctx.now_zoned());
    html! {
        li.eintrag {
            div.kopf-eintrag {
                (kind_tag(ctx, r.kind))
                h3.titel { (r.title) }
            }
            p.marken {
                span class={ "marke " (request_badge(r.state)) } { (s.request_state(r.state)) }
                span.leise { (s.requested(&requested)) }
            }
            @if let Some(p) = r.progress {
                (progress(ctx, p, r.eta, None))
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
    html! {
        li.eintrag {
            div.kopf-eintrag {
                (kind_tag(ctx, q.kind))
                h3.titel { (queue_title(ctx, q)) }
            }
            p.marken {
                span class={ "marke " (item_badge(q.state)) } { (s.item_state(q.state)) }
                span.marke.weg { (s.transport(q.transport)) }
            }
            (progress(ctx, q.progress, q.eta, q.size_bytes))
        }
    }
}

fn calendar_item(ctx: &Context, c: &CalendarItem) -> Markup {
    let s = ctx.s;
    let when = ctx.zoned(c.airs_at);
    html! {
        li.eintrag {
            p.wann { time datetime=(c.airs_at.to_string()) { (s.day_time(&when, &ctx.now_zoned())) } }
            div.kopf-eintrag {
                (kind_tag(ctx, c.kind))
                h3.titel { (c.title) }
            }
            @if c.has_file {
                p.marken { span.marke.gut { (s.already_there) } }
            }
        }
    }
}

fn recent_item(ctx: &Context, r: &RecentItem) -> Markup {
    let s = ctx.s;
    html! {
        li.eintrag {
            p.wann { time datetime=(r.imported_at.to_string()) { (s.ago(ctx.secs_since(r.imported_at))) } }
            div.kopf-eintrag {
                (kind_tag(ctx, r.kind))
                h3.titel { (r.title) }
            }
        }
    }
}

fn services(ctx: &Context, snap: &Snapshot) -> Markup {
    let s = ctx.s;
    let i = snap.indexers;
    let (class, text) = if !i.known {
        ("unbekannt", s.search_unknown.to_string())
    } else if i.enabled == 0 {
        ("unbekannt", s.search_none.to_string())
    } else if i.healthy == 0 {
        ("schlecht", s.search_down.to_string())
    } else if i.healthy < i.enabled {
        ("warn", s.search_partly(i.healthy, i.enabled))
    } else {
        ("gut", s.search_works(i.healthy, i.enabled))
    };
    html! {
        p class={ "suche " (class) } { (text) }
        @if !snap.sources.is_empty() {
            ul.dienste aria-label=(s.sources_label) {
                @for src in &snap.sources {
                    li class=(if src.available { "an" } else { "aus" }) {
                        span.punkt aria-hidden="true" {}
                        span.name { (src.label) }
                        span.leise { " · " (s.source_kind(src.kind)) }
                        span.zustand {
                            (if src.available { s.reachable } else { s.not_reachable })
                        }
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
        section.abschnitt.meldung {
            h1.abschnitt-titel { (title) }
            p { (text) }
            @if retry {
                p { a.knopf href="/auth/login" { (ctx.s.try_again) } }
            }
        }
    };
    frame(ctx, false, who, html! {}, body)
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
}
