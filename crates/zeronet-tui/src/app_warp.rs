//! The WARP dialog: asking before an account is made, showing the work as it
//! happens, and managing a profile that already has one.
//!
//! Getting an account registers a device with Cloudflare, so it starts only
//! from the dialog's own button — the dialog says what will happen and links
//! the terms, which is the person's consent — and never from a keypress alone.

use std::time::Duration;

use super::*;
use app_tasks::BgEvent;
use zeronet_tui::modal::{move_warp_selection, push_warp_step, WarpPhase, WARP_FIRST_STEP};

/// What a finished WARP job hands back.
pub(crate) struct WarpDone {
    /// The profile it changed, or `None` when it made a new one.
    pub(crate) profile: Option<i64>,
    /// The `warp://` link of the account, exits included.
    pub(crate) link: String,
    /// Servers found that work through it.
    pub(crate) exits: usize,
    /// How it connects (`auto`, `masque-h2`, ...).
    pub(crate) route: String,
    /// The account's public fingerprint.
    pub(crate) fingerprint: Option<String>,
}

/// How many servers to look for, how many to try, and for how long.
const WANT: usize = 4;
const SAMPLE: usize = 100;
const SEARCH_BUDGET: Duration = Duration::from_secs(60);

/// The `warp://` link of a stored profile, if it is one.
fn warp_link_of(record: &ConfigRecord) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(&record.raw_content).ok()?;
    value
        .get("outbounds")?
        .as_array()?
        .iter()
        .find_map(|outbound| {
            outbound
                .get("link")?
                .as_str()
                .filter(|link| link.starts_with("warp://"))
                .map(str::to_string)
        })
}

/// `raw` (a stored profile's JSON) with its `warp://` link replaced.
fn with_link(raw: &str, link: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let mut replaced = false;
    for outbound in value.get_mut("outbounds")?.as_array_mut()? {
        let is_warp = outbound
            .get("link")
            .and_then(|l| l.as_str())
            .is_some_and(|l| l.starts_with("warp://"));
        if is_warp {
            outbound["link"] = link.into();
            replaced = true;
        }
    }
    replaced
        .then(|| serde_json::to_string_pretty(&value).ok())
        .flatten()
}

