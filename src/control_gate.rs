//! Gates whether a Guest's mouse/keyboard/gamepad input is forwarded to the Host.
//!
//! Sunshine assumes the Host machine is otherwise idle (streaming to a console/TV); Ableton
//! Together's Host is actively sitting at their own computer running their DAW, so by
//! default a Guest's input is dropped instead of fighting the Host for cursor control.
//! HostApp flips this (handing control to the Guest and back) through a tiny HTTP API
//! bound to localhost only — not exposed on the same `0.0.0.0` address Guests connect to,
//! so a Guest can never grant themselves control by hitting the endpoint directly.

use std::sync::atomic::{AtomicBool, Ordering};

use actix_web::{App, HttpResponse, HttpServer, web};
use moonlight_common::stream::proto::control::packet::ControlPacket;
use tracing::{error, info};

/// Localhost-only; deliberately separate from the Guest-facing `web_server.bind_address`.
const INTERNAL_CONTROL_ADDR: &str = "127.0.0.1:47993";

static GUEST_HAS_CONTROL: AtomicBool = AtomicBool::new(false);

pub fn guest_has_control() -> bool {
    GUEST_HAS_CONTROL.load(Ordering::Relaxed)
}

fn set_guest_has_control(enabled: bool) {
    GUEST_HAS_CONTROL.store(enabled, Ordering::Relaxed);
    info!(enabled, "guest control toggled");
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

async fn set_control_handler(enabled: web::Path<bool>) -> HttpResponse {
    set_guest_has_control(enabled.into_inner());
    HttpResponse::Ok().finish()
}

async fn get_control_handler() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({ "guestHasControl": guest_has_control() }))
}

/// Runs forever (or until the process exits); spawn this as its own task.
pub async fn serve() {
    let server = HttpServer::new(|| {
        App::new()
            .route(
                "/guest-control/{enabled}",
                web::put().to(set_control_handler),
            )
            .route("/guest-control", web::get().to(get_control_handler))
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
