# queuewatch

A read-only status page for your *arr stack, for the people who use your
server — not only for whoever administers it.

Your friends want to know *"where is my film?"*. The answer is spread over
Seerr, Radarr, Sonarr, Lidarr, bindery, SABnzbd, qBittorrent, pyLoad, slskd
and Prowlarr, and every one of them would happily hand an admin API key to
whoever can reach it. queuewatch puts one page in front of all that:

- **My requests** — your own Seerr requests, with state, progress and ETA.
- **Downloading now** — the whole queue across managers and download clients,
  merged, one line per download.
- **Coming up / Recently added** — the calendar and the latest imports.
- **Search & services** — "search works: 7 of 8 sources reachable", without
  ever naming an indexer.

Sign-in is OIDC (Authorization Code with PKCE) against any provider —
Authentik, Keycloak, Zitadel, Pocket-ID. There is no JavaScript and no
database.

## Two halves, one binary

The *arr services have no read-only API keys: the key that lists the queue
also deletes the library. So queuewatch splits in two:

| | `queuewatch collect` | `queuewatch serve` |
|---|---|---|
| runs | next to the services | where people can reach it |
| holds | the API keys | an OIDC client secret, a cookie key, a token |
| talks to | the services | the collector, and the OIDC provider |
| offers | `GET /snapshot` behind a bearer token | the page |

The collector polls every service, and writes what it learned into a
**snapshot built from a closed list of fields** (`src/snapshot.rs`). Free-text
messages never make it in — that is where trackers put announce URLs with
passkeys — and neither do paths, indexer names or client names. A problem
becomes a state (`stalled`, `warning`, `failed`). If the page were ever
reachable without signing in, the snapshot is all it could leak.

One thing in it does come from a service as text: the release or file name of
a download that no manager claims (a torrent added by hand, a pyLoad link) —
it is the only name such a download has. It passes one filter first
(`src/collect/title.rs`): anything that looks like a URL, a query, a key or a
path is shown as "(unnamed download)", and every name is cut at 200
characters. That is enough because a real release name says what is being
downloaded, which is what the page is for, and nothing about the setup; the
danger is the moment a client shows something else in that place — SABnzbd
names a job added by URL `Trying https://indexer/getnzb?apikey=…` until it
has fetched it.

You can run both halves on one machine; you gain the most by not doing so.

## Configuration

One TOML file, `QUEUEWATCH_CONFIG` (default `/etc/queuewatch/queuewatch.toml`).
Each subcommand reads its own section. Secrets are always files (`*_file`),
never values.

```toml
[collect]
listen = "10.0.0.5:8095"
token_file = "/run/credentials/queuewatch-collect.service/snapshot-token"
interval_secs = 30      # default
timeout_secs = 10       # per request, default
calendar_days = 7       # default
recent_hours = 72       # default

[[collect.radarr]]      # also: sonarr, lidarr, prowlarr, sabnzbd, slskd, seerr, bindery
label = "Movies"
url = "http://127.0.0.1:7878"
api_key_file = "/run/credentials/queuewatch-collect.service/radarr"

[[collect.qbittorrent]] # also: pyload
label = "Torrents"
url = "http://127.0.0.1:8080"
username = "admin"
password_file = "/run/credentials/queuewatch-collect.service/qbittorrent"

[serve]
listen = "127.0.0.1:8096"
public_url = "https://queue.example.org"   # redirect URI: <public_url>/auth/callback
snapshot_url = "http://10.0.0.5:8095/snapshot"
snapshot_token_file = "/run/credentials/queuewatch-serve.service/snapshot-token"
cookie_key_file = "/run/credentials/queuewatch-serve.service/cookie-key"  # >= 64 bytes
home_url = "https://example.org"         # optional: a way back to your start page
language = "en"         # or "de"
snapshot_max_age_secs = 15

[serve.oidc]
issuer = "https://auth.example.org/application/o/queuewatch/"
client_id = "queuewatch"
client_secret_file = "/run/credentials/queuewatch-serve.service/oidc-client-secret"
user_claim = "preferred_username"   # matched against Seerr's user name
group_claim = "groups"
required_group = "friends"          # optional
```

### Books: bindery

[bindery](https://github.com/vavallee/bindery), the Readarr successor, is
configured like the other managers, with the API key from its settings:

```toml
[[collect.bindery]]
label = "Books"
url = "http://127.0.0.1:8787"
api_key_file = "/run/credentials/queuewatch-collect.service/bindery"
```

The collector reads its queue (`GET /api/v1/queue`) and its import history
(`GET /api/v1/history?eventType=bookImported`), with the key in the
`X-Api-Key` header. A queue line is "Author – Book Title", tagged *Book* or
*Audiobook*, and merges with the SABnzbd or qBittorrent entry of the same
download. bindery does not record the format on a download, so the tag comes
from the format token in the release name (`m4b`, `mp3`, … mean audiobook,
anything else a book) — the same inference bindery itself makes when it
picks the download client's category. Its wanted list is not read.

"My requests" matches a Seerr request to the signed-in person when Seerr's
user name equals the `user_claim`, or the e-mail addresses are equal. An
address counts only when the provider marks it `email_verified: true`.

**`user_claim` must be a claim people cannot change themselves at the
provider.** The name is the key to someone's requests: if anyone can rename
their own account to `konrad`, they read Konrad's. On most providers
`preferred_username` is safe only while self-service renaming is switched off;
if it is not, pick a claim only an admin sets.

## Network

The snapshot travels from `collect` to `serve` over plain HTTP, and so does
the bearer token that unlocks it. The snapshot holds requesters' user names
and e-mail addresses. Keep the path between the two halves on a network you
trust (one host, a VPN such as WireGuard or Tailscale, a private VLAN) or put
TLS in front of the collector. On NixOS,
`services.queuewatch.collect.allowedClients` narrows who may connect at all.

## NixOS

```nix
{
  inputs.queuewatch.url = "github:achimcc/queuewatch/v0.2.1";
  # in a module:
  imports = [ inputs.queuewatch.nixosModules.default ];
  services.queuewatch = {
    package = inputs.queuewatch.packages.x86_64-linux.default;
    collect = {
      enable = true;
      snapshotTokenFile = "/run/secrets/queuewatch-token";
      credentials.radarr = "/run/secrets/radarr-api-key";
      settings = {
        listen = "10.0.0.5:8095";
        radarr = [{ label = "Movies"; url = "http://127.0.0.1:7878";
                    api_key_file = "/run/credentials/queuewatch-collect.service/radarr"; }];
      };
    };
  };
}
```

The page half is `services.queuewatch.serve` with `snapshotTokenFile`,
`cookieKeyFile` and `oidcClientSecretFile`. Both units run with a tight
systemd sandbox, bounded memory and tasks, and get their secrets through
`LoadCredential`.

`collect.allowedClients = [ "10.0.0.7" ];` restricts the collector unit to
`localhost` and these addresses (systemd's `IPAddressAllow`, everything else
denied). The list filters **both directions**: every service the collector
polls must sit on `localhost` or in it as well, or that service shows as
unreachable.

## Development

```sh
nix develop
cargo test
nix flake check   # tests, clippy, fmt, cargo-audit, cargo-deny, a NixOS VM test
```

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
