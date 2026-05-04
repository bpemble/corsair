//! Discord webhook notifications on kill events.
//!
//! Reads `DISCORD_WEBHOOK_URL` from env at startup. If unset, all
//! `notify_kill` calls are no-ops (silent — useful in test/dev where
//! we don't want spurious external requests).
//!
//! Design choices:
//! - Fire-and-forget: kill code path must never block on a webhook
//!   round-trip. `notify_kill` spawns a tokio task and returns
//!   immediately.
//! - Best-effort: if the POST fails (Discord 5xx, network glitch,
//!   timeout), we log a warning and move on. The kill itself has
//!   already been logged via `log::error!` and persisted in the kill
//!   switch JSONL — Discord is for operator visibility, not the audit
//!   trail.
//! - Bounded latency: 5s timeout on the POST. Discord webhook
//!   normally responds in <500ms; a longer wait suggests rate limit
//!   or outage and should be abandoned.

use corsair_risk::KillEvent;
use std::time::Duration;

/// Cached webhook URL — None if env var unset. Resolved once at
/// runtime startup; subsequent reads are atomic.
static DISCORD_WEBHOOK_URL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

pub fn init_from_env() {
    let url = std::env::var("DISCORD_WEBHOOK_URL").ok().filter(|s| !s.is_empty());
    let _ = DISCORD_WEBHOOK_URL.set(url.clone());
    match url {
        Some(_) => log::info!("notify: Discord webhook configured"),
        None => log::info!("notify: DISCORD_WEBHOOK_URL unset — kill notifications disabled"),
    }
}

fn webhook_url() -> Option<String> {
    DISCORD_WEBHOOK_URL.get().and_then(|x| x.clone())
}

/// Format a KillEvent into a Discord-compatible message body.
fn format_kill_payload(ev: &KillEvent) -> serde_json::Value {
    let color = match ev.kill_type {
        // Red for any sticky risk kill, orange for daily halt
        // (auto-clears at session rollover).
        corsair_risk::KillType::Halt => 0xFF8800,
        corsair_risk::KillType::HedgeFlat => 0xCC0000,
        corsair_risk::KillType::Flatten => 0xCC0000,
    };
    serde_json::json!({
        "embeds": [{
            "title": format!("KILL: {:?} / {:?}", ev.source, ev.kill_type),
            "description": &ev.reason,
            "color": color,
            "footer": { "text": "corsair_broker" },
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }]
    })
}

/// Fire-and-forget kill notification. Spawns a tokio task that posts
/// the webhook with a 5s timeout. Safe to call from any sync or
/// async context that's running inside a tokio runtime.
pub fn notify_kill(ev: KillEvent) {
    let url = match webhook_url() {
        Some(u) => u,
        None => return,
    };
    let payload = format_kill_payload(&ev);
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                log::warn!("notify: reqwest build failed: {e}");
                return;
            }
        };
        match client.post(&url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                log::info!("notify: kill posted to Discord ({})", resp.status());
            }
            Ok(resp) => {
                log::warn!("notify: Discord returned {}", resp.status());
            }
            Err(e) => {
                log::warn!("notify: Discord POST failed: {e}");
            }
        }
    });
}
