//! The Spotify Web API client: token lifecycle, rate limiting, and the handful
//! of endpoints waytify uses.

use super::auth::{self, Tokens};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::time::{Duration, Instant};

const API: &str = "https://api.spotify.com/v1";

/// Requests are counted per application over a rolling window, so a single
/// misbehaving loop can get every user of the same client id throttled. This is
/// a floor on the gap between calls, independent of what the callers think they
/// are doing.
const MIN_REQUEST_GAP: Duration = Duration::from_millis(120);

const TIMEOUT: Duration = Duration::from_secs(15);

/// A Spotify Connect endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub is_active: bool,
    pub supports_volume: bool,
    pub volume_percent: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct Devices {
    devices: Vec<Device>,
}

#[derive(Debug, Deserialize)]
struct Me {
    product: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Playback {
    #[serde(default)]
    context: Option<PlaybackContext>,
}

#[derive(Debug, Deserialize)]
struct PlaybackContext {
    #[serde(rename = "type")]
    kind: waytify_ipc::ContextKind,
    /// Where to ask for the name. Spotify does not include it here.
    href: Option<String>,
    uri: Option<String>,
    #[serde(default)]
    external_urls: ExternalUrls,
}

#[derive(Debug, Default, Deserialize)]
struct ExternalUrls {
    spotify: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Named {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Playlists {
    items: Vec<PlaylistItem>,
}

#[derive(Debug, Deserialize)]
struct PlaylistItem {
    name: String,
    uri: Option<String>,
    #[serde(default)]
    tracks: Option<Count>,
}

#[derive(Debug, Deserialize)]
struct Count {
    total: u32,
}

#[derive(Debug, Deserialize)]
struct Queue {
    queue: Vec<Item>,
}

/// A queue entry, which is a track or an episode. Spotify discriminates them
/// with `type`, and an episode has a show where a track has artists.
#[derive(Debug, Deserialize)]
struct Item {
    name: String,
    /// Needed to play it. Without this the rows can only be looked at.
    uri: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    artists: Vec<Artist>,
    #[serde(default)]
    show: Option<Named>,
    #[serde(default)]
    duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Artist {
    name: String,
}

pub struct Client {
    http: reqwest::Client,
    client_id: String,
    tokens: Option<Tokens>,
    /// When the next request may go out, enforcing [`MIN_REQUEST_GAP`].
    next_allowed: Instant,
    /// Set when Spotify has told us to back off, and honoured until it passes.
    throttled_until: Option<Instant>,
    /// How many times in a row Spotify has refused for rate limiting.
    ///
    /// Retry-After says how long this request should wait. It does not say that
    /// waiting exactly that long again, over and over, is a good idea: a client
    /// that keeps returning at the earliest permitted moment is the one that
    /// stays throttled. Each consecutive refusal doubles the floor.
    throttle_strikes: u32,
    premium: Option<bool>,
    /// Context names by uri, so playing a whole playlist asks once rather than
    /// once per track.
    context_names: std::collections::HashMap<String, String>,
    /// Set when the stored token predates a scope this build needs.
    ///
    /// Adding a feature adds a scope, and a refresh token issued before that
    /// cannot grant it. The call fails with a 403 that says so, and the only
    /// fix is logging in again, so it is worth saying once and clearly rather
    /// than failing quietly every time the picker opens.
    scope_missing: bool,
    /// Set when Spotify refuses a library call outright.
    ///
    /// It means this token cannot read or change saved tracks at all, usually
    /// because the application is still in development mode and the account is
    /// not on its allowlist. Recorded so the like button can disappear rather
    /// than sit there failing on every click.
    library_forbidden: bool,
}

impl Client {
    pub fn new(client_id: String) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
            client_id,
            tokens: None,
            next_allowed: Instant::now(),
            throttled_until: None,
            throttle_strikes: 0,
            premium: None,
            context_names: std::collections::HashMap::new(),
            scope_missing: false,
            library_forbidden: false,
        })
    }

