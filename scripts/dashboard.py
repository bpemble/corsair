"""Corsair v2 — Real-Time Dashboard

Run with:
    streamlit run scripts/dashboard.py

Reads chain snapshot from data/chain_snapshot.json and CSV logs from logs/.
"""

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import pandas as pd

_SCRIPT_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = _SCRIPT_DIR.parent
sys.path.insert(0, str(PROJECT_ROOT))

import streamlit as st

# ---------------------------------------------------------------------------
# Auto-refresh (try streamlit-autorefresh, fall back to manual)
# ---------------------------------------------------------------------------
try:
    from streamlit_autorefresh import st_autorefresh
    _HAS_AUTOREFRESH = True
except ImportError:
    _HAS_AUTOREFRESH = False

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

SNAPSHOT_PATH = str(PROJECT_ROOT / "data" / "chain_snapshot.json")
LOG_DIR = str(PROJECT_ROOT / "logs")
FILLS_PATH = os.path.join(LOG_DIR, "fills.csv")
RISK_PATH = os.path.join(LOG_DIR, "risk_snapshots.csv")
REJECTIONS_PATH = os.path.join(LOG_DIR, "rejections.csv")

# ---------------------------------------------------------------------------
# Theme
# ---------------------------------------------------------------------------

BG_PRIMARY = "#0b0e17"
BG_CARD = "#111827"
BG_CARD_BORDER = "#1e293b"
GREEN = "#22c55e"
RED = "#ef4444"
AMBER = "#f59e0b"
TEXT_PRIMARY = "#e2e8f0"
TEXT_MUTED = "#64748b"
ACCENT = "#6366f1"

BLUE = "#38bdf8"

# ---------------------------------------------------------------------------
# Logo SVG
# ---------------------------------------------------------------------------

CORSAIR_LOGO = """<svg width="32" height="32" viewBox="0 0 100 100" fill="none" xmlns="http://www.w3.org/2000/svg">
  <circle cx="50" cy="50" r="44" stroke="#64748b" stroke-width="1.5" fill="none"/>
  <circle cx="50" cy="50" r="38" stroke="#334155" stroke-width="0.5" fill="none"/>
  <line x1="50" y1="6" x2="50" y2="14" stroke="#e2e8f0" stroke-width="2" stroke-linecap="round"/>
  <line x1="50" y1="86" x2="50" y2="94" stroke="#64748b" stroke-width="1.5" stroke-linecap="round"/>
  <line x1="6" y1="50" x2="14" y2="50" stroke="#64748b" stroke-width="1.5" stroke-linecap="round"/>
  <line x1="86" y1="50" x2="94" y2="50" stroke="#64748b" stroke-width="1.5" stroke-linecap="round"/>
  <polygon points="50,16 54,46 50,42 46,46" fill="#e2e8f0"/>
  <polygon points="50,84 54,54 50,58 46,54" fill="#64748b"/>
  <polygon points="16,50 46,46 42,50 46,54" fill="#64748b"/>
  <polygon points="84,50 54,46 58,50 54,54" fill="#64748b"/>
  <polygon points="50,16 53,44 50,40 47,44" fill="#e2e8f0" opacity="0.6" transform="rotate(45,50,50)"/>
  <polygon points="50,16 53,44 50,40 47,44" fill="#64748b" opacity="0.4" transform="rotate(135,50,50)"/>
  <polygon points="50,16 53,44 50,40 47,44" fill="#64748b" opacity="0.4" transform="rotate(225,50,50)"/>
  <polygon points="50,16 53,44 50,40 47,44" fill="#64748b" opacity="0.4" transform="rotate(315,50,50)"/>
  <circle cx="50" cy="50" r="3" fill="#e2e8f0"/>
</svg>"""

# ---------------------------------------------------------------------------
# CSS
# ---------------------------------------------------------------------------

