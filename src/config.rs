//! The configuration file. One file, two sections: `[collect]` for the side
//! that holds the API keys, `[serve]` for the side people reach. Each
//! subcommand reads its own section and refuses to start without it; the
//! other may be absent, and on a well-split deployment it is.
//!
//! Secrets are never in this file, only the paths to them (`*_file`). An
//! environment variable or a config value shows up in `/proc/<pid>/environ`,
//! in `systemctl show`, in a store path; a file under
//! `/run/credentials/<unit>/` does not.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub collect: Option<CollectConfig>,
    pub serve: Option<ServeConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectConfig {
    /// Where `GET /snapshot` listens, e.g. `10.0.4.2:8095`.
    pub listen: String,
    /// The bearer token `serve` must present.
    pub token_file: PathBuf,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Per-request timeout towards each service.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// How many days ahead the calendar looks.
    #[serde(default = "default_calendar_days")]
    pub calendar_days: u32,
    /// How many hours back "recently imported" looks.
    #[serde(default = "default_recent_hours")]
    pub recent_hours: u32,
    #[serde(default)]
    pub radarr: Vec<ApiKeyInstance>,
    #[serde(default)]
    pub sonarr: Vec<ApiKeyInstance>,
    #[serde(default)]
    pub lidarr: Vec<ApiKeyInstance>,
    #[serde(default)]
    pub prowlarr: Vec<ApiKeyInstance>,
    #[serde(default)]
    pub sabnzbd: Vec<ApiKeyInstance>,
    #[serde(default)]
    pub qbittorrent: Vec<PasswordInstance>,
    #[serde(default)]
    pub pyload: Vec<PasswordInstance>,
    #[serde(default)]
    pub slskd: Vec<ApiKeyInstance>,
    #[serde(default)]
    pub seerr: Vec<ApiKeyInstance>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyInstance {
    /// Shown on the page: "Movies", "4K", "Music". Not a host name.
    pub label: String,
    /// Base URL including any URL base, e.g. `http://10.0.2.5:7878/radarr`.
    pub url: String,
    pub api_key_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordInstance {
    pub label: String,
    pub url: String,
    pub username: String,
    pub password_file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServeConfig {
    pub listen: String,
    /// The address people use, without trailing slash, e.g.
    /// `https://queue.example.org`. The OIDC redirect URI is
    /// `<public_url>/auth/callback`.
    pub public_url: String,
    /// The collector's snapshot endpoint, e.g. `http://10.0.2.5:8095/snapshot`.
    pub snapshot_url: String,
    pub snapshot_token_file: PathBuf,
    /// 64 bytes or more, raw or base64 — the key of the private session cookie.
    pub cookie_key_file: PathBuf,
    /// UI language: `en` or `de`.
    #[serde(default = "default_language")]
    pub language: String,
    /// How long a fetched snapshot is reused before asking the collector again.
    #[serde(default = "default_snapshot_max_age")]
    pub snapshot_max_age_secs: u64,
    pub oidc: OidcConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    /// e.g. `https://auth.example.org/application/o/queuewatch/`
    pub issuer: String,
    pub client_id: String,
    pub client_secret_file: PathBuf,
    /// The claim that names the person, matched against the request manager's
    /// user name.
    #[serde(default = "default_user_claim")]
    pub user_claim: String,
    #[serde(default = "default_group_claim")]
    pub group_claim: String,
    /// When set, only members of this group get in. Unset: anyone the
    /// provider signs in.
    #[serde(default)]
    pub required_group: Option<String>,
}

fn default_interval() -> u64 {
    30
}
fn default_timeout() -> u64 {
    10
}
fn default_calendar_days() -> u32 {
    7
}
fn default_recent_hours() -> u32 {
    72
}
fn default_language() -> String {
    "en".into()
}
fn default_snapshot_max_age() -> u64 {
    15
}
fn default_user_claim() -> String {
    "preferred_username".into()
}
fn default_group_claim() -> String {
    "groups".into()
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        let config: Config =
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        if let Some(serve) = &config.serve
            && !matches!(serve.language.as_str(), "en" | "de")
        {
            anyhow::bail!(
                "serve.language must be \"en\" or \"de\", not {:?}",
                serve.language
            );
        }
        Ok(config)
    }
}

/// Reads a secret from a file, trimmed. An EMPTY file is refused: a
/// credential that never arrived otherwise lets the service start and look
/// healthy, and fails only at the first request — far from the cause.
pub fn read_secret(path: &Path) -> anyhow::Result<String> {
    let value = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?
        .trim()
        .to_string();
    if value.is_empty() {
        anyhow::bail!(
            "{} is empty; queuewatch does not start without it",
            path.display()
        );
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_are_refused() {
        let err = toml::from_str::<Config>("[collect]\nlisten='x'\ntoken_file='t'\nradar=[]\n")
            .unwrap_err();
        assert!(err.to_string().contains("radar"), "{err}");
    }

    #[test]
    fn a_collect_only_file_parses() {
        let c: Config = toml::from_str(
            "[collect]\nlisten='127.0.0.1:1'\ntoken_file='/t'\n\
             [[collect.radarr]]\nlabel='Movies'\nurl='http://r'\napi_key_file='/k'\n",
        )
        .unwrap();
        let collect = c.collect.unwrap();
        assert_eq!(collect.interval_secs, 30);
        assert_eq!(collect.radarr[0].label, "Movies");
        assert!(c.serve.is_none());
    }
}