    pub fn is_authorized(&self) -> bool {
        self.tokens.is_some()
    }

    pub fn premium(&self) -> Option<bool> {
        self.premium
    }

    /// Whether saved-track calls are worth making at all.
    pub fn library_available(&self) -> bool {
        !self.library_forbidden
    }

    /// Adopt a stored refresh token, exchanging it for a usable access token.
    pub async fn restore(&mut self, refresh_token: &str) -> Result<()> {
        self.tokens = Some(auth::refresh(&self.client_id, refresh_token).await?);
        Ok(())
    }

    /// A valid access token, refreshing if the current one is close to expiry.
    async fn access_token(&mut self) -> Result<String> {
        let tokens = self.tokens.as_ref().ok_or_else(|| anyhow!("no Spotify account connected"))?;
        if tokens.is_fresh() {
            return Ok(tokens.access.clone());
        }

        let refreshed = auth::refresh(&self.client_id, &tokens.refresh).await?;
        // Spotify sometimes rotates the refresh token, and losing the new one
        // means the next start has to log in again.
        if refreshed.refresh != tokens.refresh {
            let _ = auth::save_refresh_token(&refreshed.refresh);
        }
        let access = refreshed.access.clone();
        self.tokens = Some(refreshed);
        Ok(access)
    }

    /// Wait out the rate limiter, then issue a request.
    async fn request(
        &mut self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<reqwest::Response> {
        if let Some(until) = self.throttled_until {
            if Instant::now() < until {
                bail!("rate limited by Spotify for another {:?}", until - Instant::now());
            }
            self.throttled_until = None;
        }
        // A request that got through means whatever the limit was is over.
        if self.throttled_until.is_none() {
            self.throttle_strikes = 0;
        }

        let now = Instant::now();
        if now < self.next_allowed {
            tokio::time::sleep(self.next_allowed - now).await;
        }
        self.next_allowed = Instant::now() + MIN_REQUEST_GAP;

        let token = self.access_token().await?;
        let mut request = self.http.request(method, format!("{API}{path}")).bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }

        let response = request.send().await.with_context(|| format!("calling {path}"))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            self.throttle_strikes = self.throttle_strikes.saturating_add(1);
            let wait = backoff(retry_after, self.throttle_strikes);
            self.throttled_until = Some(Instant::now() + wait);
            bail!("rate limited by Spotify, backing off for {}s", wait.as_secs());
        }

