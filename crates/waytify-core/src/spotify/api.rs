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
struct Queue {
    queue: Vec<Item>,
}

#[derive(Debug, Deserialize)]
struct Item {
    name: String,
    #[serde(default)]
    artists: Vec<Artist>,
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
    premium: Option<bool>,
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
            premium: None,
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
            // Honouring Retry-After rather than retrying blind is the difference
            // between backing off and making it worse.
            let wait = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5);
            self.throttled_until = Some(Instant::now() + Duration::from_secs(wait));
            bail!("rate limited by Spotify, backing off for {wait}s");
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
    pub async fn is_saved(&mut self, track_id: &str) -> Result<bool> {
        let response = self
            .request(reqwest::Method::GET, &format!("/me/tracks/contains?ids={track_id}"), None)
            .await?;
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
            .with_context(|| format!("unexpected saved-track response: {body}"))?;
        Ok(saved.first().copied().unwrap_or(false))
    }

    pub async fn set_saved(&mut self, track_id: &str, saved: bool) -> Result<()> {
        let method = if saved { reqwest::Method::PUT } else { reqwest::Method::DELETE };
        let response = self.request(method, &format!("/me/tracks?ids={track_id}"), None).await?;
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

    pub async fn set_remote_volume(&mut self, percent: u8) -> Result<()> {
        let path = format!("/me/player/volume?volume_percent={}", percent.min(100));
        self.player_write(reqwest::Method::PUT, &path, None).await
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
            .map(|item| waytify_ipc::Track {
                title: item.name,
                artists: item.artists.into_iter().map(|a| a.name).collect(),
                length_ms: item.duration_ms,
                ..Default::default()
            })
            .collect())
    }
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