CUSTOM_CSS = f"""
<style>
    @import url('https://fonts.googleapis.com/css2?family=DM+Serif+Display&family=Inter:wght@300;400;500;600;700&display=swap');

    #MainMenu {{visibility: hidden;}}
    footer {{visibility: hidden;}}
    header {{visibility: hidden;}}

    .stApp {{
        background-color: {BG_PRIMARY};
        color: {TEXT_PRIMARY};
        font-family: 'Inter', sans-serif;
    }}

    /* Streamlit default .block-container has 6rem top padding — tighten it */
    .block-container {{
        padding-top: 1rem !important;
        padding-bottom: 1rem !important;
    }}

    h1, h2, h3 {{
        font-family: 'DM Serif Display', serif;
        color: {TEXT_PRIMARY};
    }}

    /* ── Header ─────────────────────────────────────────── */
    .corsair-header {{
        display: flex;
        justify-content: space-between;
        align-items: center;
        padding: 12px 24px;
        background: linear-gradient(135deg, {BG_CARD} 0%, #0f172a 100%);
        border: 1px solid {BG_CARD_BORDER};
        border-radius: 12px;
        margin-bottom: 24px;
    }}
    .corsair-brand {{
        display: flex;
        align-items: center;
        gap: 16px;
    }}
    .corsair-logo {{
        display: flex;
        align-items: center;
    }}
    .corsair-title {{
        font-family: 'DM Serif Display', serif;
        font-size: 24px;
        color: {TEXT_PRIMARY};
        margin-right: 12px;
    }}
    .corsair-subtitle {{
        font-size: 13px;
        color: {TEXT_MUTED};
    }}
    .corsair-status {{
        display: flex;
        align-items: center;
    }}
    .status-pill {{
        display: inline-flex;
        align-items: center;
        gap: 8px;
        padding: 6px 14px;
        border-radius: 20px;
        background: rgba(255,255,255,0.04);
        border: 1px solid {BG_CARD_BORDER};
    }}
    .status-dot {{
        width: 8px;
        height: 8px;
        border-radius: 50%;
        display: inline-block;
    }}
    .status-dot.live {{
        background: {GREEN};
        animation: pulse 2s infinite;
    }}
    .status-dot.stale {{
        background: {AMBER};
    }}
    .status-dot.offline {{
        background: {RED};
    }}
    .status-label {{
        font-size: 13px;
        font-weight: 500;
    }}
    .status-label.live {{
        color: {GREEN};
    }}
    .status-label.stale {{
        color: {AMBER};
    }}
    .status-label.offline {{
        color: {RED};
    }}

    @keyframes pulse {{
        0% {{ opacity: 1; }}
        50% {{ opacity: 0.4; }}
        100% {{ opacity: 1; }}
    }}

    /* ── Metric Cards ──────────────────────────────────── */
    .metric-card {{
        background-color: {BG_CARD};
        border: 1px solid {BG_CARD_BORDER};
        border-radius: 8px;
        padding: 16px;
        text-align: center;
    }}
    .metric-value {{
        font-size: 26px;
        font-weight: 700;
        margin: 4px 0;
        font-family: 'Inter', sans-serif;
    }}
    .metric-label {{
        font-size: 11px;
        color: {TEXT_MUTED};
        text-transform: uppercase;
        letter-spacing: 0.05em;
    }}
    .green {{ color: {GREEN}; }}
    .red {{ color: {RED}; }}
    .amber {{ color: {AMBER}; }}
    .blue {{ color: {BLUE}; }}

    /* ── Section Header ────────────────────────────────── */
    .section-header {{
        font-family: 'DM Serif Display', serif;
        font-size: 18px;
        color: {TEXT_PRIMARY};
        margin: 20px 0 12px 0;
        padding-bottom: 8px;
        border-bottom: 1px solid {BG_CARD_BORDER};
    }}

    /* ── Kill Banner ───────────────────────────────────── */
    .kill-banner {{
        background: rgba(239, 68, 68, 0.15);
        border: 1px solid {RED};
        border-radius: 8px;
        padding: 12px 24px;
        text-align: center;
        color: {RED};
        font-weight: 600;
        margin-bottom: 16px;
    }}

    /* ── Chain Table ────────────────────────────────────── */
    .chain-table {{
        width: 100%;
        border-collapse: collapse;
        font-family: 'Inter', monospace;
        font-size: 13px;
    }}
    .chain-table th {{
        background: {BG_CARD};
        color: {TEXT_MUTED};
        font-weight: 600;
        font-size: 11px;
        text-transform: uppercase;
        letter-spacing: 0.05em;
        padding: 8px 10px;
        text-align: right;
        border-bottom: 1px solid {BG_CARD_BORDER};
        position: sticky;
        top: 0;
        z-index: 1;
    }}
    .chain-table th.left {{
        text-align: left;
    }}
    .chain-table th.center {{
        text-align: center;
    }}
    .chain-table td {{
        padding: 6px 10px;
        text-align: right;
        border-bottom: 1px solid rgba(30, 41, 59, 0.5);
        color: {TEXT_PRIMARY};
    }}
    .chain-table td.left {{
        text-align: left;
    }}
    .chain-table td.center {{
        text-align: center;
    }}
    .chain-table tr:hover {{
        background: rgba(99, 102, 241, 0.06);
    }}
    .chain-table tr.atm-row {{
        background: rgba(99, 102, 241, 0.08);
        border-left: 3px solid {ACCENT};
    }}
    .chain-table tr.atm-row:hover {{
        background: rgba(99, 102, 241, 0.14);
    }}
    .chain-strike {{
        font-weight: 700;
        color: {TEXT_PRIMARY};
        font-size: 14px;
    }}
    .chain-our-price {{
        font-weight: 600;
    }}
    .chain-our-price.live {{
        color: {BLUE};
    }}
    .chain-our-price.behind {{
        color: {RED};
    }}
    .chain-our-price.inactive {{
        color: {TEXT_MUTED};
    }}
    .chain-edge.positive {{
        color: {GREEN};
        font-weight: 600;
    }}
    .chain-edge.negative {{
        color: {RED};
    }}
    .chain-pos.long {{
        color: {GREEN};
        font-weight: 600;
    }}
    .chain-pos.short {{
        color: {RED};
        font-weight: 600;
    }}
    .chain-status {{
        font-size: 11px;
        padding: 2px 8px;
        border-radius: 10px;
        display: inline-block;
    }}
    .chain-status.quoting {{
        background: rgba(34,197,94,0.15);
        color: {GREEN};
    }}
    .chain-status.pending {{
        background: rgba(245,158,11,0.15);
        color: {AMBER};
    }}
    .chain-status.idle {{
        background: rgba(100,116,139,0.15);
        color: {TEXT_MUTED};
    }}

    /* ── Footer ────────────────────────────────────────── */
    .corsair-footer {{
        text-align: center;
        color: {TEXT_MUTED};
        font-size: 12px;
        padding: 24px 0 12px 0;
        border-top: 1px solid {BG_CARD_BORDER};
        margin-top: 32px;
    }}
</style>
"""