        Ok(response)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&mut self, path: &str) -> Result<T> {
        let response = self.request(reqwest::Method::GET, path, None).await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Spotify returned {status} for {path}: {body}");
        }
        serde_json::from_str(&body).with_context(|| format!("unexpected response from {path}"))
    }

    /// Issue a write to `/me/player/*`, mapping the Premium refusal to something
    /// a user can act on.
    async fn player_write(
        &mut self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<()> {
        let response = self.request(method, path, body).await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            self.premium = Some(false);
            bail!("this needs Spotify Premium");
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            bail!("no active Spotify device to control");
        }
        let body = response.text().await.unwrap_or_default();
        bail!("Spotify returned {status}: {body}")
    }

    /// Whether the account can use playback controls.
    ///
    /// Cached, because it does not change within a session and asking repeatedly
    /// would spend rate limit on it.
    ///
    /// `None` means Spotify did not say, which is not the same as free. It
    /// happens when the token lacks `user-read-private`, and treating it as free
    /// would hide controls that work. Left unknown, the first write decides.
    pub async fn check_premium(&mut self) -> Result<Option<bool>> {
        if self.premium.is_some() {
            return Ok(self.premium);
        }
        let me: Me = self.get_json("/me").await?;
        tracing::debug!(product = ?me.product, "account product");

        let Some(product) = me.product.as_deref() else {
            tracing::warn!(
                "Spotify did not report the subscription level, so playback \
                 controls stay available until one is refused. Logging in again \
                 picks up the scope that reports it."
            );
            return Ok(None);
        };
        self.premium = Some(product == "premium");
        Ok(self.premium)
    }

    /// Whether a track is in the user's library.
    /// Whether an item is in the user's library.
    ///
    /// `/me/library/contains` rather than `/me/tracks/contains`: it takes full
    /// URIs and covers episodes as well as tracks, so podcasts need no second
    /// code path.
    pub async fn is_saved(&mut self, uri: &str) -> Result<bool> {
        let path = format!("/me/library/contains?uris={}", urlencode(uri));
        let response = self.request(reqwest::Method::GET, &path, None).await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::FORBIDDEN {
            self.library_forbidden = true;
            bail!(
                "Spotify refused access to your library. If the application is in \
                 development mode, add this account under User Management in the \
                 dashboard."
            );
        }
        if !status.is_success() {
            bail!("Spotify returned {status}: {body}");
        }

        let saved: Vec<bool> = serde_json::from_str(&body)
            .with_context(|| format!("unexpected library response: {body}"))?;
        Ok(saved.first().copied().unwrap_or(false))
    }

    pub async fn set_saved(&mut self, uri: &str, saved: bool) -> Result<()> {
        let method = if saved { reqwest::Method::PUT } else { reqwest::Method::DELETE };
        let path = format!("/me/library?uris={}", urlencode(uri));
        let response = self.request(method, &path, None).await?;
        let status = response.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            self.library_forbidden = true;
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("Spotify returned {status}: {body}");
        }
        Ok(())
    }

    pub async fn devices(&mut self) -> Result<Vec<Device>> {
        let devices: Devices = self.get_json("/me/player/devices").await?;
        Ok(devices.devices)
    }

    pub async fn transfer_to(&mut self, device_id: &str) -> Result<()> {
        // play: true keeps whatever was happening happening. Without it, moving
        // to another device pauses, which is never what the click meant.
        let body = serde_json::json!({ "device_ids": [device_id], "play": true });
        self.player_write(reqwest::Method::PUT, "/me/player", Some(body)).await
    }

    /// The user's own playlists, most recent first, as Spotify orders them.
    ///
    /// One page. Somebody with four hundred playlists is not going to find the
    /// one they want by scrolling a popup, and paging through all of them would
    /// spend a request per fifty for a list nobody reads to the end.
    pub async fn playlists(&mut self) -> Result<Vec<waytify_ipc::Playlist>> {
        let response = self.request(reqwest::Method::GET, "/me/playlists?limit=50", None).await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::FORBIDDEN {
            self.scope_missing = true;
            bail!(
                "your Spotify login predates this feature. Run `waytify login` \
                 again to grant access to your playlists."
            );
        }
        if !status.is_success() {
            bail!("Spotify returned {status} for playlists: {body}");
        }

        let playlists: Playlists =
            serde_json::from_str(&body).context("unexpected response from /me/playlists")?;
        Ok(playlists
            .items
            .into_iter()
            // A playlist with no uri cannot be played, so it is not offered.
            .filter_map(|p| {
                Some(waytify_ipc::Playlist {
                    name: p.name,
                    uri: p.uri?,
                    tracks: p.tracks.map(|t| t.total),
                })
            })
            .collect())
    }

    /// Whether the stored token is too old for what this build asks of it.
    pub fn needs_reauthorization(&self) -> bool {
        self.scope_missing
    }

    /// Start playing a whole context: a playlist, an album, an artist.
    pub async fn play_context(&mut self, context_uri: &str) -> Result<()> {
        let body = serde_json::json!({ "context_uri": context_uri });
        self.player_write(reqwest::Method::PUT, "/me/player/play", Some(body)).await
    }

    /// Start playing one item from inside a playlist or album.
    ///
    /// This is not "jump to a position in the queue", which Spotify does not
    /// offer. It restarts the context at the chosen item, which is the same
    /// thing from the outside as long as the item is part of that context. An
    /// item added to the queue by hand is not, and Spotify answers with an
    /// error rather than playing the wrong thing.
    pub async fn play_at(&mut self, context_uri: &str, item_uri: &str) -> Result<()> {
        let body = serde_json::json!({
            "context_uri": context_uri,
            "offset": { "uri": item_uri },
        });
        self.player_write(reqwest::Method::PUT, "/me/player/play", Some(body)).await
    }

    pub async fn set_remote_volume(&mut self, percent: u8) -> Result<()> {
        let path = format!("/me/player/volume?volume_percent={}", percent.min(100));
        self.player_write(reqwest::Method::PUT, &path, None).await
    }

    /// What the current track is being played out of.
    ///
    /// Two requests the first time and one after that: `/me/player` says which
    /// playlist or album it is, by uri, and nothing else. The name has to be
    /// fetched separately and is then remembered, because it does not change and
    /// every track in an album would otherwise ask again.
    pub async fn context(&mut self) -> Result<Option<waytify_ipc::PlayContext>> {
        let response = self.request(reqwest::Method::GET, "/me/player", None).await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        // 204 with an empty body means nothing is playing, same as the queue.
        if status == reqwest::StatusCode::NO_CONTENT || body.trim().is_empty() {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("Spotify returned {status} for playback: {body}");
        }

        let playback: Playback =
            serde_json::from_str(&body).context("unexpected response from /me/player")?;
        // Playing a single track from a search has no context at all, which is
        // an answer rather than a failure.
        let Some(context) = playback.context else { return Ok(None) };
        let url = context.external_urls.spotify;

        let uri = context.uri.unwrap_or_default();
        let identity = (!uri.is_empty()).then(|| uri.clone());
        if let Some(name) = self.context_names.get(&uri) {
            return Ok(Some(waytify_ipc::PlayContext {
                kind: context.kind,
                name: name.clone(),
                uri: identity,
                url,
            }));
        }

        let Some(href) = context.href else { return Ok(None) };
        // Ask for the name alone where the endpoint allows it. A playlist object
        // is otherwise its entire track list.
        let path = href.strip_prefix(API).unwrap_or(&href).to_string();
        let path = match context.kind {
            waytify_ipc::ContextKind::Playlist => format!("{path}?fields=name"),
            _ => path,
        };
        let named: Named = self.get_json(&path).await?;

        if !uri.is_empty() {
            self.context_names.insert(uri, named.name.clone());
        }
        Ok(Some(waytify_ipc::PlayContext {
            kind: context.kind,
            name: named.name,
            uri: identity,
            url,
        }))
    }

    /// What is coming up, as far as Spotify will say.
    pub async fn queue(&mut self) -> Result<Vec<waytify_ipc::Track>> {
        let response = self.request(reqwest::Method::GET, "/me/player/queue", None).await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if says_no_queue(status, &body) {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            bail!("Spotify returned {status} for the queue: {body}");
        }
        let queue: Queue =
            serde_json::from_str(&body).context("unexpected response from /me/player/queue")?;

        Ok(queue
            .queue
            .into_iter()
            .map(|item| {
                let podcast = item.kind.as_deref() == Some("episode");
                let mut artists: Vec<String> = item.artists.into_iter().map(|a| a.name).collect();
                // An episode has no artists. The show it belongs to is the
                // equivalent line, and leaving it blank loses the only context
                // an episode title has.
                if let Some(show) = item.show {
                    artists.push(show.name);
                }
                waytify_ipc::Track {
                    // The uri goes in the id, which is where everything else
                    // already looks for a Spotify identity.
                    id: item.uri,
                    title: item.name,
                    artists,
                    length_ms: item.duration_ms,
                    kind: if podcast {
                        waytify_ipc::MediaKind::Podcast
                    } else {
                        waytify_ipc::MediaKind::Music
                    },
                    ..Default::default()
                }
            })
            .collect())
    }
}

