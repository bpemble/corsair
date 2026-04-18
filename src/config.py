"""Configuration loader for Corsair v2.

Loads a single YAML config file and provides typed access to all parameters.
No hardcoded values in the codebase — everything comes from config.
"""

import os
from types import SimpleNamespace
from typing import Any

import yaml


def _dict_to_namespace(d: Any) -> Any:
    """Recursively convert a dict to SimpleNamespace for dot-access."""
    if isinstance(d, dict):
        return SimpleNamespace(**{k: _dict_to_namespace(v) for k, v in d.items()})
    if isinstance(d, list):
        return [_dict_to_namespace(item) for item in d]
    return d


def load_config(path: str = "config/corsair_v2_config.yaml") -> SimpleNamespace:
    """Load and return the configuration as a nested SimpleNamespace.

    The YAML is a flat structure with:
      - Portfolio-wide blocks (``account``, ``constraints``, ``kill_switch``,
        ``operations``, ``logging``) applied once.
      - Shared product-defaults (``puts``, ``quoting``, ``pricing``,
        ``synthetic_span``) which each product's own override block merges
        onto at runtime via :func:`make_product_config`.
      - ``products:`` — ordered list of product configs. ``products[0]`` is
        the primary (watchdog + healthcheck snapshot default); the rest are
        secondaries with equal runtime status (full quoting engines).

    Environment variable overrides:
        CORSAIR_GATEWAY_HOST  -> account.gateway_host
        CORSAIR_GATEWAY_PORT  -> account.gateway_port
        CORSAIR_ACCOUNT_ID    -> account.account_id
    """
    with open(path, "r") as fh:
        raw = yaml.safe_load(fh)

    config = _dict_to_namespace(raw)

    # Environment variable overrides for Docker deployment
    if os.environ.get("CORSAIR_GATEWAY_HOST"):
        config.account.gateway_host = os.environ["CORSAIR_GATEWAY_HOST"]
    if os.environ.get("CORSAIR_GATEWAY_PORT"):
        config.account.gateway_port = int(os.environ["CORSAIR_GATEWAY_PORT"])
    if os.environ.get("CORSAIR_ACCOUNT_ID"):
        config.account.account_id = os.environ["CORSAIR_ACCOUNT_ID"]

    return config


def _overlay_namespace(base, overrides):
    """Return a new SimpleNamespace with base attrs overlaid by overrides.

    Only replaces keys present in overrides; base keys not in overrides are
    kept. This lets a per-product block like ``quoting: {tick_size: 0.0005}``
    override just that field while inheriting everything else from the
    shared default block.
    """
    merged = SimpleNamespace(**vars(base))
    for k, v in vars(overrides).items():
        setattr(merged, k, v)
    return merged


def make_product_config(base_config, product_entry) -> SimpleNamespace:
    """Return a per-product config view rooted at *product_entry*.

    Per-product classes (MarketDataManager, QuoteManager, IBKRMarginChecker,
    etc.) read ``self.config.product`` / ``self.config.quoting`` / etc. The
    view this function returns makes those accesses land on the product's
    own ``product`` block and on merged (defaults + overrides) block for
    every shared-default section.

    Carries the product's ``name`` and ``snapshot_path`` onto the view so
    downstream code doesn't have to look them up by index.
    """
    ns = SimpleNamespace(**vars(base_config))
    ns.product = product_entry.product
    ns.name = product_entry.name
    ns.snapshot_path = getattr(
        product_entry, "snapshot_path",
        f"data/{product_entry.name.lower()}_chain_snapshot.json",
    )
    for block in ("puts", "quoting", "pricing", "synthetic_span"):
        if hasattr(product_entry, block):
            setattr(ns, block, _overlay_namespace(
                getattr(base_config, block), getattr(product_entry, block)))
    # Drop the products list from the per-product view — each engine only
    # cares about its own product.
    if hasattr(ns, "products"):
        delattr(ns, "products")
    return ns
