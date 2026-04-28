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

# Discover product snapshots via filename convention. Each product writes
# data/<name>_chain_snapshot.json (see src/config.py:88). Dashboard shows
# one tab per discovered file. ETH was wound down 2026-04-18; if re-enabled
# later, update its snapshot_path override in corsair_v2_config.yaml so the
# file matches the convention.
def _discover_products():
    """Return list of (name, snapshot_path) tuples for all products."""
    products = []
    data_dir = PROJECT_ROOT / "data"
    for p in sorted(data_dir.glob("*_chain_snapshot.json")):
        # Derive name from filename: hg_chain_snapshot.json -> HG
        name = p.stem.replace("_chain_snapshot", "").upper()
        products.append((name, str(p)))
    return products

PRODUCTS = _discover_products()

# Fallback snapshot path for callers that have no explicit product in scope.
# Defaults to the first discovered product's file; when no products are
# present (no corsair run yet on this host) falls back to a non-existent
# path so load_snapshot() returns None cleanly.
SNAPSHOT_PATH = (PRODUCTS[0][1] if PRODUCTS
                 else str(PROJECT_ROOT / "data" / "chain_snapshot.json"))

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
def load_snapshot(path=None):
    """Load chain snapshot JSON."""
    p = path or SNAPSHOT_PATH
    if not os.path.exists(p):
        return None
    try:
        with open(p, "r") as f:
            return json.load(f)
    except (json.JSONDecodeError, IOError):
        return None


# ---------------------------------------------------------------------------
# Product selector — single source of truth for which product's view drives
# every section below (header metrics, risk by side, open positions, chain).
# Account fields are shared across products (single IBKR sub-account) so the
# choice doesn't affect those — but per-product aggregates (delta/theta/vega,
# per-side margin, position list) and the chain table all follow this dropdown.
#
# Resolved here from session_state so the snapshot can load BEFORE the header
# renders (the status pill depends on the snapshot timestamp). The actual
# selectbox widget is rendered visually further down, between the header and
# the key-metrics row — Streamlit reruns the entire script when the user
# changes the value, and on that rerun st.session_state["selected_product"]
# already carries the new selection before this point executes.
# ---------------------------------------------------------------------------

_product_names = [p[0] for p in PRODUCTS]
_product_paths = {p[0]: p[1] for p in PRODUCTS}

_selected_product = st.session_state.get(
    "selected_product", _product_names[0] if _product_names else "HG",
)
_active_snap_path = _product_paths.get(_selected_product, SNAPSHOT_PATH)

# ---------------------------------------------------------------------------
# Load data
# ---------------------------------------------------------------------------

# Selected product drives every section that reads `snapshot` below.
snapshot = load_snapshot(_active_snap_path)

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

# Product selector — visually positioned between the header and the
# Underlying / metrics row. Backed by st.session_state["selected_product"]
# which the top-of-script logic already consulted to load the right snapshot.
_psel_col, _ = st.columns([1, 8])
with _psel_col:
    st.selectbox(
        "Product", options=_product_names,
        index=(_product_names.index(_selected_product)
               if _selected_product in _product_names else 0),
        key="selected_product",
    )

# ---------------------------------------------------------------------------
# Key Metrics Row
# ---------------------------------------------------------------------------