/// How long to wait after being refused for rate limiting.
///
/// Never less than the `Retry-After` Spotify sent, which is the part that is not
/// a guess. On top of that each consecutive refusal doubles a floor, because a
/// client that comes back at the earliest permitted moment every time is the one
/// that stays throttled. Capped so a long quiet period is not punished forever.
fn backoff(retry_after: Option<u64>, strikes: u32) -> Duration {
    const CAP: u64 = 300;
    let floor = 5u64.saturating_mul(1 << strikes.min(6).saturating_sub(1));
    Duration::from_secs(retry_after.unwrap_or(5).max(floor).min(CAP))
}

/// Percent-encode a Spotify URI for a query string.
///
/// The colons in `spotify:track:{id}` are safe unencoded but not every proxy
/// agrees, and the id itself is base62 so nothing else needs escaping.
fn urlencode(uri: &str) -> String {
    uri.replace(':', "%3A")
}

/// Whether a queue response means "there is nothing queued" rather than carrying
/// a queue or reporting a failure.
///
/// Spotify answers 204 with an empty body when there is no playback to have a
/// queue for. That is an answer, not an error: treating it as one would leave
/// the previous queue on screen after playback stopped.
fn says_no_queue(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::NO_CONTENT || body.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_starts_unauthorized() {
        let client = Client::new("cid".into()).unwrap();
        assert!(!client.is_authorized());
        assert_eq!(client.premium(), None, "premium is unknown until asked");
    }

    #[test]
    fn devices_parse_from_the_documented_shape() {
        let json = r#"{"devices":[
            {"id":"abc","name":"Phone","type":"Smartphone","is_active":true,
             "supports_volume":true,"volume_percent":40},
            {"id":null,"name":"Restricted","type":"Speaker","is_active":false,
             "supports_volume":false,"volume_percent":null}
        ]}"#;
        let parsed: Devices = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.devices.len(), 2);
        assert_eq!(parsed.devices[0].kind, "Smartphone");
        // Spotify really does return devices with no id, which cannot be
        // transferred to and must not be offered as if they could.
        assert!(parsed.devices[1].id.is_none());
    }

    #[test]
    fn backing_off_never_undercuts_what_spotify_asked_for() {
        // Retry-After is the part that is not a guess, so it is a floor.
        assert_eq!(backoff(Some(30), 1), Duration::from_secs(30));
        assert_eq!(backoff(Some(2), 1), Duration::from_secs(5), "and 5s is our own floor");

        // Each consecutive refusal doubles that floor. Coming back at the
        // earliest permitted moment every time is what keeps a client throttled.
        assert!(backoff(None, 3) > backoff(None, 2));
        assert!(backoff(None, 4) > backoff(None, 3));

        // The doubling stops, so a bad afternoon does not turn into an hour of
        // silence.
        assert_eq!(backoff(None, 30), backoff(None, 6));
        assert!(backoff(None, 30) <= Duration::from_secs(300));

        // And an implausible Retry-After is bounded rather than trusted. A day
        // of waiting is a bug at the other end, not an instruction.
        assert_eq!(backoff(Some(86_400), 1), Duration::from_secs(300));
    }

    #[test]
    fn a_library_uri_survives_a_query_string() {
        assert_eq!(
            urlencode("spotify:track:4uLU6hMCjMI75M1A2tKUQC"),
            "spotify%3Atrack%3A4uLU6hMCjMI75M1A2tKUQC"
        );
        assert_eq!(
            urlencode("spotify:episode:5vHwCgvNqDDPLTAfsvOTGw"),
            "spotify%3Aepisode%3A5vHwCgvNqDDPLTAfsvOTGw"
        );
    }

    #[test]
    fn a_playing_context_parses_and_an_absent_one_is_not_an_error() {
        use waytify_ipc::ContextKind;

        let json = r#"{"context":{"type":"playlist",
            "href":"https://api.spotify.com/v1/playlists/37i9dQZF1DXcBWIGoYBM5M",
            "uri":"spotify:playlist:37i9dQZF1DXcBWIGoYBM5M",
            "external_urls":{"spotify":"https://open.spotify.com/playlist/37i9"}}}"#;
        let playback: Playback = serde_json::from_str(json).unwrap();
        let context = playback.context.unwrap();
        assert_eq!(context.kind, ContextKind::Playlist);
        assert_eq!(
            context.external_urls.spotify.as_deref(),
            Some("https://open.spotify.com/playlist/37i9")
        );

        // Playing a single track out of a search has no context, and Spotify
        // says so with a null rather than by omitting the field.
        let none: Playback = serde_json::from_str(r#"{"context":null}"#).unwrap();
        assert!(none.context.is_none());
        let missing: Playback = serde_json::from_str("{}").unwrap();
        assert!(missing.context.is_none());

        // A kind added after this was written still counts as playing from
        // something, rather than failing the whole response.
        let odd = r#"{"context":{"type":"audiobook","href":null,"uri":null}}"#;
        let odd: Playback = serde_json::from_str(odd).unwrap();
        assert_eq!(odd.context.unwrap().kind, ContextKind::Other);

        // Every kind has something to introduce it with.
        for kind in [
            ContextKind::Playlist,
            ContextKind::Album,
            ContextKind::Artist,
            ContextKind::Show,
            ContextKind::Collection,
            ContextKind::Other,
        ] {
            assert!(!kind.label().is_empty());
        }
    }

    #[test]
    fn nothing_playing_reads_as_an_empty_queue() {
        use reqwest::StatusCode;
        assert!(says_no_queue(StatusCode::NO_CONTENT, ""));
        // Seen in the wild: a 200 with nothing in it.
        assert!(says_no_queue(StatusCode::OK, "  \n "));
        assert!(!says_no_queue(StatusCode::OK, r#"{"queue":[]}"#), "an empty list is a queue");
        // A real failure must still surface rather than being read as "nothing
        // queued", which would hide the problem behind a plausible answer.
        assert!(!says_no_queue(StatusCode::UNAUTHORIZED, r#"{"error":"expired"}"#));
    }

    #[test]
    fn queue_entries_carry_what_it_takes_to_play_them() {
        let json = r#"{"queue":[
            {"type":"track","name":"A song","uri":"spotify:track:4uLU6hMCjMI75M1A2tKUQC",
             "artists":[{"name":"Someone"}],"duration_ms":1000},
            {"type":"episode","name":"An episode","uri":"spotify:episode:5vHwCgvNqDDPLTAfsvOTGw",
             "show":{"name":"A show"},"duration_ms":2000}
        ]}"#;
        let parsed: Queue = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.queue[0].uri.as_deref(), Some("spotify:track:4uLU6hMCjMI75M1A2tKUQC"));
        assert_eq!(parsed.queue[1].kind.as_deref(), Some("episode"));
        // An episode has a show where a track has artists. Without it the row
        // would show a title and nothing else.
        assert_eq!(parsed.queue[1].show.as_ref().map(|s| s.name.as_str()), Some("A show"));
    }

    #[test]
    fn the_queue_survives_tracks_with_no_artists() {
        let json = r#"{"queue":[{"name":"Solo","artists":[],"duration_ms":1000}]}"#;
        let parsed: Queue = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.queue[0].name, "Solo");
        assert!(parsed.queue[0].artists.is_empty());
    }

    #[test]
    fn a_free_account_is_recognised() {
        let free: Me = serde_json::from_str(r#"{"product":"free"}"#).unwrap();
        assert_ne!(free.product.as_deref(), Some("premium"));
        let premium: Me = serde_json::from_str(r#"{"product":"premium"}"#).unwrap();
        assert_eq!(premium.product.as_deref(), Some("premium"));
        // Absent rather than free: an account whose product Spotify will not say
        // must not be assumed to have Premium.
        let unknown: Me = serde_json::from_str("{}").unwrap();
        assert_ne!(unknown.product.as_deref(), Some("premium"));
    }
}
