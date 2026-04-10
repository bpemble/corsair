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

# ---------------------------------------------------------------------------
# Theme
# ---------------------------------------------------------------------------

sys.path.insert(0, str(_SCRIPT_DIR))
from dashboard_style import CUSTOM_CSS, BG_CARD_BORDER, TEXT_MUTED  # noqa: E402
from dashboard_chain import build_chain_html  # noqa: E402

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
# Page config
# ---------------------------------------------------------------------------

st.set_page_config(
    page_title="Corsair v2",
    layout="wide",
    initial_sidebar_state="collapsed",
)

st.markdown(CUSTOM_CSS, unsafe_allow_html=True)

# Auto-refresh — page-level rerun every 500ms. Combined with the chain
# fragment's faster cadence (defined further down) and corsair writing the
# snapshot at 4Hz, this keeps visible state ≤500ms stale. Single-user
# dashboard so the rerun rate is well within Streamlit's comfort zone.
if _HAS_AUTOREFRESH:
    st_autorefresh(interval=500, limit=None, key="page_refresh")

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


# ---------------------------------------------------------------------------
# Load data
# ---------------------------------------------------------------------------

snapshot = load_snapshot()

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

# Latency pill (TTT / RTT / AMEND) — pulled from the engine snapshot
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
    # RTT here is the steady-state metric: amend (modify→ack) round-trip,
    # which dominates once a quote is resting. The engine also tracks
    # place RTT (placeOrder→ack) under `place_rtt_us` for debugging, but
    # it's not surfaced on the dashboard — places happen rarely (only on
    # GTD expiry or after a fill) and have a structurally higher floor
    # because IBKR runs full validation/routing on a fresh order.
    rtt = (lat.get("amend_us") or {})
    ttt_p50 = _fmt_us(ttt.get("p50")); ttt_p99 = _fmt_us(ttt.get("p99"))
    rtt_p50 = _fmt_us(rtt.get("p50")); rtt_p99 = _fmt_us(rtt.get("p99"))
    latency_pill = (
        f'<span class="latency-pill" title="rolling p50/p99 — TTT (tick→placeOrder) samples: {ttt.get("n",0)}, RTT (modify→ack) samples: {rtt.get("n",0)}">'
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
    spread_capture_mid = port.get("spread_capture_mid", 0)

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
        mid_color = "green" if spread_capture_mid >= 0 else "red"
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Spread Capture</div>
            <div class="metric-value {sc_color}">${spread_capture:,.0f}<span class="{mid_color}" style="font-size:0.55em;opacity:0.7;font-weight:normal;margin-left:0.3em;">(${spread_capture_mid:,.0f})</span></div>
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
            exp = p.get('expiry', '')
            exp_label = f"{exp[4:6]}/{exp[6:8]}" if len(exp) == 8 else exp
            rows += f"""<tr>
                <td>{int(p['strike'])}{p['right']}</td>
                <td>{exp_label}</td>
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
                <th>Contract</th><th>Exp</th><th>Qty</th><th>Avg</th><th>Mark</th>
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

# Expiry selector (rendered outside the fragment so the selectbox survives
# chain refreshes). Reads one snapshot to get the list; the fragment then
# reads its own snapshot each tick and dispatches to the selected expiry.
_sel_snap = load_snapshot() or {}
_expiries = _sel_snap.get("expiries") or []
_front = _sel_snap.get("front_month_expiry")
if _expiries:
    _default_idx = _expiries.index(_front) if _front in _expiries else 0
    # Constrain the selectbox to ~1/6 page width so it sits as a compact
    # control instead of stretching across the chain-table header.
    _sel_col, _ = st.columns([1, 5])
    with _sel_col:
        _selected = st.selectbox(
            "Expiry", options=_expiries, index=_default_idx,
            key="selected_expiry",
        )
else:
    _selected = _front
    st.session_state["selected_expiry"] = _front

@st.fragment(run_every=0.25)
def render_chain():
    """Chain table render. Lives in a fragment so only this section
    re-renders on its own 4Hz cadence — matches the snapshot write rate
    so we catch every fresh state without re-rendering the whole page."""
    snapshot = load_snapshot()
    sel = st.session_state.get("selected_expiry")
    chain_html = build_chain_html(snapshot, selected_expiry=sel)
    if chain_html is not None:
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
