"""CSV logging for Corsair v2: fills, quotes, risk snapshots, calibrations, margin scale.

Writes are async: callers (running on the event loop) enqueue rows to an
in-memory queue and return immediately. A background thread drains the
queue in batches, amortizing open/close syscalls across many rows. Before
this, synchronous per-row open/write/close was a measurable drag on TTT
— at ~3000 quote decisions/sec, the three syscalls per row landed in the
critical path between tick arrival and placeOrder.
"""

import csv
import json
import logging
import os
import queue
import threading
from datetime import date, datetime, timezone

logger = logging.getLogger(__name__)

# Batch drain window for the writer thread. 100ms is long enough to
# coalesce many rows into one open/close but short enough that on crash
# the data loss window is bounded.
_BATCH_DRAIN_SEC = 0.1
# Max rows per open/close. At ~3000 rows/sec across all files, 500
# gives us ~6 opens/sec instead of ~3000.
_BATCH_MAX_ROWS = 500
# Queue sentinel signaling the worker to exit after draining.
_SHUTDOWN = object()


# Centralized schema: one source of truth so the rotation logic can compare
# on-disk headers against the current code and migrate stale files safely.
QUOTE_HEADER = [
    "timestamp", "strike", "side", "our_price", "incumbent_price",
    "incumbent_level", "incumbent_size", "incumbent_age_ms",
    "bbo_width", "skip_reason", "theo", "put_call",
]

FILL_HEADER = [
    "timestamp", "strike", "expiry", "put_call", "side", "quantity",
    "fill_price", "spread_captured_theo", "spread_captured_mid",
    "margin_after", "delta_after",
    "theta_after", "vega_after", "fills_today",
    "cumulative_spread_theo", "cumulative_spread_mid",
    "fill_latency_ms", "underlying_price",
]

TRADE_HEADER = [
    "timestamp", "strike", "put_call", "last_price", "last_size",
    "trades_in_burst",
    "mkt_bid", "mkt_ask",
    "our_bid", "our_ask",
    "our_bid_live", "our_ask_live",
    "theo",
    "side_inferred",
]

RISK_HEADER = [
    "timestamp", "underlying_price", "margin_used", "margin_pct",
    "net_delta", "net_theta", "net_vega", "long_count", "short_count",
    "gross_positions", "unrealized_pnl", "daily_spread_capture",
]

# SABR/SVI fit telemetry — one row per accepted fit (SABRSurface.calibrate).
CALIBRATION_HEADER = [
    "timestamp", "expiry", "side", "model",
    "forward", "tte_years", "n_points", "rmse",
    # SABR params (empty when model=svi)
    "alpha", "beta", "rho_sabr", "nu",
    # SVI params (empty when model=sabr)
    "a", "b", "rho_svi", "m", "sigma",
]

# IBKR margin reconciliation — one row per update_cached_margin() tick.
MARGIN_SCALE_HEADER = [
    "timestamp", "raw_synthetic", "ibkr_actual", "ratio",
    "ibkr_scale", "clamped",
]


