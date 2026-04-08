"""CSV logging for Corsair v2: fills, quotes, risk snapshots."""

import csv
import logging
import os
from datetime import datetime

logger = logging.getLogger(__name__)


class CSVLogger:
    """Manages CSV log files for fills, rejections, and risk snapshots."""

    def __init__(self, config):
        self.log_dir = config.logging.log_dir
        os.makedirs(self.log_dir, exist_ok=True)

        self._fill_path = os.path.join(self.log_dir, "fills.csv")
        self._risk_path = os.path.join(self.log_dir, "risk_snapshots.csv")
        self._quote_path = os.path.join(self.log_dir, "quotes.csv")
        self._trade_path = os.path.join(self.log_dir, "trades.csv")

        self._init_files()

    def _init_files(self):
        """Create CSV files with headers if they don't exist."""
        if not os.path.exists(self._fill_path):
            self._write_header(self._fill_path, [
                "timestamp", "strike", "expiry", "put_call", "side", "quantity",
                "fill_price", "spread_captured_theo", "spread_captured_mid",
                "margin_after", "delta_after",
                "theta_after", "vega_after", "fills_today",
                "cumulative_spread_theo", "cumulative_spread_mid",
                "fill_latency_ms",
            ])

        if not os.path.exists(self._quote_path):
            self._write_header(self._quote_path, [
                "timestamp", "strike", "side", "our_price", "incumbent_price",
                "incumbent_level", "incumbent_size", "incumbent_age_ms",
                "bbo_width", "skip_reason", "theo", "put_call",
            ])

        if not os.path.exists(self._trade_path):
            self._write_header(self._trade_path, [
                "timestamp", "strike", "put_call", "last_price", "last_size",
                "trades_in_burst",  # how many prints rolled into this callback
                "mkt_bid", "mkt_ask",
                "our_bid", "our_ask",  # our resting prices at print time (or empty)
                "our_bid_live", "our_ask_live",
                "theo",
                "side_inferred",  # "buy" / "sell" / "" via Lee-Ready price-vs-mid
            ])

        if not os.path.exists(self._risk_path):
            self._write_header(self._risk_path, [
                "timestamp", "underlying_price", "margin_used", "margin_pct",
                "net_delta", "net_theta", "net_vega", "long_count", "short_count",
                "gross_positions", "unrealized_pnl", "daily_spread_capture",
            ])

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

