//! The INSTANCE lifecycle, on top of `zray-mobile`'s lib entry points.
//!
//! The daemon is a normal process, not a library inside somebody else's VPN
//! service, so it does not need the C ABI's pointer round-trips: it calls the
//! very same functions (`zray_start`, `zray_reload`, `zray_stop`, …) directly
//! from Rust and reads failures back through
//! [`zray_mobile::last_error_message`] rather than `zray_last_error`.
//!
//! The one desktop addition is privilege handling: a configuration with a
//! `tun` inbound makes the runtime open a macOS `utun` itself (see
//! `zero_runtime::Server::serve_tun` and `zero_tun::open_utun`), which needs
//! root. The daemon does not spawn an elevation prompt — it answers
//! `{"error": "requires root"}` and lets the GUI show its own.

use std::ffi::CString;

use serde_json::Value;
use zero_tun::privilege::{check_tun_permissions, PrivilegeStatus};

/// Whether the proxy runtime is up. Mirrors the contract's `isRunning()`.
pub fn is_running() -> bool {
    zray_mobile::zray_is_running() == 1
}

/// `inbounds[]` carrying `"protocol": "tun"` — the ones that need a TUN
/// device, which on macOS means root.
pub fn has_tun_inbound(config: &Value) -> bool {
    config
        .get("inbounds")
        .and_then(Value::as_array)
        .is_some_and(|inbounds| {
            inbounds
                .iter()
                .any(|inbound| inbound.get("protocol").and_then(Value::as_str) == Some("tun"))
        })
}

/// Map `zero_tun`'s desktop permission check onto the RPC's error strings.
fn tun_privilege_error() -> Option<String> {
    match check_tun_permissions() {
        PrivilegeStatus::Privileged => None,
        PrivilegeStatus::NeedsElevation(_) => Some("requires root".to_string()),
        PrivilegeStatus::MissingDriver(reason) => {
            Some(format!("the TUN driver is unavailable: {reason}"))
        }
        PrivilegeStatus::Unsupported => {
            Some("TUN mode is not supported on this platform".to_string())
        }
    }
}

/// Run one C-ABI lifecycle call and turn its status code into the
/// contract's `String?` outcome (`None` = success).
fn lifecycle<F>(call: F) -> Result<(), String>
where
    F: FnOnce() -> std::ffi::c_int,
{
    let code = call();
    if code == zray_mobile::ZRAY_OK {
        Ok(())
    } else {
        Err(zray_mobile::last_error_message()
            .unwrap_or_else(|| format!("the engine call failed with status {code}")))
    }
}

fn config_string(config: &Value) -> Result<CString, String> {
    CString::new(config.to_string())
        .map_err(|_| "the configuration contains a NUL byte".to_string())
}

/// `start(configJson)`: begin serving the generation described by `config`.
/// Blocks until the listeners are up (bounded by zray-mobile's own
/// `START_TIMEOUT_SECONDS`); call it from a blocking context.
pub fn start(config: &Value) -> Result<(), String> {
    if has_tun_inbound(config) {
        if let Some(reason) = tun_privilege_error() {
            return Err(reason);
        }
    }
    let text = config_string(config)?;
    lifecycle(|| {
        // SAFETY: `text` is a live NUL-terminated UTF-8 string for the whole
        // call, and the C entry point reads it without retaining the pointer.
        unsafe { zray_mobile::zray_start(text.as_ptr()) }
    })
}

/// `reload(configJson)`: swap the configuration without dropping listeners.
pub fn reload(config: &Value) -> Result<(), String> {
    let text = config_string(config)?;
    lifecycle(|| unsafe { zray_mobile::zray_reload(text.as_ptr()) })
}

/// `stop()`: tear the runtime down. An error when nothing was running, as
/// the contract specifies.
pub fn stop() -> Result<(), String> {
    lifecycle(|| zray_mobile::zray_stop())
}

/// `networkChanged()`: re-install the current generation so the resolver
/// drops its cache and pooled DoH/DoT connections. A no-op when idle.
pub fn network_changed() -> Result<(), String> {
    lifecycle(|| zray_mobile::zray_network_changed())
}

/// `stats()`: the contract's counters, or `None` when nothing is running.
pub fn stats() -> Option<Value> {
    zray_mobile::stats_value()
}
