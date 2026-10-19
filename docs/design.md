# Design decisions

Short, and each with the alternative that was turned down, so the question
does not start from scratch a second time.

## The collector holds the keys, the page holds none

Radarr, Sonarr, Lidarr and Prowlarr have one kind of API key, and it is an
admin key. SABnzbd's full key, qBittorrent's and pyLoad's passwords are the
same. A page reachable from the internet that holds them is one bug away from
somebody emptying the library.

*Turned down:* one process that calls every service on each page view. Simpler,
but it puts every key on the machine with the outside edge, and needs network
paths from there into every zone the services live in.

## The snapshot is a closed list of fields

What crosses from `collect` to `serve` is `src/snapshot.rs` and nothing else.
No field holds a message from a service. The reason is concrete: download
clients and managers put tracker errors into status messages, and tracker
errors quote the announce URL — with the passkey in it. Paths, indexer names
and client names stay out for the same kind of reason: they say more about the
setup than the people reading the page need.

The one exception is a name: a download no manager claims has nothing but the
release or file name its client shows, and the page would be useless without
it. So that name crosses — after one filter, `collect::title::client_title`,
which every source calls for every such name (and the managers for their
release-name fallback). Whatever looks like a URL, a query, a key or a path
becomes the fixed marker "(unnamed download)", which `serve` translates; the
rest is capped at 200 characters. This is a deny-list on shape, and that is
acceptable here because what it guards against is not a release name — which
says what is downloading, the very thing the page reports — but a client that
puts something else in the name's place: SABnzbd shows a job added by URL as
`Trying <url>` with the indexer's key in it, pyLoad an unchecked link,
qBittorrent a magnet without metadata.

*Turned down:* dropping client-only downloads from the page, or naming them
only by transport ("a torrent"). Safe, but the queue would then hide exactly
the downloads nobody else can explain.

A test feeds fixtures with a passkey and a path in every free-text field, and
URLs with keys in every name field, and checks that none of it reaches the
serialized snapshot; the VM test does the same end to end.

## Own OIDC, even behind a forward-auth proxy

queuewatch signs people in itself (Authorization Code + PKCE, `openidconnect`).

*Turned down:* trusting `X-Forwarded-User`-style headers from a forward-auth
proxy, with or without checking a JWT the proxy passes along. It ties the
application to one kind of proxy, and a forward-auth bypass (they happen, and
they fail open) would then be a bypass of queuewatch too. With its own sign-in
a proxy in front is a second door, not the only one.

Discovery retries for a few minutes at startup: after a deploy the provider
may itself still be starting, and a page that gave up then would run without a
working sign-in until someone restarts it. It also runs again later — every
hour, and after a sign-in fails on the token's signature (at most once a
minute) — because discovery is where the provider's signing keys come from,
and a provider that rotates them would otherwise lock everybody out until a
restart.

"My requests" trusts two things from the ID token: the `user_claim`, which
must be one people cannot change at the provider themselves, and the address,
which counts only when the provider says `email_verified: true`.

## No JavaScript, no database

The page refreshes itself every 30 seconds with a meta tag; progress bars are
native `<progress>` elements, since the policy allows no inline style. The session is an encrypted cookie; the collector keeps its
snapshot in memory. Nothing to migrate, nothing to back up, and a
Content-Security-Policy of `default-src 'none'` with only the stylesheet let
back in.
