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

/// A tun inbound without routes brings the device up but routes nothing:
/// Android's `VpnService` installs routes from its own builder, so the
/// mobile config contract never carried them. On the desktop this process
/// creates the utun itself, so the default coverage is added here — split
/// halves rather than `default` so the host's own route table survives, and
/// `zero_runtime` bypass-routes the proxy servers' addresses around them.
pub fn with_desktop_tun_routes(config: &Value) -> Value {
    let mut config = config.clone();
    let Some(inbounds) = config.get_mut("inbounds").and_then(Value::as_array_mut) else {
        return config;
    };
    for inbound in inbounds.iter_mut() {
        if inbound.get("protocol").and_then(Value::as_str) != Some("tun") {
            continue;
        }
        let Some(inbound) = inbound.as_object_mut() else { continue };
        let settings = inbound
            .entry("settings")
            .or_insert_with(|| serde_json::json!({}));
        let Some(settings) = settings.as_object_mut() else {
            continue;
        };
        if settings.contains_key("routes") {
            continue;
        }
        // The runtime only installs routes when autoRoute is on; mobile hosts
        // own their routing and never set it.
        settings.insert("autoRoute".to_string(), Value::Bool(true));
        let mut routes = vec!["0.0.0.0/1".to_string(), "128.0.0.0/1".to_string()];
        let ipv6 = settings
            .get("addresses")
            .and_then(Value::as_array)
            .is_some_and(|a| {
                a.iter().filter_map(Value::as_str).any(|addr| {
                    addr.split('/').next().is_some_and(|ip| ip.contains(':'))
                })
            });
        if ipv6 {
            routes.extend(["::/1".to_string(), "8000::/1".to_string()]);
        }
        settings.insert("routes".to_string(), Value::Array(routes.into_iter().map(Value::String).collect()));
    }
    config
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
    let text = config_string(&with_desktop_tun_routes(config))?;
    lifecycle(|| {
        // SAFETY: `text` is a live NUL-terminated UTF-8 string for the whole
        // call, and the C entry point reads it without retaining the pointer.
        unsafe { zray_mobile::zray_start(text.as_ptr()) }
    })
}

/// `reload(configJson)`: swap the configuration without dropping listeners.
pub fn reload(config: &Value) -> Result<(), String> {
    let text = config_string(&with_desktop_tun_routes(config))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_routeless_tun_gets_split_default_routes() {
        let out = with_desktop_tun_routes(&json!({
            "inbounds": [{"protocol": "tun", "settings": {"addresses": ["172.19.0.1/30"], "mtu": 1500}}]
        }));
        let routes = out["inbounds"][0]["settings"]["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0], json!("0.0.0.0/1"));
        assert_eq!(out["inbounds"][0]["settings"]["autoRoute"], json!(true));
    }

    #[test]
    fn ipv6_addresses_pull_in_the_v6_halves_and_existing_routes_survive() {
        let out = with_desktop_tun_routes(&json!({
            "inbounds": [
                {"protocol": "tun", "settings": {"addresses": ["172.19.0.1/30", "fdfe::1/126"]}},
                {"protocol": "tun", "settings": {"routes": ["10.0.0.0/8"]}},
                {"protocol": "socks", "settings": {}}
            ]
        }));
        assert_eq!(out["inbounds"][0]["settings"]["routes"].as_array().unwrap().len(), 4);
        assert_eq!(out["inbounds"][1]["settings"]["routes"][0], json!("10.0.0.0/8"));
        assert!(out["inbounds"][2]["settings"].get("routes").is_none());
    }
}
