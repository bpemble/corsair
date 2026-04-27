"""Utility functions for Corsair v2."""

import logging
import os
from datetime import datetime, date, timezone
from zoneinfo import ZoneInfo

_CME_TZ = ZoneInfo("America/Chicago")


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
    """Return calendar days from today to expiry (YYYYMMDD string).

    "Today" is the CME calendar date (America/Chicago) — `date.today()` in
    a UTC container rolls over six hours early and zeros out TTE for
    positions whose expiry is the upcoming CT business day, collapsing
    delta to intrinsic and theta to 0.
    """
    exp_date = datetime.strptime(expiry, "%Y%m%d").date()
    today_cme = datetime.now(_CME_TZ).date()
    return (exp_date - today_cme).days


def time_to_expiry_years(expiry: str) -> float:
    """Return time to expiry in years (calendar days / 365)."""
    return max(days_to_expiry(expiry), 0) / 365.0


def round_to_tick(price: float, tick_size: float) -> float:
    """Round a price to the nearest valid tick."""
    return round(round(price / tick_size) * tick_size, 10)


def floor_to_tick(price: float, tick_size: float) -> float:
    """Round a price DOWN to the nearest valid tick."""
    import math
    return math.floor(price / tick_size) * tick_size


def ceil_to_tick(price: float, tick_size: float) -> float:
    """Round a price UP to the nearest valid tick."""
    import math
    return math.ceil(price / tick_size) * tick_size


# CME month codes: F G H J K M N Q U V X Z (Jan through Dec).
_CME_MONTH_CODES = "FGHJKMNQUVXZ"


def format_hxe_symbol(expiry: str, put_call: str, strike: float) -> str:
    """Format HG option symbol per hg_spec_v1.3 §2.2: 'HXE[M][Y] [CP][strike×100]'.

    e.g. expiry='20260320', put_call='C', strike=4.85 → 'HXEH6 C485'.
    """
    exp_date = datetime.strptime(expiry, "%Y%m%d").date()
    month_code = _CME_MONTH_CODES[exp_date.month - 1]
    year_digit = exp_date.year % 10
    strike_cents = int(round(strike * 100))
    return f"HXE{month_code}{year_digit} {put_call}{strike_cents}"


def iso8601ms_utc() -> str:
    """Return current UTC time as ISO 8601 with millisecond precision and 'Z'.

    Spec: hg_spec_v1.3.md §17.1 ('ts' field format).
    """
    now = datetime.now(timezone.utc)
    return now.strftime("%Y-%m-%dT%H:%M:%S") + f".{now.microsecond // 1000:03d}Z"
