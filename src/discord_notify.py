"""Discord webhook notifications for Corsair v2 fills and alerts."""

import logging
import os
import threading
from urllib.request import Request, urlopen
import json

logger = logging.getLogger(__name__)

DISCORD_WEBHOOK_URL = os.environ.get("DISCORD_WEBHOOK_URL", "")


def send_fill_notification(
    *,
    side: str,
    quantity: int,
    strike: float,
    expiry: str,
    put_call: str,
    fill_price: float,
    theo: float,
    spread_captured: float,
    underlying: float,
    delta: float,
    theta: float,
    market_bid: float,
    market_ask: float,
    margin_after: float,
    net_delta: float,
    net_theta: float,
    fills_today: int,
):
    """Post a fill notification to Discord. Non-blocking (fires in a thread)."""
    url = DISCORD_WEBHOOK_URL
    if not url:
        return

    right_label = "P" if put_call == "P" else "C"
    exp_short = f"{expiry[4:6]}/{expiry[6:8]}"

    # Adaptive precision: HG strikes (~$5-7) and prices (~$0.04) need
    # decimals; ETH strikes (~$2400) and prices (~$80) don't. int(strike)
    # used to truncate "6.25" → "6" so multiple HG strikes appeared
    # identical in Discord. Same with .2f on sub-dollar prices.
    strike_str = (f"{strike:g}" if strike != int(strike) else str(int(strike)))
    px_fmt = (lambda v: f"{v:.4f}") if abs(fill_price) < 1 else (lambda v: f"{v:.2f}")
    bbo_fmt = (lambda v: f"{v:.4f}") if (market_bid and abs(market_bid) < 1) else (lambda v: f"{v:.2f}")
    spread = f"{bbo_fmt(market_bid)} / {bbo_fmt(market_ask)}" if market_bid > 0 and market_ask > 0 else "—"
    theo_str = px_fmt(theo) if theo and theo > 0 else "—"
    edge_str = f"${spread_captured:+.0f}" if spread_captured != 0 else "$0"

    embed = {
        "title": f"{'🟢' if side == 'BOUGHT' else '🔴'} {side} {quantity} × {strike_str}{right_label} @ {px_fmt(fill_price)}",
        "color": 0x2ECC71 if side == "BOUGHT" else 0xE74C3C,
        "fields": [
            {"name": "Expiry", "value": exp_short, "inline": True},
            {"name": "Underlying", "value": f"{underlying:.2f}", "inline": True},
            {"name": "Theo", "value": theo_str, "inline": True},
            {"name": "BBO", "value": spread, "inline": True},
            {"name": "Delta", "value": f"{delta:.3f}", "inline": True},
            {"name": "Theta", "value": f"${theta:.1f}", "inline": True},
            {"name": "Edge", "value": edge_str, "inline": True},
            {"name": "Margin", "value": f"${margin_after:,.0f}", "inline": True},
            {"name": "Portfolio Δ", "value": f"{net_delta:+.2f}", "inline": True},
            {"name": "Portfolio θ", "value": f"${net_theta:,.0f}", "inline": True},
            {"name": "Fills Today", "value": str(fills_today), "inline": True},
        ],
    }

    payload = json.dumps({"embeds": [embed]}).encode("utf-8")

    def _post():
        try:
            req = Request(url, data=payload, headers={
                "Content-Type": "application/json",
                "User-Agent": "Corsair/2.0",
            })
            urlopen(req, timeout=5)
        except Exception as e:
            logger.warning("Discord webhook failed: %s", e)

    threading.Thread(target=_post, daemon=True).start()


def send_alert(title: str, message: str, color: int = 0xE74C3C):
    """Post a critical alert to Discord. Non-blocking."""
    url = DISCORD_WEBHOOK_URL
    if not url:
        return

    embed = {"title": title, "description": message, "color": color}
    payload = json.dumps({"embeds": [embed]}).encode("utf-8")

    def _post():
        try:
            req = Request(url, data=payload, headers={
                "Content-Type": "application/json",
                "User-Agent": "Corsair/2.0",
            })
            urlopen(req, timeout=5)
        except Exception as e:
            logger.warning("Discord alert webhook failed: %s", e)

    threading.Thread(target=_post, daemon=True).start()
