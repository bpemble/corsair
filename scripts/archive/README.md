# Archived scripts

One-off research and probing scripts that served their purpose during
development. Kept here for reproducibility / occasional re-runs but no
longer part of the active operational toolset.

If you need one of these scripts, just run it in place — paths still
resolve from repo root via the `sys.path` shim.

| Script | Purpose | When written |
|---|---|---|
| `mm_probe_v2.py` | Tick-side latency / response timing probe | Early Phase 0 |
| `mm_probe_v2_analysis.py` | Companion analyzer | Early Phase 0 |
| `mm_response_probe.py` | MM cancel/replace response timing | Early Phase 0 |
| `multi_amend_probe.py` | Sequential amend latency probe | Early Phase 0 |
| `multi_client_probe.py` | Multi-clientId behavior probe (FA-rewriting investigation) | 2026-04-08 |
| `amend_latency_probe.py` | Single-amend latency probe | Early Phase 0 |
| `concurrency_sweep_probe.py` | Concurrency / per-key cap sweep | Pre-cap=5 finalization |
| `nbbo_decay_analysis.py` | Tick-to-decay analysis (Alabaster) | 2026-04 |
| `regime_cycle_analysis.py` | Regime classifier study | 2026-04 |
| `requote_cycle_analysis.py` | Requote-rate vs P&L study | 2026-04 |
| `peer_cap_session_analysis.py` | Per-key peer cap calibration | Pre-cap=5 |
| `adverse_selection.py` | Adverse-fill rate analyzer | 2026-04 |
| `behind_incumbent_distance.py` | Distance-to-NBBO study | 2026-04 |
| `dual_tick_capture.py` | Two-stream tick capture for parity work | 2026-04 |
| `test_thread3_stage0.py` | Thread 3 hedge-deploy gate verification | 2026-04-26 |
| `_latency_quote_sample.py` | Latency sample generator | 2026-04 |
| `min_edge_experiment_comparison.py` | min_edge_ticks tuning | 2026-04 |
| `induce_burst.py` | Synthetic burst-tracker injection | 2026-04 |

The active operational scripts remain in `scripts/`:
- `flatten.py` — manual position flatten when broker can't auto-recover
- `dashboard.py`, `dashboard_chain.py`, `dashboard_style.py` — Streamlit UI
- `cut_over_preflight.py` — pre-cut-over verification
- `induce_kill_switch.py` — sentinel-based kill induction (Gate 0 tests)
- `live_monitor.py` — live cut-over watch
- `parity_compare.py`, `rust_trader_parity.py` — Python/Rust parity harnesses
- `reconcile_positions.py` — position state reconciliation utility
