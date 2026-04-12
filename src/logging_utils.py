"""CSV logging for Corsair v2: fills, quotes, risk snapshots, calibrations, margin scale."""

import csv
import logging
import os
from datetime import date, datetime

logger = logging.getLogger(__name__)


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
    "fill_latency_ms",
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
        try:
            with open(path, "a", newline="") as f:
                writer = csv.writer(f)
                writer.writerow(row)
        except Exception as e:
            logger.warning("Failed to write to %s: %s", path, e)

    def log_fill(self, strike, expiry, put_call, side, quantity, fill_price,
                 spread_captured_theo, spread_captured_mid,
                 margin_after, delta_after, theta_after, vega_after,
                 fills_today, cumulative_spread_theo, cumulative_spread_mid,
                 fill_latency_ms=None):
        self._append_row(self._fill_path, [
            datetime.now().isoformat(), strike, expiry, put_call, side,
            quantity, fill_price,
            f"{spread_captured_theo:.2f}", f"{spread_captured_mid:.2f}",
            f"{margin_after:.0f}", f"{delta_after:.3f}", f"{theta_after:.0f}",
            f"{vega_after:.0f}", fills_today,
            f"{cumulative_spread_theo:.0f}", f"{cumulative_spread_mid:.0f}",
            f"{fill_latency_ms:.0f}" if fill_latency_ms is not None else "",
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
