//! Spotify authorization, using Authorization Code with PKCE.
//!
//! PKCE rather than the plain authorization code flow because waytify ships as a
//! binary anyone can read, and an embedded client secret would not be a secret.
//! PKCE is designed for exactly that situation and needs no secret at all.
//!
//! The refresh token goes to the system keyring rather than a dotfile. It is a
//! long lived credential to someone's music account, and a file in `~/.config`
//! ends up in dotfile repositories.
//!
//! The client id is not a secret and lives in the config file, but it is still
//! per user: rate limits are counted per application, so everyone sharing one id
//! would share one budget. The README asks people to register their own.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

/// Keyring entry the refresh token is stored under.
const KEYRING_SERVICE: &str = "waytify";
const KEYRING_USER: &str = "spotify-refresh-token";

/// Everything waytify asks for, and nothing more.
///
/// Each one maps to a feature: the playback scopes for devices and transfer, the
/// library scopes for the like button. Asking for more than is used makes the
/// consent screen alarming for no benefit.
pub const SCOPES: &[&str] = &[
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-library-read",
    "user-library-modify",
];

/// How long to wait for the person to finish authorizing in their browser.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);

/// Renew this long before expiry, so a request never races the clock.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub expires_at: Instant,
}

impl Tokens {
    pub fn is_fresh(&self) -> bool {
        Instant::now() + REFRESH_MARGIN < self.expires_at
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Absent when refreshing: Spotify may keep the existing refresh token.
    refresh_token: Option<String>,
    expires_in: u64,
}

/// Store a refresh token in the keyring.
pub fn save_refresh_token(token: &str) -> Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .context("opening the keyring")?
        .set_password(token)
        .context("writing to the keyring")
}

/// Read the stored refresh token, if there is one.
///
/// A missing entry means "not logged in", which is an ordinary state rather than
/// an error, so it comes back as `None`.
pub fn load_refresh_token() -> Result<Option<String>> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).context("opening the keyring")?;
    match entry.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("reading from the keyring"),
    }
}

/// Forget the stored token. Used by `waytify logout`.
pub fn forget_refresh_token() -> Result<()> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).context("opening the keyring")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("deleting from the keyring"),
    }
}

/// Exchange a stored refresh token for a usable access token.
pub async fn refresh(client_id: &str, refresh_token: &str) -> Result<Tokens> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    let response: TokenResponse =
        post_form(&params).await.context("refreshing the access token")?;

    Ok(Tokens {
        access: response.access_token,
        // Spotify only sometimes rotates the refresh token. Keeping the old one
        // when none comes back is what the spec expects.
        refresh: response.refresh_token.unwrap_or_else(|| refresh_token.to_string()),
        expires_at: Instant::now() + Duration::from_secs(response.expires_in),
    })
}

/// Run the full login: open a browser, catch the redirect, exchange the code.
///
/// Blocking on purpose. This is a one-off interactive command, and the loopback
/// listener wants a plain socket rather than a runtime.
pub async fn login(client_id: &str) -> Result<Tokens> {
    let verifier = code_verifier();
    let challenge = code_challenge(&verifier);

    // Port zero so the OS picks a free one. A fixed port would collide with
    // anything else already listening and fail for a reason nobody could guess.
    let listener =
        TcpListener::bind("127.0.0.1:0").context("opening a local port for the redirect")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let state = random_string(16);
    let url = authorize_url(client_id, &redirect_uri, &challenge, &state);

    println!("Opening your browser to authorize waytify.");
    println!("If nothing opens, visit this URL:\n\n{url}\n");
    let _ = open_in_browser(&url);

    let code = wait_for_code(listener, &state)?;
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", client_id),
        ("code_verifier", verifier.as_str()),
    ];
    let response: TokenResponse =
        post_form(&params).await.context("exchanging the authorization code")?;

    let refresh =
        response.refresh_token.ok_or_else(|| anyhow!("Spotify did not return a refresh token"))?;

    Ok(Tokens {
        access: response.access_token,
        refresh,
        expires_at: Instant::now() + Duration::from_secs(response.expires_in),
    })
}

pub fn authorize_url(client_id: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let scope = SCOPES.join(" ");
    format!(
        "{AUTHORIZE_URL}?client_id={}&response_type=code&redirect_uri={}\
         &code_challenge_method=S256&code_challenge={}&state={}&scope={}",
        urlencode(client_id),
        urlencode(redirect_uri),
        urlencode(challenge),
        urlencode(state),
        urlencode(&scope),
    )
}

async fn post_form(params: &[(&str, &str)]) -> Result<TokenResponse> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let response = client.post(TOKEN_URL).form(params).send().await?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // Spotify's errors are specific and worth passing through rather than
        // flattening into "login failed".
        bail!("Spotify returned {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("unexpected token response: {body}"))
}

