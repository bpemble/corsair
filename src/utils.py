"""Utility functions for Corsair v2."""

import logging
import os
from datetime import datetime, date


def setup_logging(log_config) -> None:
    """Configure logging from config."""
    log_dir = getattr(log_config, "log_dir", "./logs")
    os.makedirs(log_dir, exist_ok=True)

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
        handlers=[
            logging.StreamHandler(),
            logging.FileHandler(os.path.join(log_dir, "corsair_v2.log")),
        ],
    )

    # Silence noisy ib_insync internals. At INFO they emit a Position dump
    # for every account update, a full TradeLogEntry replay on every order
    # status change, and per-tick info that filled corsair_v2.log to 523 GB
    # in a couple of days. WARNING is loud enough to catch real problems
    # (decode failures, disconnect events) without the steady-state firehose.
    # Our own src.* loggers stay at INFO.
    for noisy in ("ib_insync.wrapper", "ib_insync.ib", "ib_insync.client",
                  "ib_insync.Decoder"):
        logging.getLogger(noisy).setLevel(logging.WARNING)


def days_to_expiry(expiry: str) -> int:
    """Return calendar days from today to expiry (YYYYMMDD string)."""
    exp_date = datetime.strptime(expiry, "%Y%m%d").date()
    return (exp_date - date.today()).days


def time_to_expiry_years(expiry: str) -> float:
    """Return time to expiry in years (calendar days / 365)."""
    return max(days_to_expiry(expiry), 0) / 365.0


def round_to_tick(price: float, tick_size: float) -> float:
    """Round a price to the nearest valid tick."""
    return round(round(price / tick_size) * tick_size, 10)
