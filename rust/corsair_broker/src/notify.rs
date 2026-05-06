//! Discord webhook notifications on kill events and (optionally) fills.
//!
//! Reads `DISCORD_WEBHOOK_URL` from env at startup. If unset, all
//! `notify_*` calls are no-ops (silent — useful in test/dev where
//! we don't want spurious external requests).
//!
//! Design choices:
//! - Fire-and-forget: kill/fill code path must never block on a
//!   webhook round-trip. Spawn a tokio task and return immediately.
//! - Best-effort: if the POST fails (Discord 5xx, network glitch,
//!   timeout), we log a warning and move on. Kills are already
//!   persisted in the kill_switch JSONL; fills in the fills JSONL.
//!   Discord is for operator visibility, not the audit trail.
//! - Bounded latency: 5s timeout on the POST.
//! - No rate limit: previously a 6s gap dropped the second of any
//!   back-to-back fills (e.g. a close+open pair within 1s during the
//!   2026-05-06 morning hedge churn) and the operator never saw
//!   half the activity. Discord webhook real limits (~150/min) are
//!   well above our worst observed fill burst. If Discord starts
//!   returning 429s in practice, switch to a token bucket here.

use corsair_risk::KillEvent;
use std::time::Duration;

/// Cached webhook URL — None if env var unset. Resolved once at
/// runtime startup; subsequent reads are atomic.
static DISCORD_WEBHOOK_URL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Process-wide reqwest client for Discord webhook calls. Built once
/// on first use (after `init_from_env`) and reused. Each kill/fill
/// notification was previously rebuilding `reqwest::Client` from
/// scratch — that lazily initializes a tokio runtime resource pool +
/// rustls cert store on every call (~hundreds of µs of unnecessary
/// work for a fire-and-forget webhook).
static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|e| {
                log::warn!("notify: reqwest build failed, using default client: {e}");
                reqwest::Client::new()
            })
    })
}

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
        let client = http_client();
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

// ─── Fill notifications ──────────────────────────────────────────────

fn fill_color(side: corsair_broker_api::Side) -> u64 {
    match side {
        // Green for sells (we got hit on our offer / closed a long).
        corsair_broker_api::Side::Sell => 0x2ECC71,
        // Blue for buys.
        corsair_broker_api::Side::Buy => 0x3498DB,
    }
}

/// Optional decoration fields collected by the caller. Any field left
/// `None` is omitted from the embed, so this struct degrades gracefully
/// when the broker hasn't yet observed enough state to populate it
/// (boot before the first SABR fit, hedge fills on the underlying that
/// don't have an option label, etc.).
#[derive(Default, Debug, Clone)]
pub struct FillNotifyContext {
    pub instrument_label: String,
    /// Strike + right portion of the title, e.g. "5.85P". Falls back
    /// to instrument_label when None.
    pub leg_label: Option<String>,
    /// Expiry formatted as MM/DD.
    pub expiry_mmdd: Option<String>,
    pub underlying: Option<f64>,
    pub theo: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub delta: Option<f64>,
    /// Per-contract theta in dollars/day.
    pub theta: Option<f64>,
    /// Signed edge captured at fill in dollars (positive = we beat
    /// theo on the right side).
    pub edge_usd: Option<f64>,
    pub margin_used: Option<f64>,
    /// Effective portfolio delta (options + hedge), per CLAUDE.md §14.
    pub portfolio_delta: Option<f64>,
    pub portfolio_theta: Option<f64>,
    pub fills_today: Option<u32>,
}

fn comma_usd(v: f64) -> String {
    let neg = v < 0.0;
    let n = v.abs() as u64;
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    if neg { format!("-{out}") } else { out }
}

