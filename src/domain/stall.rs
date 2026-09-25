//! The thresholds of the stalled-session checks (ADR-0043 decision 4): the
//! `[stall]` table of the repository's `dagq.toml`, each a positive number
//! of seconds, with the defaults a person chose (2026-09-25).
use serde::Serialize;
use serde_json::Value;

use super::RunEvent;

/// Receipt-less idle before the nudge, and again before the `stalled` ask.
pub const DEFAULT_IDLE_WITHOUT_RECEIPT_SECS: i64 = 20 * 60;
/// How long a sent text may wait to be taken up, and again after the Enter.
pub const DEFAULT_SEND_CONFIRM_SECS: i64 = 60;
/// Background work running this long is a `long_background` alert.
pub const DEFAULT_BACKGROUND_ALERT_SECS: i64 = 30 * 60;

/// The event the supervisor records with the values it loaded at start.
pub const STALL_CONFIG_LOADED: &str = "stall_config_loaded";

/// One background task an idle marker lists as `running`: its ID and what
/// it runs, as the agent described it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BackgroundTask {
    pub id: String,
    pub description: String,
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StallConfig {
    pub idle_without_receipt_secs: i64,
    pub send_confirm_secs: i64,
    pub background_alert_secs: i64,
}

impl Default for StallConfig {
    fn default() -> Self {
        Self {
            idle_without_receipt_secs: DEFAULT_IDLE_WITHOUT_RECEIPT_SECS,
            send_confirm_secs: DEFAULT_SEND_CONFIRM_SECS,
            background_alert_secs: DEFAULT_BACKGROUND_ALERT_SECS,
        }
    }
}

impl StallConfig {
    /// The setting names of the `[stall]` table.
    pub const KEYS: [&str; 3] = [
        "idle_without_receipt_secs",
        "send_confirm_secs",
        "background_alert_secs",
    ];

    /// The setting `key` set to `secs`; `None` for a key the table does not have.
    pub fn set(&mut self, key: &str, secs: i64) -> Option<()> {
        let field = match key {
            "idle_without_receipt_secs" => &mut self.idle_without_receipt_secs,
            "send_confirm_secs" => &mut self.send_confirm_secs,
            "background_alert_secs" => &mut self.background_alert_secs,
            _ => return None,
        };
        *field = secs;
        Some(())
    }

    /// The values of the latest `stall_config_loaded` in `events`: what the
    /// supervisor that recorded it runs with. A value missing from the
    /// payload keeps its default.
    pub fn loaded(events: &[RunEvent]) -> Option<Self> {
        let event = events
            .iter()
            .rev()
            .find(|event| event.kind == STALL_CONFIG_LOADED)?;
        let mut config = Self::default();
        for key in Self::KEYS {
            if let Some(secs) = event
                .payload
                .get(key)
                .and_then(Value::as_i64)
                .filter(|&secs| secs > 0)
            {
                config.set(key, secs);
            }
        }
        Some(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
        }
    }

    #[test]
    fn the_latest_loaded_config_wins_over_the_defaults() {
        assert_eq!(StallConfig::loaded(&[]), None);
        let events = [
            event(1, STALL_CONFIG_LOADED, json!({"send_confirm_secs": 5})),
            event(
                2,
                STALL_CONFIG_LOADED,
                json!({"idle_without_receipt_secs": 600, "send_confirm_secs": 0}),
            ),
            event(3, "run_claimed", json!({})),
        ];
        assert_eq!(
            StallConfig::loaded(&events),
            Some(StallConfig {
                idle_without_receipt_secs: 600,
                ..StallConfig::default()
            })
        );
        assert_eq!(StallConfig::default().set("other", 1), None);
    }
}
