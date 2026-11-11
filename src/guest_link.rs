//! Token-gated link-only Guest joining.
//!
//! The original design made ANY unauthenticated visitor on `web_server.bind_address` into
//! a Guest automatically (`UserAuth::None` falling back to a global `default_user`). That's
//! fine on a trusted LAN, but the moment this port is reachable from the internet — exactly
//! what a remote collaborator needs — it's a fully open door: no password, no token,
//! nothing. This module replaces that blanket bypass with a single-use handshake: the Guest
//! visits `GET /g?token=...` (the only request that needs to succeed with no credentials at
//! all), the token is checked against a random secret generated once per process, and only
//! then is a normal session cookie issued for the passwordless "Gość" user — same as any
//! other login from that point on. `UserAuth::None` itself stays rejected everywhere else
//! (see `App::clear_guest_default_user_if_stale`).
//!
//! HostApp fetches the current token through `control_gate`'s existing localhost-only
//! internal API (never the `0.0.0.0` address Guests connect to) and embeds it in the link
//! it hands out, so a Guest never sees or needs to know the token exists — they just click
//! the link.

use actix_web::{
    HttpResponse, Responder, get,
    http::header,
    web::{Data, Query},
};
use serde::Deserialize;
use std::sync::OnceLock;
use tracing::{info, warn};

use crate::{
    api::auth::build_cookie,
    app::{App, user::AuthenticatedUser},
};

static GUEST_TOKEN: OnceLock<String> = OnceLock::new();

/// The current process' Guest-join secret. Lazily generated on first use and stable for the
/// lifetime of the process; regenerated (different value) every time web-server restarts,
/// so a link handed out before a restart stops working — HostApp always fetches a fresh one
/// when a new session starts.
pub fn token() -> &'static str {
    GUEST_TOKEN.get_or_init(|| {
        use moonlight_common::{crypto::rustcrypto::RustCryptoBackend, http::pair::PairingCryptoBackend};

        let mut random = [0u8; 24];
        match RustCryptoBackend.random_bytes(&mut random) {
            Ok(()) => hex::encode(random),
            Err(err) => {
                // Should never happen on a real OS RNG; better a usable-but-weaker token
                // than a hard crash of the whole session.
                warn!(error = %err, "failed to generate secure randomness for the guest-join token; using a weaker fallback");
                use std::time::{SystemTime, UNIX_EPOCH};
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                format!("{nanos:x}")
            }
        }
    })
}

/// Constant-time-ish comparison — not strictly necessary for a hobby LAN/friend-to-friend
/// tool, but cheap to get right and avoids leaking the token one byte at a time via timing.
fn token_matches(candidate: &str) -> bool {
    let expected = token().as_bytes();
    let candidate = candidate.as_bytes();

    if expected.len() != candidate.len() {
        return false;
    }

    expected
        .iter()
        .zip(candidate.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[derive(Deserialize)]
struct GuestJoinQuery {
    token: String,
    /// Stream-quality settings HostApp lets the Host pick (bitrate/fps/resolution/codec) —
    /// see `web/stream.ts`'s `parseSettingsFromQuery`. Passed straight through as-is onto
    /// the `/stream.html` redirect query string; the Guest never has to (and in this app's
    /// flow, never gets the chance to) set these themselves.
    #[serde(flatten)]
    stream_settings: StreamSettingsQuery,
}

#[derive(Deserialize, Default)]
struct StreamSettingsQuery {
    bitrate: Option<String>,
    fps: Option<String>,
    #[serde(rename = "videoSize")]
    video_size: Option<String>,
    #[serde(rename = "videoCodec")]
    video_codec: Option<String>,
}

impl StreamSettingsQuery {
    /// Renders as `&key=value` pairs for appending to a redirect URL's query string, URL-
    /// encoding each value (host/app titles and the like could in principle contain
    /// characters that need escaping, even though these particular values come from
    /// HostApp's own fixed option lists today).
    fn to_query_pairs(&self) -> String {
        [
            ("bitrate", &self.bitrate),
            ("fps", &self.fps),
            ("videoSize", &self.video_size),
            ("videoCodec", &self.video_codec),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.as_ref().map(|value| format!("{key}={}", url_encode(value))))
        .collect::<Vec<_>>()
        .join("&")
    }
}

/// Minimal percent-encoding for a query value — these are short, known-alphabet tokens
/// (numbers, "1080p", "h264", ...) so a full encoding crate would be overkill.
fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `GET /g?token=...` — the one unauthenticated endpoint a Guest actually visits. Validates
/// the token, provisions/reuses the passwordless "Gość" user, issues it a real session
/// cookie, and redirects straight into the stream of whichever host/app this Guest can see
/// (dawswithfriends only ever has one Sunshine host paired, so "the first one" is fine).
#[get("/g")]
pub async fn guest_join(app: Data<App>, Query(query): Query<GuestJoinQuery>) -> impl Responder {
    if !token_matches(&query.token) {
        warn!("rejected guest join attempt: invalid or missing token");
        return HttpResponse::Forbidden()
            .body("Nieprawidłowy lub wygasły token w linku dołączenia do sesji.");
    }

    let mut guest = match app.ensure_guest_user().await {
        Ok(guest) => guest,
        Err(err) => {
            warn!(error = %err, "failed to provision guest user for token-gated join");
            return HttpResponse::InternalServerError().finish();
        }
    };

    let session_expiration = app.config().web_server.session_cookie_expiration;
    let session = match guest.new_session(session_expiration).await {
        Ok(session) => session,
        Err(err) => {
            warn!(error = %err, "failed to create guest session");
            return HttpResponse::InternalServerError().finish();
        }
    };
    let mut session_bytes = [0; _];
    let session_str = session.encode(&mut session_bytes);

    let destination = resolve_stream_destination(&app, &mut guest)
        .await
        .unwrap_or_else(|| format!("{}/", app.config().web_server.url_path_prefix));
    let settings_pairs = query.stream_settings.to_query_pairs();
    let destination = if settings_pairs.is_empty() {
        destination
    } else {
        let separator = if destination.contains('?') { "&" } else { "?" };
        format!("{destination}{separator}{settings_pairs}")
    };

    info!("guest joined session via token-gated link");

    HttpResponse::Found()
        .cookie(build_cookie(&app, session_expiration, session_str))
        .append_header((header::LOCATION, destination))
        .finish()
}

/// Best-effort: picks the Guest's first visible host and its "Desktop" app (falling back to
/// whatever app is first) so the link drops straight into the stream instead of a host list.
/// Returns `None` on any failure (no host paired yet, no apps listed) — the caller falls
/// back to the plain root page, where the Guest is now a normal logged-in user and can pick
/// manually.
async fn resolve_stream_destination(app: &App, guest: &mut AuthenticatedUser) -> Option<String> {
    let mut host = guest.hosts().await.ok()?.into_iter().next()?;
    let host_id = host.id();

    let apps = host.list_apps(guest).await.ok()?;
    let chosen = apps
        .iter()
        .find(|candidate| candidate.title == "Desktop")
        .or_else(|| apps.first())?;

    Some(format!(
        "{}/stream.html?hostId={}&appId={}",
        app.config().web_server.url_path_prefix,
        host_id.0,
        chosen.id.0
    ))
}