# ---------------------------------------------------------------------------
# Page config
# ---------------------------------------------------------------------------

st.set_page_config(
    page_title="Corsair v2",
    layout="wide",
    initial_sidebar_state="collapsed",
)

st.markdown(CUSTOM_CSS, unsafe_allow_html=True)

# Auto-refresh
if _HAS_AUTOREFRESH:
    st_autorefresh(interval=2000, limit=None, key="chain_refresh")

# ---------------------------------------------------------------------------
# Data loading
# ---------------------------------------------------------------------------


@st.cache_data(ttl=2)
def load_snapshot():
    """Load chain snapshot JSON."""
    if not os.path.exists(SNAPSHOT_PATH):
        return None
    try:
        with open(SNAPSHOT_PATH, "r") as f:
            return json.load(f)
    except (json.JSONDecodeError, IOError):
        return None


@st.cache_data(ttl=5)
def load_csv(path: str) -> Optional[pd.DataFrame]:
    if not os.path.exists(path):
        return None
    try:
        df = pd.read_csv(path)
        if df.empty:
            return None
        return df
    except Exception:
        return None


# ---------------------------------------------------------------------------
# Load data
# ---------------------------------------------------------------------------

snapshot = load_snapshot()
fills_df = load_csv(FILLS_PATH)
rejections_df = load_csv(REJECTIONS_PATH)