if snapshot is not None:
    port = snapshot.get("portfolio", {})
    underlying = snapshot.get("underlying_price", 0)
    margin_val = port.get("margin", 0)
    # Headline Net Delta = effective (options + hedge). Per-side Calls/Puts
    # buckets below stay options-only since hedge isn't a Call or Put.
    # Falls back to options-only net_delta on snapshots without the field
    # (engine boots before the hedge subsystem registers).
    net_delta = port.get("effective_delta", port.get("net_delta", 0))
    net_theta = port.get("net_theta", 0)
    net_vega = port.get("net_vega", 0)
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

    col1, col2, col3, col4, col5, col6, col7 = st.columns(7)

    # Stale forward detection: weekend / CME maintenance windows freeze the
    # underlying tick stream, leaving daily P&L marked against a dead price.
    # 30s is comfortably above normal tick cadence (sub-second in active
    # hours) and below the shortest expected feed gap.
    forward_age = float(snapshot.get("underlying_age_s", 0) or 0)
    marks_stale = forward_age > 30.0
    stale_label = (f' <span class="amber" style="font-size:0.5em;font-weight:normal;">'
                   f'STALE {forward_age:.0f}s</span>') if marks_stale else ""
    underlying_cls = " amber" if marks_stale else ""

    with col1:
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Underlying</div>
            <div class="metric-value{underlying_cls}">${underlying:,.4f}{stale_label}</div>
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
        # Vega: §13 disabled vega_kill (threshold characterization showed it
        # bound on 67-88% of fills as a choke). With no gate, no color band —
        # we display it for operator visibility since it's now the largest
        # unhedged risk vector.
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Net Vega</div>
            <div class="metric-value">{net_vega:+,.0f}</div>
        </div>
        """, unsafe_allow_html=True)

    with col6:
        st.markdown(f"""
        <div class="metric-card">
            <div class="metric-label">Fills Today</div>
            <div class="metric-value">{fills_today}</div>
        </div>
        """, unsafe_allow_html=True)

    with col7:
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

    a1, a2, a3, a4, a5, a6, a7 = st.columns(7)
    # Unrealized P&L marks against `underlying_price` and option mids — both
    # freeze when the underlying tick stream stalls. Render in amber + STALE
    # tag instead of green/red so a +$30K phantom doesn't read as a real win.
    unr = acct.get("unrealized_pnl", 0)
    unr_label = f"${unr:,.0f}" + (" <span style='font-size:0.55em;opacity:0.7;font-weight:normal;'>STALE</span>"
                                  if marks_stale else "")
    unr_color = "amber" if marks_stale else ("green" if unr >= 0 else "red")
    # Today's P&L = NLV − NLV-at-session-open (real intraday move). Honest
    # number for "how much did we make today"; differs from Unrealized +
    # Realized, which are anchored to fill price / session-cumulative and
    # don't isolate today's move on already-open positions. None until the
    # first NLV read after the 17:00 CT rollover sets the anchor.
    daily_nlv = acct.get("daily_pnl_nlv")
    if daily_nlv is None:
        daily_label = "—"
        daily_color = None
    else:
        daily_label = f"${daily_nlv:+,.0f}"
        daily_color = "green" if daily_nlv >= 0 else "red"
    cells = [
        ("Account",       acct.get("account_id", "—"),                None),
        ("Cash",          f"${acct.get('cash', 0):,.0f}",              None),
        ("Net Liq",       f"${acct.get('net_liq', 0):,.0f}",           None),
        ("Maint Margin",  f"${acct.get('maint_margin', 0):,.0f}",      None),
        ("Today's P&L",   daily_label,                                 daily_color),
        ("Unrealized P&L", unr_label,                                  unr_color),
        ("Realized P&L",  f"${acct.get('realized_pnl', 0):,.0f}",      "green" if acct.get("realized_pnl", 0) >= 0 else "red"),
    ]
    for col, (label, value, color) in zip([a1, a2, a3, a4, a5, a6, a7], cells):
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
        # Sort by product first so each product's rows cluster together.
        for p in sorted(side_positions,
                        key=lambda x: (x.get("product", ""), x["expiry"], x["strike"])):
            qty = p["qty"]
            qty_class = "pos-long" if qty > 0 else "pos-short"
            unr = p["unrealized_pnl"]
            unr_class = "positive" if unr >= 0 else "negative"
            exp = p.get('expiry', '')
            exp_label = f"{exp[4:6]}/{exp[6:8]}" if len(exp) == 8 else exp
            # Strike formatting: HG strikes are sub-dollar (e.g. 5.85)
            # so int() would truncate to 5; keep two decimals when needed.
            strike = p['strike']
            strike_label = (f"{strike:g}" if strike != int(strike)
                            else str(int(strike)))
            prod = p.get("product", "—")
            # Adaptive price precision: HG options trade at ~$0.04
            # (multiplier 25000 → $1,000/contract premium), where 2dp
            # rounds away the bid/ask spread entirely. Show 4dp when
            # below $1, 2dp otherwise — covers ETH (~$80) and HG
            # (~$0.04) without burying ETH in noise.
            avg = p['avg_price']; mark = p['mark']
            avg_fmt = f"{avg:.4f}" if abs(avg) < 1 else f"{avg:.2f}"
            mark_fmt = f"{mark:.4f}" if abs(mark) < 1 else f"{mark:.2f}"
            rows += f"""<tr>
                <td>{prod}</td>
                <td>{strike_label}{p['right']}</td>
                <td>{exp_label}</td>
                <td class="{qty_class}">{qty:+d}</td>
                <td>${avg_fmt}</td>
                <td>${mark_fmt}</td>
                <td class="chain-edge {unr_class}">${unr:+,.0f}</td>
                <td>{p['delta']:+.3f}</td>
                <td>{p['theta']:+,.0f}</td>
            </tr>"""
        return f"""
        <table class="chain-table">
            <thead><tr>
                <th>Product</th><th>Contract</th><th>Exp</th><th>Qty</th><th>Avg</th><th>Mark</th>
                <th>P&amp;L</th><th>&Delta;</th><th>&Theta;</th>
            </tr></thead>
            <tbody>{rows}</tbody>
        </table>
        """

    # Selected product drives this section too — the top-of-page dropdown
    # is the single source of truth. Switching to HG shows HG positions;
    # ETH shows ETH. Tag each row with the active product so the Product
    # column renders consistently with the chain table.
    for _p in positions:
        _p.setdefault("product", _selected_product)
    calls_positions = [p for p in positions if p.get("right") == "C"]
    puts_positions = [p for p in positions if p.get("right") == "P"]

    if calls_b or puts_b or positions:
        # Hedge tile — futures hedge state from HedgeManager. Surfaces
        # expiry, qty, avg entry, mark, and MTM P&L. Hidden when the
        # snapshot has no hedge block (older snapshots, or hedge
        # subsystem disabled).
        hedge_b = snapshot.get("hedge") if snapshot else None
        if hedge_b:
            qty = int(hedge_b.get("qty", 0))
            avg = float(hedge_b.get("avg_entry", 0) or 0)
            fwd = float(hedge_b.get("forward", 0) or 0)
            mtm = float(hedge_b.get("mtm_usd", 0) or 0)
            real = float(hedge_b.get("realized_pnl_usd", 0) or 0)
            exp = hedge_b.get("expiry", "") or ""
            exp_label = f"{exp[4:6]}/{exp[6:8]}" if len(exp) == 8 else (exp or "—")
            qty_class = "pos-long" if qty > 0 else ("pos-short" if qty < 0 else "")
            mtm_class = "positive" if mtm >= 0 else "negative"
            real_class = "positive" if real >= 0 else "negative"
            qty_label = "Flat" if qty == 0 else f"{qty:+d}"
            avg_label = f"${avg:.4f}" if qty != 0 else "—"
            mark_label = f"${fwd:.4f}" if fwd > 0 else "—"
            mtm_label = f"${mtm:+,.2f}" if qty != 0 else "$0"
            real_label = f"${real:+,.2f}"
            st.markdown('<div class="section-header">Hedge</div>', unsafe_allow_html=True)
            st.markdown(f"""
            <table class="chain-table bucket-table">
              <thead><tr>
                <th>Exp</th><th>Qty</th>
                <th>Avg Entry</th><th>Mark (F)</th>
                <th>Unrealized</th><th>Realized</th>
              </tr></thead>
              <tbody>
                <tr>
                  <td>{exp_label}</td>
                  <td class="{qty_class}">{qty_label}</td>
                  <td>{avg_label}</td>
                  <td>{mark_label}</td>
                  <td class="chain-edge {mtm_class}">{mtm_label}</td>
                  <td class="chain-edge {real_class}">{real_label}</td>
                </tr>
              </tbody>
            </table>
            """, unsafe_allow_html=True)

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

# Expiry selector (product is set by the top-of-page dropdown). Rendered
# outside the fragment so the selectbox survives chain refreshes. Reads
# the active product's snapshot to get the expiry list; the fragment then
# re-reads its own snapshot each tick.
_sel_snap = snapshot or {}
_expiries = _sel_snap.get("expiries") or []
_front = _sel_snap.get("front_month_expiry")

_sel_col, _ = st.columns([1, 5])
with _sel_col:
    if _expiries:
        _default_idx = _expiries.index(_front) if _front in _expiries else 0
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
    _prod = st.session_state.get("selected_product", "ETH")
    _path = _product_paths.get(_prod, SNAPSHOT_PATH)
    snapshot = load_snapshot(_path)
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
