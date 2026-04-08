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
import streamlit.components.v1 as components

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
# Desaturated steady-state variants — used for table cells & passive labels.
# Reserve full-saturation GREEN/RED/AMBER for kill banners, alerts, and the
# LIVE indicator where the eye should snap to them.
GREEN_MUTED = "#4d9b6e"
RED_MUTED = "#b86a6a"
AMBER_MUTED = "#b88a3e"
TEXT_PRIMARY = "#cbd5e1"
TEXT_SECONDARY = "#94a3b8"
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
        gap: 10px;
    }}
    .status-pill {{
        display: inline-flex;
        align-items: center;
        gap: 8px;
        padding: 6px 12px;
        border-radius: 8px;
        background: rgba(255,255,255,0.025);
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
    .latency-pill {{
        display: inline-flex;
        flex-direction: column;
        align-items: flex-start;
        gap: 2px;
        padding: 5px 12px;
        border-radius: 8px;
        background: rgba(255,255,255,0.025);
        border: 1px solid {BG_CARD_BORDER};
        font-family: ui-monospace, "SF Mono", Menlo, monospace;
        font-size: 10.5px;
        line-height: 1.2;
        color: {TEXT_MUTED};
        letter-spacing: 0.2px;
    }}
    .latency-pill .lat-row {{
        display: inline-flex;
        gap: 8px;
    }}
    .latency-pill .lat-key {{
        opacity: 0.7;
    }}

    @keyframes pulse {{
        0% {{ opacity: 1; }}
        50% {{ opacity: 0.4; }}
        100% {{ opacity: 1; }}
    }}

    /* ── Metric Cards ──────────────────────────────────── */
    .metric-card {{
        background-color: transparent;
        border: none;
        border-bottom: 1px solid {BG_CARD_BORDER};
        border-radius: 0;
        padding: 8px 4px 10px 4px;
        text-align: left;
    }}
    .metric-value {{
        font-size: 20px;
        font-weight: 600;
        margin: 2px 0 0 0;
        font-family: 'Inter', sans-serif;
        font-variant-numeric: tabular-nums;
        letter-spacing: -0.01em;
    }}
    .metric-label {{
        font-size: 10px;
        color: {TEXT_MUTED};
        text-transform: uppercase;
        letter-spacing: 0.08em;
        font-weight: 500;
    }}
    .green {{ color: {GREEN_MUTED}; }}
    .red {{ color: {RED_MUTED}; }}
    .amber {{ color: {AMBER_MUTED}; }}
    .blue {{ color: {BLUE}; }}

    /* ── Section Header ────────────────────────────────── */
    .section-header {{
        font-family: 'Inter', sans-serif;
        font-size: 10px;
        font-weight: 600;
        color: {TEXT_MUTED};
        text-transform: uppercase;
        letter-spacing: 0.12em;
        margin: 22px 0 8px 0;
        padding-bottom: 6px;
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
    .chain-table td.chain-theo {{
        color: {TEXT_SECONDARY};
        font-weight: 500;
    }}
    .chain-table th.chain-side-hdr {{
        font-size: 10px;
        letter-spacing: 0.15em;
        color: {TEXT_MUTED};
        background: rgba(255,255,255,0.015);
        border-bottom: 1px solid {BG_CARD_BORDER};
    }}
    .chain-table td.chain-strike-cell {{
        background: rgba(255,255,255,0.025);
        border-left: 1px solid {BG_CARD_BORDER};
        border-right: 1px solid {BG_CARD_BORDER};
    }}
    .bucket-table {{
        width: 100%;
    }}
    .bucket-table td.bucket-label {{
        text-align: left;
        font-weight: 600;
        color: {TEXT_PRIMARY};
        letter-spacing: 0.05em;
    }}
    .side-subhdr {{
        font-family: 'Inter', sans-serif;
        font-size: 9px;
        font-weight: 600;
        color: {TEXT_MUTED};
        text-transform: uppercase;
        letter-spacing: 0.14em;
        margin: 0 0 6px 2px;
    }}
    .empty-positions {{
        font-size: 12px;
        color: {TEXT_MUTED};
        padding: 8px 4px;
        font-style: italic;
    }}
    .chain-table tr.atm-row {{
        background: rgba(99, 102, 241, 0.08);
        border-left: 3px solid {ACCENT};
    }}
    .chain-table tr.atm-row:hover {{
        background: rgba(99, 102, 241, 0.14);
    }}
    .chain-strike {{
        font-weight: 400;
        color: {TEXT_PRIMARY};
        font-size: 13px;
    }}
    .chain-our-price {{
        font-weight: 500;
    }}
    .chain-our-price.quoting {{
        color: {GREEN_MUTED};
    }}
    .chain-our-price.pending {{
        color: {AMBER_MUTED};
    }}
    .chain-our-price.behind {{
        color: {RED_MUTED};
    }}
    .chain-our-price.idle {{
        color: {TEXT_MUTED};
    }}
    .chain-edge.positive {{
        color: {GREEN_MUTED};
        font-weight: 500;
    }}
    .chain-edge.negative {{
        color: {RED_MUTED};
        font-weight: 500;
    }}
    .chain-pos.long {{
        color: {GREEN_MUTED};
        font-weight: 500;
    }}
    .chain-pos.short {{
        color: {RED_MUTED};
        font-weight: 500;
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

# Auto-refresh — page-level refresh stays at 5s for the slow-changing
# sections (account, positions, risk buckets, header). The option chain
# itself runs in an st.fragment with its own 2s cadence (defined further
# down) so it updates faster without re-rendering the whole page.
if _HAS_AUTOREFRESH:
    st_autorefresh(interval=5000, limit=None, key="page_refresh")

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

# Latency pill (TTT / RTT) — pulled from the engine snapshot
def _fmt_us(us):
    if us is None:
        return "—"
    if us < 1000:
        return f"{us}μs"
    return f"{us/1000:.1f}ms"

latency_pill = ""
if snapshot is not None:
    lat = snapshot.get("latency") or {}
    ttt = (lat.get("ttt_us") or {})
    rtt = (lat.get("rtt_us") or {})
    ttt_p50 = _fmt_us(ttt.get("p50")); ttt_p99 = _fmt_us(ttt.get("p99"))
    rtt_p50 = _fmt_us(rtt.get("p50")); rtt_p99 = _fmt_us(rtt.get("p99"))
    latency_pill = (
        f'<span class="latency-pill" title="rolling p50/p99 — TTT samples: {ttt.get("n",0)}, RTT samples: {rtt.get("n",0)}">'
        f'<span class="lat-row"><span class="lat-key">TTT</span>{ttt_p50} / {ttt_p99}</span>'
        f'<span class="lat-row"><span class="lat-key">RTT</span>{rtt_p50} / {rtt_p99}</span>'
        f'</span>'
    )

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
        {latency_pill}
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

    # Color bands track the snapshot's `limits` block so the dashboard
    # adapts automatically to whatever stage we're in. Within constraint
    # → no extra color (default white text). Between constraint and kill
    # → amber. Beyond kill threshold → red.
    limits = snapshot.get("limits", {}) if snapshot else {}
    delta_ceiling = float(limits.get("delta_ceiling", 0) or 0)
    delta_kill = float(limits.get("delta_kill", 0) or 0)
    theta_floor = float(limits.get("theta_floor", 0) or 0)
    margin_ceiling = float(limits.get("margin_ceiling", 0) or 0)
    margin_kill = float(limits.get("margin_kill", 0) or 0)

    def _delta_color(d):
        if delta_ceiling <= 0:
            return ""
        if abs(d) <= delta_ceiling:
            return ""  # within constraint → default text color
        if delta_kill > 0 and abs(d) <= delta_kill:
            return "amber"
        return "red"

    def _theta_color(t):
        if theta_floor >= 0:
            return ""
        if t >= theta_floor:
            return ""  # within constraint → default text color
        return "red"  # below floor — no separate kill threshold for theta in spec

    def _margin_color(m):
        if margin_ceiling <= 0:
            return ""
        if m <= margin_ceiling:
            return ""  # within constraint → default text color
        if margin_kill > 0 and m <= margin_kill:
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
        mc = _margin_color(margin_val)
        cls = f" {mc}" if mc else ""
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Margin</div>
            <div class="metric-value{cls}">${margin_val:,.0f}</div>
        </div>
        """, unsafe_allow_html=True)

    with col3:
        dc = _delta_color(net_delta)
        cls = f" {dc}" if dc else ""
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Net Delta</div>
            <div class="metric-value{cls}">{net_delta:+.2f}</div>
        </div>
        """, unsafe_allow_html=True)

    with col4:
        tc = _theta_color(net_theta)
        cls = f" {tc}" if tc else ""
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Net Theta</div>
            <div class="metric-value{cls}">{net_theta:+,.0f}</div>
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

    # ── Per-side risk buckets and open positions, side by side ──
    calls_b = port.get("calls") or {}
    puts_b = port.get("puts") or {}

    def _bucket_table(b):
        d = b.get("delta", 0); t = b.get("theta", 0)
        m = b.get("margin", 0); g = b.get("gross", 0)
        dc = "green" if abs(d) <= 1.5 else ("amber" if abs(d) <= 2.0 else "red")
        tc = "green" if t >= -300 else ("amber" if t >= -400 else "red")
        return f"""
        <table class="chain-table bucket-table">
          <thead><tr>
            <th>Margin</th><th>Δ</th><th>Θ</th><th>Contracts</th>
          </tr></thead>
          <tbody>
            <tr>
              <td>${m:,.0f}</td>
              <td class="{dc}">{d:+.2f}</td>
              <td class="{tc}">{t:+,.0f}</td>
              <td>{g}</td>
            </tr>
          </tbody>
        </table>
        """

    def _positions_table(side_positions):
        if not side_positions:
            return '<div class="empty-positions">No open positions.</div>'
        rows = ""
        for p in sorted(side_positions, key=lambda x: (x["expiry"], x["strike"])):
            qty = p["qty"]
            qty_class = "pos-long" if qty > 0 else "pos-short"
            unr = p["unrealized_pnl"]
            unr_class = "positive" if unr >= 0 else "negative"
            rows += f"""<tr>
                <td>{int(p['strike'])}{p['right']}</td>
                <td class="{qty_class}">{qty:+d}</td>
                <td>${p['avg_price']:.2f}</td>
                <td>${p['mark']:.2f}</td>
                <td class="chain-edge {unr_class}">${unr:+,.0f}</td>
                <td>{p['delta']:+.3f}</td>
                <td>{p['theta']:+,.0f}</td>
            </tr>"""
        return f"""
        <table class="chain-table">
            <thead><tr>
                <th>Contract</th><th>Qty</th><th>Avg</th><th>Mark</th>
                <th>P&amp;L</th><th>&Delta;</th><th>&Theta;</th>
            </tr></thead>
            <tbody>{rows}</tbody>
        </table>
        """

    calls_positions = [p for p in positions if p.get("right") == "C"]
    puts_positions = [p for p in positions if p.get("right") == "P"]

    if calls_b or puts_b or positions:
        st.markdown('<div class="section-header">Risk by Side</div>', unsafe_allow_html=True)
        rcol1, rcol2 = st.columns(2)
        with rcol1:
            st.markdown('<div class="side-subhdr">Calls</div>', unsafe_allow_html=True)
            st.markdown(_bucket_table(calls_b), unsafe_allow_html=True)
        with rcol2:
            st.markdown('<div class="side-subhdr">Puts</div>', unsafe_allow_html=True)
            st.markdown(_bucket_table(puts_b), unsafe_allow_html=True)

        st.markdown('<div class="section-header">Open Positions</div>', unsafe_allow_html=True)
        pcol1, pcol2 = st.columns(2)
        with pcol1:
            st.markdown('<div class="side-subhdr">Calls</div>', unsafe_allow_html=True)
            st.markdown(_positions_table(calls_positions), unsafe_allow_html=True)
        with pcol2:
            st.markdown('<div class="side-subhdr">Puts</div>', unsafe_allow_html=True)
            st.markdown(_positions_table(puts_positions), unsafe_allow_html=True)

# ---------------------------------------------------------------------------
# Option Chain Table
# ---------------------------------------------------------------------------

st.markdown('<div class="section-header">Option Chain</div>', unsafe_allow_html=True)

@st.fragment(run_every=2)
def render_chain():
    """Chain table render. Lives in a fragment so only this section
    re-renders on its own 2s cadence — the rest of the page refreshes
    at 5s and doesn't flicker the table on every refresh."""
    snapshot = load_snapshot()
    if snapshot is not None and snapshot.get("strikes"):
        atm_strike = snapshot.get("atm_strike", 0)
        strikes = snapshot["strikes"]

        def _our_price_cell(price, live, mkt_ref, side):
            """Color-coded our_bid / our_ask cell.
              - None  → idle (gray dash)
              - live (Submitted) AND best-quote → quoting (green)
              - live but behind → behind (red)
              - has order but not yet Submitted → pending (amber)
            """
            if price is None:
                return '<span class="chain-our-price idle">-</span>'
            if live:
                if side == "BUY":
                    cls = "quoting" if (mkt_ref == 0 or price > mkt_ref) else "behind"
                else:
                    cls = "quoting" if (mkt_ref == 0 or price < mkt_ref) else "behind"
            else:
                cls = "pending"
            return f'<span class="chain-our-price {cls}">{price:.1f}</span>'

        def _fmt_side(s):
            """Render the half-row cells for one option type at one strike.

            Returns a tuple of HTML strings for:
              oi, vol, pos, our_bid, our_ask, theo, mkt_bid, mkt_ask, edge
            Cells are returned blank if the side block is None (no contract).
            """
            if s is None:
                blank = '<span style="color:#475569">—</span>'
                return (blank,) * 9
            mkt_bid = s.get("market_bid", 0) or 0
            mkt_ask = s.get("market_ask", 0) or 0
            our_bid = s.get("our_bid")
            our_ask = s.get("our_ask")
            bid_live = s.get("bid_live", False)
            ask_live = s.get("ask_live", False)
            theo = s.get("theo")
            pos = s.get("position", 0)
            volume = s.get("volume", 0) or 0
            oi = s.get("open_interest", 0) or 0

            our_bid_html = _our_price_cell(our_bid, bid_live, mkt_bid, "BUY")
            our_ask_html = _our_price_cell(our_ask, ask_live, mkt_ask, "SELL")

            theo_html = f"{theo:.1f}" if theo is not None else "-"

            # Edge per side vs theo. Bid edge = theo - our_bid (positive = we
            # buy below fair). Ask edge = our_ask - theo (positive = we sell
            # above fair). Average the two if both sides are quoting.
            edge_html = "-"
            if theo is not None:
                edges = []
                if our_bid is not None:
                    edges.append(theo - our_bid)
                if our_ask is not None:
                    edges.append(our_ask - theo)
                if edges:
                    avg = sum(edges) / len(edges)
                    cls = "positive" if avg > 0 else "negative"
                    edge_html = f'<span class="chain-edge {cls}">{avg:+.1f}</span>'

            if pos > 0:
                pos_html = f'<span class="chain-pos long">+{pos}</span>'
            elif pos < 0:
                pos_html = f'<span class="chain-pos short">{pos}</span>'
            else:
                pos_html = f'<span style="color:{TEXT_MUTED}">0</span>'

            mkt_bid_html = f"{mkt_bid:.1f}" if mkt_bid > 0 else "-"
            mkt_ask_html = f"{mkt_ask:.1f}" if mkt_ask > 0 else "-"
            return (f"{oi:,}", f"{volume:,}", pos_html, our_bid_html, our_ask_html,
                    theo_html, mkt_bid_html, mkt_ask_html, edge_html)

        rows_html = ""
        for strike_str in sorted(strikes.keys(), key=lambda s: float(s)):
            block = strikes[strike_str]
            # Backward compat: older snapshots stored a flat dict per strike.
            if "call" not in block and "put" not in block:
                call = block
                put = None
            else:
                call = block.get("call")
                put = block.get("put")

            strike_val = float(strike_str)
            is_atm = abs(strike_val - atm_strike) < 1.0
            row_class = ' class="atm-row"' if is_atm else ""

            (c_oi, c_vol, c_pos, c_our_bid, c_our_ask,
             c_theo, c_mkt_bid, c_mkt_ask, c_edge) = _fmt_side(call)
            (p_oi, p_vol, p_pos, p_our_bid, p_our_ask,
             p_theo, p_mkt_bid, p_mkt_ask, p_edge) = _fmt_side(put)

            rows_html += f"""<tr{row_class}>
                <td class="center">{c_oi}</td>
                <td class="center">{c_vol}</td>
                <td class="center">{c_pos}</td>
                <td>{c_mkt_bid}</td>
                <td>{c_our_bid}</td>
                <td class="chain-theo">{c_theo}</td>
                <td>{c_our_ask}</td>
                <td>{c_mkt_ask}</td>
                <td>{c_edge}</td>
                <td class="center chain-strike-cell"><span class="chain-strike">{int(strike_val)}</span></td>
                <td>{p_edge}</td>
                <td>{p_mkt_bid}</td>
                <td>{p_our_bid}</td>
                <td class="chain-theo">{p_theo}</td>
                <td>{p_our_ask}</td>
                <td>{p_mkt_ask}</td>
                <td class="center">{p_pos}</td>
                <td class="center">{p_vol}</td>
                <td class="center">{p_oi}</td>
            </tr>"""

        chain_html = f"""
        <div id="chain-scroll" style="max-height: 820px; overflow-y: auto; border: 1px solid {BG_CARD_BORDER}; border-radius: 8px;">
        <table class="chain-table">
            <thead>
                <tr>
                    <th colspan="9" class="center chain-side-hdr">CALLS</th>
                    <th class="center"></th>
                    <th colspan="9" class="center chain-side-hdr">PUTS</th>
                </tr>
                <tr>
                    <th class="center">OI</th>
                    <th class="center">Vol</th>
                    <th class="center">Pos</th>
                    <th>Mkt Bid</th>
                    <th>Our Bid</th>
                    <th>Theo</th>
                    <th>Our Ask</th>
                    <th>Mkt Ask</th>
                    <th>Edge</th>
                    <th class="center">Strike</th>
                    <th>Edge</th>
                    <th>Mkt Bid</th>
                    <th>Our Bid</th>
                    <th>Theo</th>
                    <th>Our Ask</th>
                    <th>Mkt Ask</th>
                    <th class="center">Pos</th>
                    <th class="center">Vol</th>
                    <th class="center">OI</th>
                </tr>
            </thead>
            <tbody>
                {rows_html}
            </tbody>
        </table>
        </div>
        """
        st.markdown(chain_html, unsafe_allow_html=True)
        # Streamlit strips <script> tags from st.markdown HTML for security, so
        # we inject the auto-scroll JS through a tiny components.html iframe.
        # The script reaches into window.parent.document to find the ATM row in
        # the main page and centers it in its scrollable container. Re-runs on
        # every Streamlit refresh because the components iframe is re-mounted.
        components.html(
            """
            <script>
            (function() {
                const doc = window.parent.document;
                const scroller = doc.getElementById('chain-scroll');
                if (!scroller) return;
                const atm = scroller.querySelector('tr.atm-row');
                if (!atm) return;
                const offset = atm.offsetTop - (scroller.clientHeight / 2) + (atm.offsetHeight / 2);
                scroller.scrollTop = Math.max(0, offset);
            })();
            </script>
            """,
            height=0,
        )
    else:
        st.info("No option chain data available.")


render_chain()

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