# ---------------------------------------------------------------------------
# Determine status
# ---------------------------------------------------------------------------

if snapshot is not None:
    ts = snapshot.get("timestamp", "")
    try:
        snap_time = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        age = (datetime.now(timezone.utc) - snap_time).total_seconds()
        if age < 10:
            status_class = "live"
            status_text = "LIVE"
        elif age < 60:
            status_class = "stale"
            status_text = f"STALE ({int(age)}s)"
        else:
            status_class = "offline"
            status_text = f"OFFLINE ({int(age)}s ago)"
    except Exception:
        status_class = "offline"
        status_text = "OFFLINE"
else:
    status_class = "offline"
    status_text = "NO DATA"

# ---------------------------------------------------------------------------
# Header
# ---------------------------------------------------------------------------

st.markdown(f"""
<div class="corsair-header">
    <div class="corsair-brand">
        <div class="corsair-logo">{CORSAIR_LOGO}</div>
        <div>
            <span class="corsair-title">Corsair</span>
            <span class="corsair-subtitle">Systematic Options Market Maker</span>
        </div>
    </div>
    <div class="corsair-status">
        <span class="status-pill">
            <span class="status-dot {status_class}"></span>
            <span class="status-label {status_class}">{status_text}</span>
        </span>
    </div>
</div>
""", unsafe_allow_html=True)

# ---------------------------------------------------------------------------
# Key Metrics Row
# ---------------------------------------------------------------------------

if snapshot is not None:
    port = snapshot.get("portfolio", {})
    underlying = snapshot.get("underlying_price", 0)
    margin_val = port.get("margin", 0)
    net_delta = port.get("net_delta", 0)
    net_theta = port.get("net_theta", 0)
    fills_today = port.get("fills_today", 0)
    spread_capture = port.get("spread_capture", 0)

    def _delta_color(d):
        if abs(d) < 0.5:
            return "green"
        if abs(d) < 1.0:
            return "amber"
        return "red"

    def _theta_color(t):
        if t > -100:
            return "green"
        if t > -400:
            return "amber"
        return "red"

    col1, col2, col3, col4, col5, col6 = st.columns(6)

    with col1:
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Underlying</div>
            <div class="metric-value">${underlying:,.2f}</div>
        </div>
        """, unsafe_allow_html=True)

    with col2:
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Margin</div>
            <div class="metric-value">${margin_val:,.0f}</div>
        </div>
        """, unsafe_allow_html=True)

    with col3:
        dc = _delta_color(net_delta)
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Net Delta</div>
            <div class="metric-value {dc}">{net_delta:+.2f}</div>
        </div>
        """, unsafe_allow_html=True)

    with col4:
        tc = _theta_color(net_theta)
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Net Theta</div>
            <div class="metric-value {tc}">${net_theta:,.0f}/day</div>
        </div>
        """, unsafe_allow_html=True)

    with col5:
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Fills Today</div>
            <div class="metric-value">{fills_today}</div>
        </div>
        """, unsafe_allow_html=True)

    with col6:
        sc_color = "green" if spread_capture >= 0 else "red"
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Spread Capture</div>
            <div class="metric-value {sc_color}">${spread_capture:,.0f}</div>
        </div>
        """, unsafe_allow_html=True)
else:
    st.info("No snapshot data. Start Corsair v2 to begin.")

# ---------------------------------------------------------------------------
# Account & Positions
# ---------------------------------------------------------------------------