/// Serve exactly one request, pull the code out of it, and answer the browser.
fn wait_for_code(listener: TcpListener, expected_state: &str) -> Result<String> {
    // Non-blocking so the wait can be bounded. A blocking accept would hang
    // forever if the person closed the tab without deciding, and this runs in the
    // foreground of a command they are watching.
    listener.set_nonblocking(true).context("configuring the redirect listener")?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;

    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    bail!("timed out waiting for authorization");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e).context("accepting the redirect"),
        }
    };
    stream.set_nonblocking(false).ok();

    // Only the request line is needed, and it holds the whole query.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let target = request_line.split_whitespace().nth(1).unwrap_or_default().to_string();
    let outcome = parse_callback(&target, expected_state);

    let body = match &outcome {
        Ok(_) => "waytify is authorized. You can close this tab.",
        Err(_) => "Authorization failed. Check the terminal running waytify.",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();

    outcome
}

/// Pull the authorization code out of the redirect target.
pub fn parse_callback(target: &str, expected_state: &str) -> Result<String> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let mut code = None;
    let mut state = None;
    let mut error = None;

    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        match key {
            "code" => code = Some(urldecode(value)),
            "state" => state = Some(urldecode(value)),
            "error" => error = Some(urldecode(value)),
            _ => {}
        }
    }

    if let Some(error) = error {
        bail!("Spotify refused the request: {error}");
    }
    // The state check is what stops another page on the machine from feeding a
    // code of its own into the loopback listener.
    if state.as_deref() != Some(expected_state) {
        bail!("the redirect did not carry the expected state");
    }
    code.ok_or_else(|| anyhow!("the redirect carried no authorization code"))
}

fn open_in_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("running xdg-open")?;
    Ok(())
}

/// A high entropy string in the character set PKCE allows.
pub fn code_verifier() -> String {
    random_string(64)
}

pub fn code_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn random_string(len: usize) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    (0..len).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect()
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifiers_are_long_enough_and_never_repeat() {
        let a = code_verifier();
        let b = code_verifier();
        // PKCE requires between 43 and 128 characters.
        assert!((43..=128).contains(&a.len()), "length was {}", a.len());
        assert_ne!(a, b, "two verifiers should not be identical");
    }

    #[test]
    fn verifiers_use_only_permitted_characters() {
        let verifier = code_verifier();
        assert!(
            verifier.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)),
            "{verifier} contains a character PKCE does not allow"
        );
    }

    #[test]
    fn the_challenge_is_the_documented_transform() {
        // The example pair from RFC 7636, which is the definition of correct here.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(code_challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn the_challenge_is_url_safe_and_unpadded() {
        let challenge = code_challenge(&code_verifier());
        assert!(!challenge.contains('='), "padding would break the query string");
        assert!(
            !challenge.contains('+') && !challenge.contains('/'),
            "{challenge} is not url safe"
        );
    }

    #[test]
    fn a_callback_yields_its_code() {
        let target = "/callback?code=AQD123&state=abc";
        assert_eq!(parse_callback(target, "abc").unwrap(), "AQD123");
    }

    #[test]
    fn a_mismatched_state_is_refused() {
        // Without this check, any page on the machine could post a code of its
        // own to the loopback listener and have it accepted.
        let target = "/callback?code=AQD123&state=somebody-elses";
        assert!(parse_callback(target, "abc").is_err());
    }

    #[test]
    fn a_refusal_is_reported_rather_than_read_as_success() {
        let target = "/callback?error=access_denied&state=abc";
        let err = parse_callback(target, "abc").unwrap_err().to_string();
        assert!(err.contains("access_denied"), "{err}");
    }

    #[test]
    fn a_callback_with_no_code_is_an_error() {
        assert!(parse_callback("/callback?state=abc", "abc").is_err());
        assert!(parse_callback("/callback", "abc").is_err());
    }

    #[test]
    fn percent_encoded_values_survive_the_round_trip() {
        let awkward = "a b+c/d=e&f";
        assert_eq!(urldecode(&urlencode(awkward)), awkward);
    }

    #[test]
    fn the_authorize_url_carries_everything_spotify_needs() {
        let url = authorize_url("cid", "http://127.0.0.1:1234/callback", "chal", "st");
        for required in [
            "client_id=cid",
            "response_type=code",
            "code_challenge_method=S256",
            "code_challenge=chal",
            "state=st",
        ] {
            assert!(url.contains(required), "{url} is missing {required}");
        }
        // The redirect must be encoded or the query ends at the first slash.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1234%2Fcallback"), "{url}");
        assert!(url.contains("user-library-modify"), "scopes should be present");
    }

    #[test]
    fn tokens_are_renewed_before_they_expire() {
        let nearly = Tokens {
            access: "a".into(),
            refresh: "r".into(),
            expires_at: Instant::now() + Duration::from_secs(30),
        };
        assert!(!nearly.is_fresh(), "a token expiring inside the margin is not fresh");

        let plenty = Tokens {
            access: "a".into(),
            refresh: "r".into(),
            expires_at: Instant::now() + Duration::from_secs(3600),
        };
        assert!(plenty.is_fresh());
    }
}
