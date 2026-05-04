"""Dashboard theme: color constants + the full CSS block.

Kept separate from dashboard.py so the dashboard module is just layout
and data binding, not 350 lines of inline stylesheet.
"""

# ── Colors ─────────────────────────────────────────────────────────
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
        /* Vertical padding tuned to match the two-row latency-pill below
           — both pills sit in the same header row and need to line up. */
        padding: 8px 12px;
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
    .status-dot.live {{ background: {GREEN}; animation: pulse 2s infinite; }}
    .status-dot.stale {{ background: {AMBER}; }}
    .status-dot.offline {{ background: {RED}; }}
    .status-label {{ font-size: 13px; font-weight: 500; }}
    .status-label.live {{ color: {GREEN}; }}
    .status-label.stale {{ color: {AMBER}; }}
    .status-label.offline {{ color: {RED}; }}
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
    .latency-pill .lat-row {{ display: inline-flex; gap: 8px; }}
    .latency-pill .lat-key {{ opacity: 0.7; }}

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
    .chain-scroll {{
        max-height: 820px;
        overflow-y: auto;
        border: 1px solid {BG_CARD_BORDER};
        border-radius: 8px;
    }}
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
    .chain-table th.left {{ text-align: left; }}
    .chain-table th.center {{ text-align: center; }}
    .chain-table td {{
        padding: 6px 10px;
        text-align: right;
        border-bottom: 1px solid rgba(30, 41, 59, 0.5);
        color: {TEXT_PRIMARY};
    }}
    .chain-table td.left {{ text-align: left; }}
    .chain-table td.center {{ text-align: center; }}
    .chain-table tr:hover {{ background: rgba(99, 102, 241, 0.06); }}
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
    .bucket-table {{ width: 100%; }}
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
    .chain-table tr.atm-row:hover {{ background: rgba(99, 102, 241, 0.14); }}
    .chain-strike {{
        font-weight: 400;
        color: {TEXT_PRIMARY};
        font-size: 13px;
    }}
    .chain-our-price {{ font-weight: 500; }}
    .chain-our-price.quoting {{ color: {GREEN_MUTED}; }}
    .chain-our-price.pending {{ color: {AMBER_MUTED}; }}
    .chain-our-price.behind {{ color: {RED_MUTED}; }}
    .chain-our-price.idle {{ color: {TEXT_MUTED}; }}
    .chain-edge.positive {{ color: {GREEN_MUTED}; font-weight: 500; }}
    .chain-edge.negative {{ color: {RED_MUTED}; font-weight: 500; }}
    .chain-pos.long {{ color: {GREEN_MUTED}; font-weight: 500; }}
    .chain-pos.short {{ color: {RED_MUTED}; font-weight: 500; }}
    .chain-pos.zero {{ color: {TEXT_MUTED}; }}
    .chain-blank {{ color: #475569; }}
    .chain-status {{
        font-size: 11px;
        padding: 2px 8px;
        border-radius: 10px;
        display: inline-block;
    }}
    .chain-status.quoting {{ background: rgba(34,197,94,0.15); color: {GREEN}; }}
    .chain-status.pending {{ background: rgba(245,158,11,0.15); color: {AMBER}; }}
    .chain-status.idle {{ background: rgba(100,116,139,0.15); color: {TEXT_MUTED}; }}

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