class CSVLogger:
    """Manages CSV log files for fills, quotes, trades, risk snapshots,
    calibrations, and margin-scale reconciliations.

    Rotation policy:
      - Header drift: if an existing file's first line differs from the
        current schema, rotate it aside (to ``<name>.stale-<mtime>.csv``)
        and create a fresh one. Fixes the silent column-misalignment bug
        that was corrupting ``quotes.csv`` after we added new columns.
      - Daily rotation for ``quotes.csv`` only: the file can grow to
        several GB per session so we cut it at the local-date boundary
        (``quotes.<YYYY-MM-DD>.csv``). Other CSVs are low-volume and
        don't need rotation.
    """

    def __init__(self, config):
        self.log_dir = config.logging.log_dir
        os.makedirs(self.log_dir, exist_ok=True)

        # Paper-trading JSONL log dir (corsair→crowsnest interface, spec
        # hg_spec_v1.3.md §17). Sibling of log_dir so logs/ stays for
        # operational CSVs and logs-paper/ is the stable external interface.
        self._paper_log_dir = os.path.join(
            os.path.dirname(os.path.abspath(self.log_dir)), "logs-paper")
        os.makedirs(self._paper_log_dir, exist_ok=True)

        self._fill_path = os.path.join(self.log_dir, "fills.csv")
        self._risk_path = os.path.join(self.log_dir, "risk_snapshots.csv")
        self._quote_path = os.path.join(self.log_dir, "quotes.csv")
        self._trade_path = os.path.join(self.log_dir, "trades.csv")
        self._calibration_path = os.path.join(self.log_dir, "calibrations.csv")
        self._margin_scale_path = os.path.join(self.log_dir, "margin_scale.csv")

        # Tracks the local date of the currently-open quotes.csv file so
        # we can rotate at midnight without re-stat'ing on every append.
        self._quote_date: date = date.today()

        self._init_files()

        # Async write queue + worker. Bounded at 100K rows to prevent
        # runaway memory if the writer thread stalls. On overflow we drop
        # the oldest row and log — losing noisy quote telemetry is fine;
        # blocking the event loop is not.
        self._write_queue: queue.Queue[tuple[str, list]] = queue.Queue(maxsize=100_000)
        self._dropped_rows = 0
        self._writer_thread = threading.Thread(
            target=self._writer_loop, name="csv-writer", daemon=True,
        )
        self._writer_thread.start()

    def _init_files(self):
        """Create CSV files with headers if they don't exist. Rotate any
        file whose existing header doesn't match the current schema."""
        for path, header in [
            (self._fill_path, FILL_HEADER),
            (self._quote_path, QUOTE_HEADER),
            (self._trade_path, TRADE_HEADER),
            (self._risk_path, RISK_HEADER),
            (self._calibration_path, CALIBRATION_HEADER),
            (self._margin_scale_path, MARGIN_SCALE_HEADER),
        ]:
            self._ensure_schema(path, header)

        # Seed the rotation anchor off the *current* quotes.csv mtime so
        # that a session spanning midnight still rotates the file we just
        # inherited from the previous day (if any).
        try:
            mtime = os.path.getmtime(self._quote_path)
            self._quote_date = datetime.fromtimestamp(mtime).date()
        except OSError:
            self._quote_date = date.today()

    def _ensure_schema(self, path: str, header: list):
        """If ``path`` doesn't exist, create it with ``header``. If it
        exists but the first line doesn't match, rotate the stale file
        aside and create a fresh one with the current schema."""
        if not os.path.exists(path):
            self._write_header(path, header)
            return

        existing = self._read_header(path)
        if existing == header:
            return

        # Schema drift. Rename the stale file so the next open creates
        # a fresh header-matching file. Don't delete it — historical data
        # is still readable if you know the old schema.
        try:
            mtime = os.path.getmtime(path)
            tag = datetime.fromtimestamp(mtime).strftime("%Y%m%d-%H%M%S")
        except OSError:
            tag = datetime.now().strftime("%Y%m%d-%H%M%S")
        stale = f"{path}.stale-{tag}"
        try:
            os.rename(path, stale)
            logger.warning(
                "CSV schema drift on %s: existing %d cols != expected %d cols. "
                "Rotated to %s; creating fresh file.",
                os.path.basename(path),
                len(existing) if existing else 0,
                len(header),
                os.path.basename(stale),
            )
        except OSError as e:
            logger.error("Failed to rotate stale %s: %s — overwriting", path, e)
        self._write_header(path, header)

    def _read_header(self, path: str) -> list:
        """Read and return the first CSV row of ``path`` as a list of
        field names. Returns ``[]`` if the file is empty or unreadable."""
        try:
            with open(path, "r", newline="") as f:
                reader = csv.reader(f)
                return next(reader, [])
        except (OSError, StopIteration):
            return []

    def _maybe_rotate_quotes_for_date(self):
        """Daily rotation for ``quotes.csv``. If the local date has
        advanced since the file was opened, rename the current file to
        ``quotes.<YYYY-MM-DD>.csv`` (stamped with the *old* date) and
        let the next write recreate it with a fresh header."""
        today = date.today()
        if today == self._quote_date:
            return
        old_date = self._quote_date
        self._quote_date = today
        if not os.path.exists(self._quote_path):
            return
        dated_path = os.path.join(
            self.log_dir, f"quotes.{old_date.isoformat()}.csv")
        try:
            # If the dated path already exists (e.g. a previous session on
            # the same day already rotated), append a disambiguator.
            if os.path.exists(dated_path):
                tag = datetime.now().strftime("%H%M%S")
                dated_path = os.path.join(
                    self.log_dir, f"quotes.{old_date.isoformat()}.{tag}.csv")
            os.rename(self._quote_path, dated_path)
            logger.info("quotes.csv rotated to %s", os.path.basename(dated_path))
        except OSError as e:
            logger.warning("quotes.csv rotation failed: %s", e)
            return
        # Recreate with the current header so the next append lands in a
        # well-formed file.
        self._write_header(self._quote_path, QUOTE_HEADER)

    def _write_header(self, path: str, fields: list):
        with open(path, "w", newline="") as f:
            writer = csv.writer(f)
            writer.writerow(fields)

    def _append_row(self, path: str, row: list):
        """Enqueue a row for the background writer. Non-blocking: drops
        the row if the queue is full rather than blocking the event loop."""
        try:
            self._write_queue.put_nowait((path, row))
        except queue.Full:
            self._dropped_rows += 1
            # Log once per 1000 drops to avoid flooding stderr if the
            # writer thread wedges.
            if self._dropped_rows % 1000 == 1:
                logger.warning(
                    "CSV write queue full; dropped %d rows so far",
                    self._dropped_rows,
                )

    def _writer_loop(self):
        """Drain the write queue in batches, coalescing rows per-file so
        we open/close each target file at most once per batch."""
        while True:
            batch: list[tuple[str, list]] = []
            # Block on the first row so we don't busy-spin when idle.
            try:
                first = self._write_queue.get(timeout=1.0)
            except queue.Empty:
                continue
            if first is _SHUTDOWN:
                return
            batch.append(first)

            # Drain additional rows up to the batch limits.
            drain_deadline = None
            import time as _time
            while len(batch) < _BATCH_MAX_ROWS:
                if drain_deadline is None:
                    drain_deadline = _time.monotonic() + _BATCH_DRAIN_SEC
                remaining = drain_deadline - _time.monotonic()
                if remaining <= 0:
                    break
                try:
                    item = self._write_queue.get(timeout=min(remaining, 0.05))
                except queue.Empty:
                    break
                if item is _SHUTDOWN:
                    self._flush_batch(batch)
                    return
                batch.append(item)

            self._flush_batch(batch)

    def _flush_batch(self, batch: list[tuple[str, list]]):
        """Group rows by target file and write each group with a single
        open/close. Errors are logged but don't halt the writer.

        CSV paths receive lists (written via csv.writer); .jsonl paths
        receive pre-serialized JSON strings (written as lines).
        """
        by_path: dict = {}
        for path, row in batch:
            by_path.setdefault(path, []).append(row)
        for path, rows in by_path.items():
            try:
                with open(path, "a", newline="") as f:
                    if path.endswith(".jsonl"):
                        f.write("\n".join(rows) + "\n")
                    else:
                        writer = csv.writer(f)
                        writer.writerows(rows)
            except Exception as e:
                logger.warning("Failed to write %d rows to %s: %s",
                               len(rows), path, e)

    def shutdown(self, timeout: float = 5.0):
        """Flush outstanding rows and stop the writer thread. Safe to call
        multiple times; blocks up to *timeout* seconds."""
        try:
            self._write_queue.put_nowait(_SHUTDOWN)
        except queue.Full:
            # Queue is backed up — force shutdown by draining what we can.
            pass
        self._writer_thread.join(timeout=timeout)

    def log_paper_event(self, event: dict) -> None:
        """Legacy entry point — routes to the fills stream.

        Preserved for compatibility with fill_handler's existing call site
        (which emits event_type="fill" or "skip" into a single file).
        v1.4 §9.5 splits streams into separate files; callers should
        prefer the typed helpers below.
        """
        self.log_paper_stream("fills", event)

    def log_paper_stream(self, stream: str, event: dict) -> None:
        """Write ``event`` to ``logs-paper/<stream>-YYYY-MM-DD.jsonl``.

        Core primitive backing the v1.4 §9.5 eight-stream interface:
        fills, skips, kill_switch, reconnects, sabr_fits, margin_snapshots,
        pnl_snapshots, hedge_trades.

        The caller is responsible for populating the schema fields for
        each stream; this method only adds `timestamp_utc` if missing.
        Write is enqueued on the CSVLogger background thread — no blocking
        I/O on the event loop.
        """
        if "timestamp_utc" not in event:
            # datetime.utcnow() is deprecated in 3.12+; use an aware UTC
            # datetime and strip the timezone suffix for the ISO-8601 Z
            # suffix we emit downstream.
            now_utc = datetime.now(timezone.utc)
            event["timestamp_utc"] = (
                now_utc.replace(tzinfo=None).isoformat(timespec="milliseconds")
                + "Z"
            )
        path = os.path.join(
            self._paper_log_dir,
            f"{stream}-{date.today().isoformat()}.jsonl")
        try:
            line = json.dumps(event, separators=(",", ":"), default=str)
        except (TypeError, ValueError) as e:
            logger.warning("paper event serialization failed (%s): %s (event=%s)",
                           stream, e, event)
            return
        self._append_row(path, line)

    # ── Typed v1.4 paper-stream helpers (§9.5) ─────────────────────────
    def log_paper_skip(self, symbol: str, side: str, skip_reason: str,
                       strike: float = 0.0, cp: str = "",
                       theo: float = None, bbo_bid: float = None,
                       bbo_ask: float = None, forward: float = None,
                       expiry: str = "") -> None:
        """Record a skipped quote attempt. Emits one row per skip reason
        per (strike, right, side). Callers de-dup via the CSV quotes log;
        this stream is the crowsnest-facing summary."""
        self.log_paper_stream("skips", {
            "event_type": "skip",
            "symbol": symbol,
            "cp": cp,
            "strike": strike,
            "expiry": expiry,
            "side": side,
            "skip_reason": skip_reason,
            "theo": theo,
            "bbo_bid": bbo_bid,
            "bbo_ask": bbo_ask,
            "forward": forward,
        })

    def log_paper_kill_switch(self, switch_name: str, reason: str,
                              source: str, book_state: dict) -> None:
        """Record a kill switch trigger with full book snapshot per
        v1.4 §9.4. ``book_state`` should include positions, margin, pnl,
        delta, theta, vega, and the current forward."""
        self.log_paper_stream("kill_switch", {
            "event_type": "kill_switch_fire",
            "switch_name": switch_name,
            "reason": reason,
            "source": source,
            "book_state": book_state,
        })

    def log_paper_reconnect(self, event: str, duration_sec: float = None,
                            pending_orders_pre: int = None,
                            pending_orders_post: int = None,
                            detail: str = "") -> None:
        """Record a gateway disconnect/reconnect. ``event`` is one of
        "disconnect", "reconnect", "reconnect_failed"."""
        self.log_paper_stream("reconnects", {
            "event_type": event,
            "duration_sec": duration_sec,
            "pending_orders_pre": pending_orders_pre,
            "pending_orders_post": pending_orders_post,
            "detail": detail,
        })

    def log_paper_sabr_fit(self, expiry: str, side: str, model: str,
                           forward: float, tte_years: float,
                           n_points: int, rmse: float,
                           params: dict) -> None:
        """Record a SABR/SVI surface fit. ``params`` is the model-native
        parameter dict (e.g. {a,b,rho,m,sigma} for SVI)."""
        self.log_paper_stream("sabr_fits", {
            "event_type": "sabr_fit",
            "expiry": expiry,
            "side": side,
            "model": model,
            "forward": forward,
            "tte_years": tte_years,
            "n_points": n_points,
            "rmse": rmse,
            "params": params,
        })

    def log_paper_margin_snapshot(self, current_margin_usd: float,
                                   capital_usd: float,
                                   margin_pct: float,
                                   ibkr_reported: float = None,
                                   synthetic_raw: float = None,
                                   ibkr_scale: float = None) -> None:
        """Record a SPAN margin snapshot (v1.4 §9.5; 1-min cadence)."""
        self.log_paper_stream("margin_snapshots", {
            "event_type": "margin_snapshot",
            "current_margin_usd": round(float(current_margin_usd), 2),
            "capital_usd": float(capital_usd),
            "margin_pct": round(float(margin_pct), 4),
            "ibkr_reported_usd": ibkr_reported,
            "synthetic_raw_usd": synthetic_raw,
            "ibkr_scale": ibkr_scale,
        })

    def log_paper_pnl_snapshot(self, daily_pnl_usd: float,
                                realized_pnl_usd: float,
                                mtm_pnl_usd: float,
                                hedge_mtm_usd: float = 0.0,
                                capital_usd: float = 0.0,
                                halt_threshold_usd: float = 0.0,
                                positions_count: int = 0,
                                net_delta: float = 0.0,
                                net_theta: float = 0.0,
                                net_vega: float = 0.0,
                                forward: float = 0.0) -> None:
        """Record an intraday P&L snapshot (v1.4 §9.5; 30-sec cadence).
        The daily halt check fires on ``daily_pnl_usd`` (= realized + MTM
        + hedge_mtm) vs halt_threshold_usd."""
        self.log_paper_stream("pnl_snapshots", {
            "event_type": "pnl_snapshot",
            "daily_pnl_usd": round(float(daily_pnl_usd), 2),
            "realized_pnl_usd": round(float(realized_pnl_usd), 2),
            "mtm_pnl_usd": round(float(mtm_pnl_usd), 2),
            "hedge_mtm_usd": round(float(hedge_mtm_usd), 2),
            "capital_usd": float(capital_usd),
            "halt_threshold_usd": float(halt_threshold_usd),
            "positions_count": int(positions_count),
            "net_delta": round(float(net_delta), 3),
            "net_theta": round(float(net_theta), 2),
            "net_vega": round(float(net_vega), 2),
            "forward": float(forward),
        })

    def log_paper_hedge_trade(self, side: str, qty: int, price: float,
                              forward_at_trade: float,
                              net_delta_pre: float,
                              net_delta_post: float,
                              reason: str,
                              mode: str,
                              order_id: str = "") -> None:
        """Record a delta-hedge futures trade (v1.4 §5 / §9.5). In
        observe mode, this is the intent log — no actual order placed."""
        self.log_paper_stream("hedge_trades", {
            "event_type": "hedge_trade",
            "mode": mode,
            "side": side,
            "qty": int(qty),
            "price": float(price) if price is not None else None,
            "forward_at_trade": float(forward_at_trade),
            "net_delta_pre": round(float(net_delta_pre), 3),
            "net_delta_post": round(float(net_delta_post), 3),
            "reason": reason,
            "order_id": order_id,
        })

    def log_fill(self, strike, expiry, put_call, side, quantity, fill_price,
                 spread_captured_theo, spread_captured_mid,
                 margin_after, delta_after, theta_after, vega_after,
                 fills_today, cumulative_spread_theo, cumulative_spread_mid,
                 fill_latency_ms=None, underlying_price=None):
        self._append_row(self._fill_path, [
            datetime.now().isoformat(), strike, expiry, put_call, side,
            quantity, fill_price,
            f"{spread_captured_theo:.2f}", f"{spread_captured_mid:.2f}",
            f"{margin_after:.0f}", f"{delta_after:.3f}", f"{theta_after:.0f}",
            f"{vega_after:.0f}", fills_today,
            f"{cumulative_spread_theo:.0f}", f"{cumulative_spread_mid:.0f}",
            f"{fill_latency_ms:.0f}" if fill_latency_ms is not None else "",
            f"{underlying_price:.2f}" if underlying_price is not None else "",
        ])

    def log_trade(self, strike, put_call, last_price, last_size,
                  trades_in_burst, mkt_bid, mkt_ask,
                  our_bid, our_ask, our_bid_live, our_ask_live,
                  theo, side_inferred):
        """Append one row per detected trade print on an option contract.

        Captures market context AND our resting state at print time so we
        can later analyze: did this print happen at our quote? At what
        side? Did we miss a fillable opportunity? This is the raw data the
        capture-rate analysis runs on.
        """
        self._append_row(self._trade_path, [
            datetime.now().isoformat(), strike, put_call,
            f"{last_price:.2f}" if last_price is not None else "",
            last_size if last_size is not None else "",
            trades_in_burst,
            f"{mkt_bid:.2f}" if mkt_bid else "",
            f"{mkt_ask:.2f}" if mkt_ask else "",
            f"{our_bid:.2f}" if our_bid is not None else "",
            f"{our_ask:.2f}" if our_ask is not None else "",
            int(bool(our_bid_live)),
            int(bool(our_ask_live)),
            f"{theo:.2f}" if theo is not None else "",
            side_inferred or "",
        ])

    def log_quote(self, strike, side, our_price, incumbent_price,
                  incumbent_level, incumbent_size, incumbent_age_ms,
                  bbo_width, skip_reason="", theo=None, put_call="C"):
        # Cheap per-call rotation check — datetime.now() is already called
        # below for the timestamp field, so the extra .date() compare is
        # noise relative to the file I/O.
        self._maybe_rotate_quotes_for_date()
        self._append_row(self._quote_path, [
            datetime.now().isoformat(), strike, side,
            f"{our_price:.2f}" if our_price is not None else "",
            f"{incumbent_price:.2f}" if incumbent_price is not None else "",
            incumbent_level if incumbent_level is not None else "",
            incumbent_size if incumbent_size is not None else "",
            incumbent_age_ms if incumbent_age_ms is not None else "",
            f"{bbo_width:.2f}" if bbo_width is not None else "",
            skip_reason,
            f"{theo:.2f}" if theo is not None else "",
            put_call,
        ])

    def log_risk_snapshot(self, underlying_price, margin_used, margin_pct,
                          net_delta, net_theta, net_vega, long_count,
                          short_count, gross_positions, unrealized_pnl,
                          daily_spread_capture):
        self._append_row(self._risk_path, [
            datetime.now().isoformat(), f"{underlying_price:.2f}",
            f"{margin_used:.0f}", f"{margin_pct:.3f}",
            f"{net_delta:.3f}", f"{net_theta:.0f}", f"{net_vega:.0f}",
            long_count, short_count, gross_positions,
            f"{unrealized_pnl:.0f}", f"{daily_spread_capture:.0f}",
        ])

    def log_calibration(self, expiry: str, side: str, model: str,
                        forward: float, tte_years: float,
                        n_points: int, rmse: float,
                        alpha=None, beta=None, rho_sabr=None, nu=None,
                        a=None, b=None, rho_svi=None, m=None, sigma=None):
        """One row per accepted SABR/SVI fit.

        ``model`` is ``"sabr"`` or ``"svi"``. Unused param columns are
        written as empty strings so the two models share one file without
        forcing a join downstream.

        Also mirrors the fit into ``logs-paper/sabr_fits-YYYY-MM-DD.jsonl``
        per v1.4 §9.5 so reconciliation can consume structured params
        without parsing the fixed-width CSV.
        """
        def _fmt(v, prec=4):
            return f"{v:.{prec}f}" if v is not None else ""
        self._append_row(self._calibration_path, [
            datetime.now().isoformat(), expiry, side, model,
            _fmt(forward, 2), _fmt(tte_years, 5),
            n_points, _fmt(rmse, 5),
            _fmt(alpha), _fmt(beta), _fmt(rho_sabr), _fmt(nu),
            _fmt(a), _fmt(b), _fmt(rho_svi), _fmt(m), _fmt(sigma),
        ])
        # Paper JSONL mirror — model-native params only (drop null fields).
        if model == "svi":
            params = {"a": a, "b": b, "rho": rho_svi, "m": m, "sigma": sigma}
        else:
            params = {"alpha": alpha, "beta": beta, "rho": rho_sabr, "nu": nu}
        params = {k: v for k, v in params.items() if v is not None}
        try:
            self.log_paper_sabr_fit(
                expiry=expiry, side=side, model=model,
                forward=float(forward), tte_years=float(tte_years),
                n_points=int(n_points), rmse=float(rmse), params=params,
            )
        except Exception:
            pass

    def log_margin_scale(self, raw_synthetic: float, ibkr_actual: float,
                         ratio: float, ibkr_scale: float, clamped: bool):
        """One row per IBKR margin reconciliation tick. Lets us track
        scale drift and catch the moment synthetic and IBKR diverge."""
        self._append_row(self._margin_scale_path, [
            datetime.now().isoformat(),
            f"{raw_synthetic:.0f}", f"{ibkr_actual:.0f}",
            f"{ratio:.4f}", f"{ibkr_scale:.4f}",
            "1" if clamped else "0",
        ])