impl App<'_> {
    /// The selected profile, when it is a WARP one: its id, name and link.
    fn selected_warp_profile(&self) -> Option<(i64, String, String)> {
        let record = *self.visible_configs().get(self.selected_config_idx)?;
        Some((record.id, record.remark.clone(), warp_link_of(record)?))
    }

    /// Open the dialog: the manager when a WARP profile is selected, the offer
    /// otherwise, and the work in progress if there is some.
    pub(crate) fn open_warp_dialog(&mut self) {
        let phase = if self.bg.warp_in_flight {
            WarpPhase::Working {
                steps: self.bg.warp_steps.clone(),
            }
        } else if let Some((profile, remark, link)) = self.selected_warp_profile() {
            match zero_discovery::warp::summarize(&link) {
                Some(summary) => WarpPhase::Manage {
                    profile,
                    remark,
                    exits: summary.exits,
                    reverse: summary.reverse,
                    route: summary.route.to_string(),
                    selected: 0,
                },
                None => WarpPhase::Offer,
            }
        } else {
            WarpPhase::Offer
        };
        self.show_warp(phase, true);
    }

    /// Put `phase` on screen: into the open WARP dialog, or (when `open`)
    /// into a new one.
    fn show_warp(&mut self, phase: WarpPhase, open: bool) {
        if let ModalState::Warp { phase: shown, .. } = &mut self.modal_state {
            *shown = phase;
        } else if open {
            self.modal_state = ModalState::Warp {
                phase,
                created_tick: self.effects.current_tick(),
            };
            self.open_modal_effect();
        }
    }

    fn warp_dialog_open(&self) -> bool {
        matches!(self.modal_state, ModalState::Warp { .. })
    }

    /// The dialog's main button: Enter, or a click.
    pub(crate) async fn warp_primary(&mut self) -> Result<()> {
        let ModalState::Warp { phase, .. } = &self.modal_state else {
            return Ok(());
        };
        match phase.clone() {
            WarpPhase::Offer => self.start_warp_registration(),
            WarpPhase::Working { .. } => {}
            WarpPhase::Done { profile, .. } => {
                self.close_modal();
                let action = self.connection.connect_to(profile);
                self.apply_engine_action(action).await?;
            }
            WarpPhase::Failed(_) => match self.bg.warp_target {
                Some(profile) => self.start_warp_search(profile),
                None => self.start_warp_registration(),
            },
            WarpPhase::Manage { selected, .. } => self.warp_option(selected),
        }
        Ok(())
    }

    /// Up and down in the manage list.
    pub(crate) fn warp_move(&mut self, delta: i32) {
        if let ModalState::Warp {
            phase: WarpPhase::Manage { selected, .. },
            ..
        } = &mut self.modal_state
        {
            *selected = move_warp_selection(*selected, delta);
        }
    }

    /// One entry of the manage list, chosen by Enter or a click.
    pub(crate) fn warp_option(&mut self, index: usize) {
        let ModalState::Warp {
            phase:
                WarpPhase::Manage {
                    profile,
                    reverse,
                    selected,
                    ..
                },
            ..
        } = &mut self.modal_state
        else {
            return;
        };
        *selected = index;
        let (profile, reverse) = (*profile, *reverse);
        match index {
            0 => self.start_warp_search(profile),
            1 => self.flip_warp_order(profile, reverse),
            _ => self.start_warp_registration(),
        }
    }

    /// Change which path goes first, keeping everything else.
    fn flip_warp_order(&mut self, profile: i64, reverse: bool) {
        let Some(record) = self.config_by_id(profile).cloned() else {
            return;
        };
        let Some(link) = warp_link_of(&record) else {
            return;
        };
        let changed = zero_discovery::warp::link_with_mode(&link, !reverse)
            .ok()
            .and_then(|new| with_link(&record.raw_content, &new));
        let Some(raw) = changed else {
            self.toasts.warning(
                "Find servers for this account first; there is nothing to go through yet.",
            );
            return;
        };
        if self
            .db
            .update_config_content(profile, &record.address, record.port, &raw)
            .is_err()
        {
            self.toasts.error("Could not save the change.");
            return;
        }
        self.reload_configs();
        if let ModalState::Warp {
            phase: WarpPhase::Manage { reverse, .. },
            ..
        } = &mut self.modal_state
        {
            *reverse = !*reverse;
        }
        self.toasts.success(if reverse {
            "The tunnel alone goes first now; servers are the failsafe."
        } else {
            "Servers go first now; the tunnel alone is the failsafe."
        });
    }

    /// Ask Cloudflare for a free account, then look for servers that work
    /// through it, and add it all as a profile.
    pub(crate) fn start_warp_registration(&mut self) {
        if self.bg.warp_in_flight {
            self.open_warp_dialog();
            return;
        }
        self.begin_warp_work(None);
        let tunnel = self
            .connection
            .dialled()
            .map(|_| std::net::SocketAddr::from(([127, 0, 0, 1], self.settings.http_port)));
        let tx = self.bg.tx.clone();
        tokio::spawn(async move {
            let progress = {
                let tx = tx.clone();
                move |line: &str| {
                    let _ = tx.send(BgEvent::WarpProgress(line.to_string()));
                }
            };
            let result = async {
                let relay = std::env::var("ZERONET_WARP_RELAY")
                    .ok()
                    .zip(std::env::var("ZERONET_WARP_RELAY_AUTH").ok());
                let link =
                    zero_discovery::warp::register_anywhere(tunnel, relay, true, &progress).await?;
                // The account is useful without servers, so a search that
                // finds none is not an error here.
                let exits = zero_discovery::warp::gather_exits(
                    &link,
                    WANT,
                    SAMPLE,
                    SEARCH_BUDGET,
                    &progress,
                )
                .await
                .unwrap_or_default();
                let link = if exits.is_empty() {
                    link
                } else {
                    zero_discovery::warp::link_with_exits(&link, &exits, false).unwrap_or(link)
                };
                let route = zero_discovery::warp::summarize(&link)
                    .map_or("auto", |summary| summary.route)
                    .to_string();
                Ok(WarpDone {
                    profile: None,
                    exits: exits.len(),
                    route,
                    fingerprint: zero_discovery::warp::fingerprint(&link),
                    link,
                })
            }
            .await;
            let _ = tx.send(BgEvent::WarpFinished(result));
        });
    }

    /// Look for servers that work through the account of `profile`, and list
    /// them on it.
    pub(crate) fn start_warp_search(&mut self, profile: i64) {
        if self.bg.warp_in_flight {
            self.open_warp_dialog();
            return;
        }
        let Some(link) = self.config_by_id(profile).and_then(warp_link_of) else {
            return;
        };
        self.begin_warp_work(Some(profile));
        let reverse = zero_discovery::warp::summarize(&link).is_some_and(|s| s.reverse);
        let tx = self.bg.tx.clone();
        tokio::spawn(async move {
            let progress = {
                let tx = tx.clone();
                move |line: &str| {
                    let _ = tx.send(BgEvent::WarpProgress(line.to_string()));
                }
            };
            let result = async {
                let exits = zero_discovery::warp::gather_exits(
                    &link,
                    WANT + 1,
                    SAMPLE,
                    SEARCH_BUDGET,
                    &progress,
                )
                .await?;
                if exits.is_empty() {
                    return Err(
                        "No server carried a request through this account right now. Try again in a while."
                            .to_string(),
                    );
                }
                let new = zero_discovery::warp::link_with_exits(&link, &exits, reverse)?;
                let route = zero_discovery::warp::summarize(&new)
                    .map_or("auto", |summary| summary.route)
                    .to_string();
                Ok(WarpDone {
                    profile: Some(profile),
                    exits: exits.len(),
                    route,
                    fingerprint: zero_discovery::warp::fingerprint(&new),
                    link: new,
                })
            }
            .await;
            let _ = tx.send(BgEvent::WarpFinished(result));
        });
    }

    fn begin_warp_work(&mut self, target: Option<i64>) {
        self.bg.warp_in_flight = true;
        self.bg.warp_target = target;
        self.bg.warp_steps = vec![WARP_FIRST_STEP.to_string()];
        self.show_warp(
            WarpPhase::Working {
                steps: self.bg.warp_steps.clone(),
            },
            true,
        );
    }

    /// A line of progress from the running job.
    pub(crate) fn on_warp_progress(&mut self, line: String) {
        push_warp_step(&mut self.bg.warp_steps, line);
        if let ModalState::Warp {
            phase: WarpPhase::Working { steps },
            ..
        } = &mut self.modal_state
        {
            *steps = self.bg.warp_steps.clone();
        }
    }

    /// The running job finished: store what it made and say how it went.
    pub(crate) fn on_warp_finished(&mut self, result: Result<WarpDone, String>) {
        self.bg.warp_in_flight = false;
        let done = match result {
            Ok(done) => done,
            Err(reason) => {
                if self.warp_dialog_open() {
                    self.show_warp(WarpPhase::Failed(reason), false);
                } else {
                    self.toasts.error(reason);
                }
                return;
            }
        };
        let stored = match done.profile {
            Some(id) => self.replace_warp_link(id, &done.link).map(|()| id),
            None => zero_config::parse_link(&done.link)
                .map_err(|error| error.to_string())
                .and_then(|link| self.store_share_link(&link).map_err(|e| e.to_string())),
        };
        let id = match stored {
            Ok(id) => id,
            Err(reason) => {
                let message = format!("The account was made but could not be saved: {reason}");
                if self.warp_dialog_open() {
                    self.show_warp(WarpPhase::Failed(message), false);
                } else {
                    self.toasts.error(message);
                }
                return;
            }
        };
        self.reload_configs();
        self.focus_profile(id);
        let servers = match done.exits {
            0 => "No servers were found that work through it yet; you can look again from this dialog."
                .to_string(),
            1 => "1 server works through it and is kept as a failsafe.".to_string(),
            n => format!("{n} servers work through it and are kept as a failsafe."),
        };
        let headline = if done.profile.is_some() {
            "Servers updated"
        } else {
            "WARP is ready"
        };
        let detail = format!(
            "It connects by {}. {servers}",
            match done.route.as_str() {
                "auto" => "trying every way at once and keeping the first that works",
                other => other,
            }
        );
        if self.warp_dialog_open() {
            self.show_warp(
                WarpPhase::Done {
                    profile: id,
                    headline: headline.into(),
                    detail,
                    fingerprint: done.fingerprint.clone(),
                    finished_tick: self.effects.current_tick(),
                },
                false,
            );
        } else {
            self.toasts.success(format!("{headline}. {detail}"));
        }
    }

    /// Swap the link inside a stored profile.
    fn replace_warp_link(&mut self, id: i64, link: &str) -> Result<(), String> {
        let record = self
            .config_by_id(id)
            .cloned()
            .ok_or_else(|| "the profile is gone".to_string())?;
        let raw = with_link(&record.raw_content, link)
            .ok_or_else(|| "the profile is not a WARP profile any more".to_string())?;
        self.db
            .update_config_content(id, &record.address, record.port, &raw)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_warp_profile_is_recognised_by_its_link_and_the_link_can_be_swapped() {
        let raw = serde_json::json!({
            "outbounds": [
                {"tag": "proxy", "link": "warp://abc#WARP"},
                {"tag": "direct", "protocol": "freedom"}
            ]
        })
        .to_string();
        let swapped = with_link(&raw, "warp://def#WARP").unwrap();
        let value: serde_json::Value = serde_json::from_str(&swapped).unwrap();
        assert_eq!(value["outbounds"][0]["link"], "warp://def#WARP");
        assert_eq!(value["outbounds"][1]["protocol"], "freedom");
        // Anything else is left alone.
        let other =
            serde_json::json!({"outbounds": [{"tag": "proxy", "link": "vless://x"}]}).to_string();
        assert!(with_link(&other, "warp://def").is_none());
        assert!(with_link("not json", "warp://def").is_none());
    }
}
