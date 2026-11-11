//! Gates whether a Guest's mouse/keyboard/gamepad input is forwarded to the Host.
//!
//! Sunshine assumes the Host machine is otherwise idle (streaming to a console/TV); Ableton
//! Together's Host is actively sitting at their own computer running their DAW. Two modes,
//! switched by HostApp's "Collaboration mode" button/hotkey — never anything in between:
//!
//! - **Host-only (default)**: only the Host ever has control. The Guest is a pure
//!   viewer+mic; their input is always dropped, full stop — there is no way for them to
//!   claim it, not even by acting.
//! - **Collaboration mode**: control alternates automatically with whoever is actually
//!   acting. The Guest's own input claims it the instant they move/type
//!   (`claim_guest_control`, called from the Guest-input gating path in
//!   `webrtc::stream::webrtc_loop`); the Host's own physical input instantly reclaims it the
//!   same way, client-side, via HostApp's SoftTakeoverMonitor.
//!
//! The mode toggle lives on a tiny HTTP API bound to localhost only — not exposed on the
//! same `0.0.0.0` address Guests connect to, so a Guest can never flip it themselves.

use std::sync::atomic::{AtomicBool, Ordering};

use actix_web::{App, HttpResponse, HttpServer, web};
use moonlight_common::stream::proto::control::packet::ControlPacket;
use tracing::{error, info};

/// Localhost-only; deliberately separate from the Guest-facing `web_server.bind_address`.
const INTERNAL_CONTROL_ADDR: &str = "127.0.0.1:47993";

static GUEST_HAS_CONTROL: AtomicBool = AtomicBool::new(false);

/// Off by default: Host-only. On: collaboration mode (see module doc).
static COLLABORATION_MODE: AtomicBool = AtomicBool::new(false);

pub fn guest_has_control() -> bool {
    GUEST_HAS_CONTROL.load(Ordering::Relaxed)
}

pub fn collaboration_mode_enabled() -> bool {
    COLLABORATION_MODE.load(Ordering::Relaxed)
}

/// Switches between Host-only and collaboration mode. Turning collaboration mode off also
/// immediately revokes any control the Guest currently has — Host-only means only the Host
/// ever has control, not "whoever had it last keeps it".
fn set_collaboration_mode(enabled: bool) {
    COLLABORATION_MODE.store(enabled, Ordering::Relaxed);
    if !enabled {
        GUEST_HAS_CONTROL.store(false, Ordering::Relaxed);
    }
    info!(enabled, "collaboration mode toggled");
}

/// Called from the Guest input gating path (`webrtc::stream::webrtc_loop`) when the Guest
/// sends input while not having control. In Host-only mode this is a no-op — the Guest's
/// input stays dropped, by design. In collaboration mode, it grants them control instead of
/// dropping the packet, so acting is itself the handoff.
pub fn claim_guest_control() {
    if collaboration_mode_enabled() && !guest_has_control() {
        GUEST_HAS_CONTROL.store(true, Ordering::Relaxed);
        info!("guest claimed control via input");
    }
}

/// Whether a `ControlPacket` variant represents Guest input that should be gated by
/// [`guest_has_control`]. `WebState` (keyboard state) is handled by its own call site in
/// `webrtc::stream::webrtc_loop` since it needs translating into individual key packets
/// first, not forwarded here.
pub fn is_guest_input_packet(packet: &ControlPacket) -> bool {
    matches!(
        packet,
        ControlPacket::MouseMoveRelative { .. }
            | ControlPacket::MouseMoveAbsolute { .. }
            | ControlPacket::MouseButton { .. }
            | ControlPacket::MouseScroll { .. }
            | ControlPacket::MouseHorizontalScroll { .. }
            | ControlPacket::Keyboard { .. }
            | ControlPacket::Text { .. }
            | ControlPacket::Touch { .. }
            | ControlPacket::Pen { .. }
            | ControlPacket::ControllerState { .. }
            | ControlPacket::ControllerArrival { .. }
            | ControlPacket::ControllerMotion { .. }
            | ControlPacket::ControllerBattery { .. }
            | ControlPacket::ControllerTouch { .. }
    )
}

async fn get_control_handler() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "guestHasControl": guest_has_control(),
        "collaborationModeEnabled": collaboration_mode_enabled(),
    }))
}

/// HostApp's SoftTakeoverMonitor calls this the instant it sees the Host's own physical
/// mouse/keyboard input during collaboration mode — the Host acting is itself the handoff
/// back, the same way the Guest acting (`claim_guest_control`) is the handoff to them. There
/// is no equivalent "grant" endpoint: in this design the Guest only ever gets control by
/// acting, never by the Host manually handing it over.
async fn reclaim_control_handler() -> HttpResponse {
    GUEST_HAS_CONTROL.store(false, Ordering::Relaxed);
    HttpResponse::Ok().finish()
}

async fn set_collaboration_mode_handler(enabled: web::Path<bool>) -> HttpResponse {
    set_collaboration_mode(enabled.into_inner());
    HttpResponse::Ok().finish()
}

/// Lets HostApp read the current Guest-join secret (see `guest_link`) so it can embed it in
/// the link it hands out — localhost-only, same reasoning as the rest of this module.
async fn get_guest_token_handler() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({ "token": crate::guest_link::token() }))
}

/// Runs forever (or until the process exits); spawn this as its own task.
pub async fn serve() {
    let server = HttpServer::new(|| {
        App::new()
            .route("/guest-control", web::get().to(get_control_handler))
            .route(
                "/guest-control/reclaim",
                web::put().to(reclaim_control_handler),
            )
            .route(
                "/collaboration-mode/{enabled}",
                web::put().to(set_collaboration_mode_handler),
            )
            .route("/guest-token", web::get().to(get_guest_token_handler))
    });

    let server = match server.bind(INTERNAL_CONTROL_ADDR) {
        Ok(server) => server,
        Err(err) => {
            error!(error = %err, address = INTERNAL_CONTROL_ADDR, "failed to bind internal control-gate server; HostApp won't be able to toggle guest control");
            return;
        }
    };

    if let Err(err) = server.run().await {
        error!(error = %err, "internal control-gate server stopped unexpectedly");
    }
}