if snapshot is not None:
    acct = snapshot.get("account", {})
    port = snapshot.get("portfolio", {})
    positions = port.get("positions", [])

    st.markdown('<div class="section-header">Account</div>', unsafe_allow_html=True)

    a1, a2, a3, a4, a5, a6 = st.columns(6)
    cells = [
        ("Account",       acct.get("account_id", "—"),                None),
        ("Cash",          f"${acct.get('cash', 0):,.0f}",              None),
        ("Net Liq",       f"${acct.get('net_liq', 0):,.0f}",           None),
        ("Maint Margin",  f"${acct.get('maint_margin', 0):,.0f}",      None),
        ("Unrealized P&L", f"${acct.get('unrealized_pnl', 0):,.0f}",   "green" if acct.get("unrealized_pnl", 0) >= 0 else "red"),
        ("Realized P&L",  f"${acct.get('realized_pnl', 0):,.0f}",      "green" if acct.get("realized_pnl", 0) >= 0 else "red"),
    ]
    for col, (label, value, color) in zip([a1, a2, a3, a4, a5, a6], cells):
        cls = f" {color}" if color else ""
        col.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">{label}</div>
            <div class="metric-value{cls}">{value}</div>
        </div>
        """, unsafe_allow_html=True)

    st.markdown('<div class="section-header">Open Positions</div>', unsafe_allow_html=True)
    if positions:
        pos_rows = ""
        for p in sorted(positions, key=lambda x: (x["expiry"], x["right"], x["strike"])):
            qty = p["qty"]
            qty_class = "pos-long" if qty > 0 else "pos-short"
            unr = p["unrealized_pnl"]
            unr_class = "positive" if unr >= 0 else "negative"
            pos_rows += f"""<tr>
                <td>{p['expiry']}</td>
                <td>{int(p['strike'])}{p['right']}</td>
                <td class="{qty_class}">{qty:+d}</td>
                <td>${p['avg_price']:.2f}</td>
                <td>${p['mark']:.2f}</td>
                <td class="chain-edge {unr_class}">${unr:+,.0f}</td>
                <td>{p['delta']:+.3f}</td>
                <td>${p['theta']:+,.0f}</td>
            </tr>"""
        positions_html = f"""
        <table class="chain-table">
            <thead><tr>
                <th>Expiry</th><th>Contract</th><th>Qty</th><th>Avg Price</th>
                <th>Mark</th><th>Unrealized P&amp;L</th><th>&Delta;</th><th>&Theta;/day</th>
            </tr></thead>
            <tbody>{pos_rows}</tbody>
        </table>
        """
        st.markdown(positions_html, unsafe_allow_html=True)
    else:
        st.caption("No open positions.")

# ---------------------------------------------------------------------------
# Option Chain Table
# ---------------------------------------------------------------------------

st.markdown('<div class="section-header">Option Chain</div>', unsafe_allow_html=True)

if snapshot is not None and snapshot.get("strikes"):
    atm_strike = snapshot.get("atm_strike", 0)
    strikes = snapshot["strikes"]

    # Build HTML table
    rows_html = ""
    for strike_str in sorted(strikes.keys(), key=lambda s: float(s)):
        s = strikes[strike_str]
        strike_val = float(strike_str)
        is_atm = abs(strike_val - atm_strike) < 1.0
        row_class = ' class="atm-row"' if is_atm else ""

        mkt_bid = s.get("market_bid", 0) or 0
        mkt_ask = s.get("market_ask", 0) or 0
        our_bid = s.get("our_bid")
        our_ask = s.get("our_ask")
        bid_live = s.get("bid_live", False)
        ask_live = s.get("ask_live", False)
        theo = s.get("theo")
        delta = s.get("delta", 0)
        iv = s.get("iv", 0)
        volume = s.get("volume", 0) or 0
        oi = s.get("open_interest", 0) or 0
        pos = s.get("position", 0)
        status = s.get("status", "idle")

        # Format our bid
        if our_bid is not None:
            if bid_live and our_bid > mkt_bid:
                bid_class = "live"
            elif bid_live:
                bid_class = "behind"
            else:
                bid_class = "inactive"
            our_bid_str = f'<span class="chain-our-price {bid_class}">{our_bid:.1f}</span>'
        else:
            our_bid_str = f'<span class="chain-our-price inactive">-</span>'

        # Format our ask
        if our_ask is not None:
            if ask_live and our_ask < mkt_ask:
                ask_class = "live"
            elif ask_live:
                ask_class = "behind"
            else:
                ask_class = "inactive"
            our_ask_str = f'<span class="chain-our-price {ask_class}">{our_ask:.1f}</span>'
        else:
            our_ask_str = f'<span class="chain-our-price inactive">-</span>'

        # Theo
        theo_str = f"{theo:.1f}" if theo is not None else "-"

        # Edge per side vs theo
        # Bid edge = theo - our_bid (positive = we buy below theo = good)
        # Ask edge = our_ask - theo (positive = we sell above theo = good)
        edge_str = "-"
        if theo is not None:
            edges = []
            if our_bid is not None:
                edges.append(theo - our_bid)
            if our_ask is not None:
                edges.append(our_ask - theo)
            if edges:
                avg_edge = sum(edges) / len(edges)
                edge_class = "positive" if avg_edge > 0 else "negative"
                edge_str = f'<span class="chain-edge {edge_class}">{avg_edge:+.1f}</span>'

        # Position
        if pos > 0:
            pos_str = f'<span class="chain-pos long">+{pos}</span>'
        elif pos < 0:
            pos_str = f'<span class="chain-pos short">{pos}</span>'
        else:
            pos_str = f'<span style="color:{TEXT_MUTED}">0</span>'

        # Status pill
        status_str = f'<span class="chain-status {status}">{status}</span>'

        rows_html += f"""<tr{row_class}>
            <td class="center"><span class="chain-strike">{strike_str}</span></td>
            <td class="center">{volume:,}</td>
            <td class="center">{oi:,}</td>
            <td class="center">{pos_str}</td>
            <td>{mkt_bid:.1f}</td>
            <td>{our_bid_str}</td>
            <td style="background-color:rgba(99,102,241,0.08);color:{TEXT_PRIMARY};font-weight:500;">{theo_str}</td>
            <td>{our_ask_str}</td>
            <td>{mkt_ask:.1f}</td>
            <td class="center">{volume:,}</td>
            <td class="center">{oi:,}</td>
            <td>{edge_str}</td>
            <td>{delta:.3f}</td>
            <td class="center">{status_str}</td>
        </tr>"""

    chain_html = f"""
    <div style="max-height: 600px; overflow-y: auto; border: 1px solid {BG_CARD_BORDER}; border-radius: 8px;">
    <table class="chain-table">
        <thead>
            <tr>
                <th class="center">Strike</th>
                <th class="center">Vol</th>
                <th class="center">OI</th>
                <th class="center">Pos</th>
                <th>Mkt Bid</th>
                <th>Our Bid</th>
                <th>Theo</th>
                <th>Our Ask</th>
                <th>Mkt Ask</th>
                <th class="center">Vol</th>
                <th class="center">OI</th>
                <th>Edge</th>
                <th>Delta</th>
                <th class="center">Status</th>
            </tr>
        </thead>
        <tbody>
            {rows_html}
        </tbody>
    </table>
    </div>
    """
    st.markdown(chain_html, unsafe_allow_html=True)
else:
    st.info("No option chain data available.")

# ---------------------------------------------------------------------------
# Tabs: Fills / Rejections
# ---------------------------------------------------------------------------

st.markdown('<div class="section-header">Logs</div>', unsafe_allow_html=True)

tab1, tab2 = st.tabs(["Fills", "Rejections"])

with tab1:
    if fills_df is not None and len(fills_df) > 0:
        display_df = fills_df.iloc[::-1].head(50)
        st.dataframe(display_df, use_container_width=True, hide_index=True)
    else:
        st.info("No fills recorded yet.")

with tab2:
    if rejections_df is not None and len(rejections_df) > 0:
        display_df = rejections_df.iloc[::-1].head(50)
        st.dataframe(display_df, use_container_width=True, hide_index=True)
    else:
        st.info("No rejections recorded yet.")

# ---------------------------------------------------------------------------
# Footer
# ---------------------------------------------------------------------------

st.markdown(
    '<div class="corsair-footer">Corsair v2.0 | Ethereal Capital</div>',
    unsafe_allow_html=True,
)

# ---------------------------------------------------------------------------
# Fallback auto-refresh if streamlit-autorefresh not installed
# ---------------------------------------------------------------------------

if not _HAS_AUTOREFRESH:
    import time as _time
    if "last_refresh" not in st.session_state:
        st.session_state.last_refresh = _time.time()
    _elapsed = _time.time() - st.session_state.last_refresh
    if _elapsed > 3:
        st.session_state.last_refresh = _time.time()
        st.rerun()