/// Format a fill into a Discord embed: title with side emoji, then
/// 11 inline fields (Expiry / Underlying / Theo / BBO / Delta /
/// Theta / Edge / Margin / Portfolio Δ / Portfolio θ / Fills Today).
/// Fields the caller couldn't compute are rendered as "—" so the
/// embed shape stays stable.
fn format_fill_payload(
    fill: &corsair_broker_api::events::Fill,
    ctx: &FillNotifyContext,
) -> serde_json::Value {
    let (side_word, emoji) = match fill.side {
        corsair_broker_api::Side::Buy => ("BOUGHT", "🟢"),
        corsair_broker_api::Side::Sell => ("SOLD", "🔴"),
    };
    let leg = ctx
        .leg_label
        .clone()
        .unwrap_or_else(|| ctx.instrument_label.clone());
    let title = format!(
        "{emoji} {side_word} {qty} × {leg} @ {price:.4}",
        qty = fill.qty,
        price = fill.price,
    );

    let dash = || "—".to_string();
    let exp_v = ctx.expiry_mmdd.clone().unwrap_or_else(dash);
    let und_v = ctx.underlying.map(|v| format!("{v:.2}")).unwrap_or_else(dash);
    let theo_v = ctx.theo.map(|v| format!("{v:.4}")).unwrap_or_else(dash);
    let bbo_v = match (ctx.bid, ctx.ask) {
        (Some(b), Some(a)) if b > 0.0 && a > 0.0 => format!("{b:.4} / {a:.4}"),
        _ => dash(),
    };
    let delta_v = ctx.delta.map(|v| format!("{v:.3}")).unwrap_or_else(dash);
    let theta_v = ctx.theta.map(|v| format!("${v:.1}")).unwrap_or_else(dash);
    let edge_v = ctx.edge_usd.map(|v| format!("${v:+.0}")).unwrap_or_else(dash);
    let margin_v = ctx
        .margin_used
        .map(|v| format!("${}", comma_usd(v)))
        .unwrap_or_else(dash);
    let pdelta_v = ctx
        .portfolio_delta
        .map(|v| format!("{v:+.2}"))
        .unwrap_or_else(dash);
    let ptheta_v = ctx
        .portfolio_theta
        .map(|v| format!("${}", comma_usd(v)))
        .unwrap_or_else(dash);
    let fills_v = ctx
        .fills_today
        .map(|v| v.to_string())
        .unwrap_or_else(dash);

    serde_json::json!({
        "embeds": [{
            "title": title,
            "color": fill_color(fill.side),
            "fields": [
                {"name": "Expiry",       "value": exp_v,    "inline": true},
                {"name": "Underlying",   "value": und_v,    "inline": true},
                {"name": "Theo",         "value": theo_v,   "inline": true},
                {"name": "BBO",          "value": bbo_v,    "inline": true},
                {"name": "Delta",        "value": delta_v,  "inline": true},
                {"name": "Theta",        "value": theta_v,  "inline": true},
                {"name": "Edge",         "value": edge_v,   "inline": true},
                {"name": "Margin",       "value": margin_v, "inline": true},
                {"name": "Portfolio Δ",  "value": pdelta_v, "inline": true},
                {"name": "Portfolio θ",  "value": ptheta_v, "inline": true},
                {"name": "Fills Today",  "value": fills_v,  "inline": true},
            ],
            "footer": { "text": format!("corsair_broker · exec={}", &fill.exec_id) },
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }]
    })
}

/// Fire-and-forget fill notification. Caller passes a
/// `FillNotifyContext` with whatever decoration fields it could
/// resolve from broker state — missing fields render as "—".
pub fn notify_fill(fill: corsair_broker_api::events::Fill, ctx: FillNotifyContext) {
    let url = match webhook_url() {
        Some(u) => u,
        None => return,
    };
    let payload = format_fill_payload(&fill, &ctx);
    tokio::spawn(async move {
        let client = http_client();
        match client.post(&url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                log::debug!("notify: fill posted to Discord ({})", resp.status());
            }
            Ok(resp) => {
                log::warn!("notify: Discord (fill) returned {}", resp.status());
            }
            Err(e) => {
                log::warn!("notify: Discord (fill) POST failed: {e}");
            }
        }
    });
}
