"""Option chain table HTML rendering for the Corsair v2 dashboard.

Pure-string HTML generation, no Streamlit imports. The dashboard module
calls build_chain_html(snapshot) and pipes the result through st.markdown.
All styling lives in dashboard_style.CSS — this module only emits class
names, never inline styles.
"""


def _our_price_cell(price, live, mkt_ref, raw_ref, side, pfmt="1"):
    """Color-coded our_bid / our_ask cell.
      - None  → idle (gray dash)
      - live (Submitted) AND at/better than BBO → quoting (green)
      - live but behind → behind (red)
      - has order but not yet Submitted → pending (amber)
    """
    if price is None:
        return '<span class="chain-our-price idle">-</span>'
    if live:
        # Compare against raw L1 BBO: if our price IS the best bid/ask,
        # we're leading. The clean "market" ref can equal our price when
        # the self-filter cache is stale, which would incorrectly show
        # red for a quote that's actually at the top of the book.
        at_bbo = (price >= raw_ref) if side == "BUY" else (price <= raw_ref)
        cls = "quoting" if (raw_ref == 0 or at_bbo) else "behind"
    else:
        cls = "pending"
    return f'<span class="chain-our-price {cls}">{price:.{pfmt}f}</span>'


def _fmt_side(s, pfmt="1"):
    """Render the half-row cells for one option type at one strike.

    *pfmt* controls price decimal places ("1" for ETH, "4" for HG).
    Returns (oi, vol, pos, our_bid, our_ask, theo, mkt_bid, mkt_ask, edge).
    Cells are returned blank if the side block is None (no contract).
    """
    if s is None:
        return ('<span class="chain-blank">—</span>',) * 9

    mkt_bid = s.get("market_bid", 0) or 0
    mkt_ask = s.get("market_ask", 0) or 0
    raw_bid = s.get("raw_bid", 0) or 0
    raw_ask = s.get("raw_ask", 0) or 0
    our_bid = s.get("our_bid")
    our_ask = s.get("our_ask")
    theo = s.get("theo")
    pos = s.get("position", 0)

    our_bid_html = _our_price_cell(our_bid, s.get("bid_live", False), mkt_bid, raw_bid, "BUY", pfmt=pfmt)
    our_ask_html = _our_price_cell(our_ask, s.get("ask_live", False), mkt_ask, raw_ask, "SELL", pfmt=pfmt)
    theo_html = f"{theo:.{pfmt}f}" if theo is not None else "-"

    # Edge per side vs theo. Bid edge = theo - our_bid (positive = we buy
    # below fair). Ask edge = our_ask - theo (positive = we sell above fair).
    # Average the two if both sides are quoting.
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
            edge_html = f'<span class="chain-edge {cls}">{avg:+.{pfmt}f}</span>'

    if pos > 0:
        pos_html = f'<span class="chain-pos long">+{pos}</span>'
    elif pos < 0:
        pos_html = f'<span class="chain-pos short">{pos}</span>'
    else:
        pos_html = '<span class="chain-pos zero">0</span>'

    # Fall back to raw L1 for observation-only strikes: get_clean_bbo only
    # populates market_bid/ask for strikes find_incumbent evaluates (quoted
    # set). Wings outside the quote window stream raw ticks but never fill
    # the clean cache — raw is safe to show there because we have no orders
    # to filter out on those strikes.
    disp_bid = mkt_bid if mkt_bid > 0 else raw_bid
    disp_ask = mkt_ask if mkt_ask > 0 else raw_ask
    mkt_bid_html = f"{disp_bid:.{pfmt}f}" if disp_bid > 0 else "-"
    mkt_ask_html = f"{disp_ask:.{pfmt}f}" if disp_ask > 0 else "-"
    return (
        f"{s.get('open_interest', 0) or 0:,}",
        f"{s.get('volume', 0) or 0:,}",
        pos_html, our_bid_html, our_ask_html,
        theo_html, mkt_bid_html, mkt_ask_html, edge_html,
    )


_HEADER_HTML = """
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
"""


def _build_row(strike_str, block, atm_strike, decimal=False):
    # Backward compat: older snapshots stored a flat dict per strike.
    if "call" not in block and "put" not in block:
        call, put = block, None
    else:
        call, put = block.get("call"), block.get("put")

    strike_val = float(strike_str)
    # ATM detection threshold: ~half a strike width (1.0 for ETH $25 grid,
    # 0.005 for HG $0.01 grid).
    atm_tol = 0.005 if decimal else 1.0
    row_class = ' class="atm-row"' if abs(strike_val - atm_strike) < atm_tol else ""
    strike_label = f"{strike_val:.2f}" if decimal else str(int(strike_val))
    # Price format: 4 decimals for small prices (HG), 1 for large (ETH)
    pfmt = "4" if decimal else "1"

    (c_oi, c_vol, c_pos, c_our_bid, c_our_ask,
     c_theo, c_mkt_bid, c_mkt_ask, c_edge) = _fmt_side(call, pfmt=pfmt)
    (p_oi, p_vol, p_pos, p_our_bid, p_our_ask,
     p_theo, p_mkt_bid, p_mkt_ask, p_edge) = _fmt_side(put, pfmt=pfmt)

    return f"""<tr{row_class}>
        <td class="center">{c_oi}</td>
        <td class="center">{c_vol}</td>
        <td class="center">{c_pos}</td>
        <td>{c_mkt_bid}</td>
        <td>{c_our_bid}</td>
        <td class="chain-theo">{c_theo}</td>
        <td>{c_our_ask}</td>
        <td>{c_mkt_ask}</td>
        <td>{c_edge}</td>
        <td class="center chain-strike-cell"><span class="chain-strike">{strike_label}</span></td>
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


def build_chain_html(snapshot, selected_expiry: str = None):
    """Return the full chain table HTML, or None if the snapshot has
    no strike data. Wrapper div uses .chain-scroll for styling.

    If ``selected_expiry`` is provided and present in snapshot["chains"],
    that expiry's chain is rendered. Otherwise falls back to the legacy
    top-level ``snapshot["strikes"]`` block (front month)."""
    if not snapshot:
        return None
    chains = snapshot.get("chains") or {}
    strikes = None
    if selected_expiry and selected_expiry in chains:
        strikes = chains[selected_expiry].get("strikes")
    if not strikes:
        strikes = snapshot.get("strikes")
    if not strikes:
        return None
    atm_strike = snapshot.get("atm_strike", 0)
    # Detect whether strikes are decimal (HG-style $5.90) vs integer
    # (ETH-style 2350) so we can pick the right format strings.
    _decimal_strikes = any("." in k for k in strikes)
    rows = "".join(
        _build_row(k, strikes[k], atm_strike, decimal=_decimal_strikes)
        for k in sorted(strikes.keys(), key=float)
    )
    return f'<div id="chain-scroll" class="chain-scroll">{_HEADER_HTML}{rows}</tbody></table></div>'
