# Corsair → Arty A7-100T Feasibility Analysis

> All resource numbers are engineering estimates. Anything resting on
> assumption rather than measurement is flagged. "Brutally honest" was
> the brief; that's the tone.

## TL;DR

**Recommendation: Yes, a meaningfully scoped prototype fits, and it is
worth doing — but do not try to port the system. Port the *tick → place*
decision, stub everything else, drive from recorded MDP3.** Three
components fit naturally (parser, gates, order TX). One component
(Black-76 + SVI/SABR theo) is the hard part and the right vehicle for
the prototype's research value. Calibration, risk, hedging, position
keeping must stay in software.

The honest framing: this is a **latency-pipeline experiment**, not a
"trading system on FPGA." Done that way, Arty A7-100T is the correct
starting board.

---

## 1. Codebase inventory & hot path

Measured LOC (28,494 Rust LOC total across the workspace):

| Crate | LOC | Hot path? | Notes |
|---|---:|---|---|
| `corsair_trader` | 5,014 | **Yes (core)** | tick consumer + decision flow + order TX |
| `corsair_pricing` | 1,616 | **Yes (theo)** | Black-76, SVI, SABR, Greeks, SPAN, Brent |
| `corsair_ipc` | 1,240 | **Yes (transport)** | SHM rings + msgpack frames |
| `corsair_market_data` | 962 | Partially | ATM tracker, subscription window |
| `corsair_broker_ibkr_native` | 4,754 | TX side only | IBKR wire client |
| `corsair_broker` | 6,378 | No | risk monitor, hedge, snapshot, IPC server |
| `corsair_risk` | 1,196 | No (1Hz) | risk_state publisher |
| `corsair_constraint` | 731 | No | margin gate |
| `corsair_position` | 1,371 | No | position book, P&L |
| `corsair_hedge` | 1,057 | No | hedge controller (30s cadence) |
| `corsair_oms` | 123 | No | shrunk to two functions |

**The actual hot path** (`decision.rs:122-605`, the function
`decide_on_tick`, ~480 lines of in-line logic):

```
[wire bytes]
  → MDP3 parser (skipped today — IBKR feed; would need replacement)
  → tick struct
  → kill_count atomic check (1-cycle)
  → vol_surface DashMap lookup (~2 hashes)
  → ATM-window + OTM-only filter (4 compares)
  → risk gates compute (5 compares + 2 Option unwraps)
  → tte_cache lookup or chrono parse (rare)
  → theo_cache lookup
       hit:  return (theo_at_fit, delta_at_fit)
       miss: compute_theo (SVI or SABR implied_vol → Black-76 → delta)
  → Taylor reprice: theo + delta·(spot − spot_at_fit)
  → L1/L2 self-fill filter (4 compares + 2 Option unwraps)
  → two-sided + thin-book + wide-spread gates
  → for each side {BUY, SELL}:
       target = theo ± edge
       tick-jump if behind incumbent
       cross-protect against contra BBO
       cap to mkt_mid ± edge
       quantize to tick grid
       dead-band + GTD-refresh + cooldown + unack-inflight gates
       emit Modify or Place
  → msgpack-encode command frame
  → SHM ring write
```

External hot-path dependencies: **`statrs::Normal::cdf`** (erfc-based
polynomial in pricing.rs and greeks.rs), **`chrono`** (TTE parse, cached
so amortized), **`dashmap`** (concurrent map), **`rmp-serde`** (msgpack).

---

## 2. Algorithmic analysis per component

### 2a. MDP3 parser (NOT in current codebase — would replace IBKR transport)

- **Computes**: byte stream → fixed-record tick struct (price, size,
  side, sequence, channel).
- **State**: small ring buffer for gap detection + book state if you
  want top-N depth.
- **Ops**: byte alignment, big-endian uint reads, signed price decoding
  (mantissa+exponent), branch on template ID.
- **HW pipeline?** **Yes — natural fit.** Streaming SBE decoder is the
  canonical FPGA workload. Fixed latency, deterministic, no
  data-dependent loops.
- **Branches**: only on template ID (small fan-out — implement as case
  statement, all branches in parallel).

### 2b. Strike/expiry filter & risk gates (ATM window, OTM-only, kill check, risk_block)

- **Computes**: ~10 comparisons against scalar thresholds.
- **State**: 5–8 scalar registers (delta_ceiling, delta_kill,
  margin_ceiling_pct, …).
- **HW pipeline?** **Trivial. 1–2 cycles.** Direct combinational
  comparators.

### 2c. Vol surface lookup

- **State**: ~4 expiries × 2 sides = 8 entries × {a, b, ρ, m, σ,
  fit_ts, spot_at_fit, fwd, calibrated_min/max} ≈ 8 × 80 bytes ≈
  **640 bytes total**. Trivial.
- **Ops**: hash (expiry, right) → entry. Production: DashMap; in HW:
  small CAM / direct-mapped LUTRAM.
- **HW pipeline?** **Yes.** With 8 entries, fully parallel compare
  against {expiry_id, right_bit} works in 1 cycle.

### 2d. Theo cache

- **State**: ≤200 entries × (theo_f64, delta_f64) = 3.2 KB.
- **HW pipeline?** **Yes** — small BRAM, hash-keyed.
- **Decision**: For prototype, skip the cache and recompute theo on
  every tick. Adds latency but removes a state machine. Alternative:
  64-entry direct-mapped (~1 KB) with strike-quantized index.

### 2e. SVI / SABR implied vol (the hard one)

#### SVI

```
k = ln(K/F)
w = a + b·(ρ·(k − m) + √((k − m)² + σ²))
σ_iv = √(w / t)
```

Operations: **1 ln, 1 sqrt (inner), 1 sqrt (outer), 1 div, 5 mul,
3 add**. All f64.

#### SABR (Hagan)

```
fk = F·K
fk_β = (F·K)^((1−β)/2)
ln_fk = ln(F/K)
z = (ν/α) · fk_β · ln_fk
sqrt_term = √(max(1 − 2ρz + z², 0))
xz = z / ln((sqrt_term + z − ρ) / (1 − ρ))   [unless |z|<ε; ATM branch]
denom1 = fk_β · (1 + ((1−β)²/24)·ln_fk² + ((1−β)⁴/1920)·ln_fk⁴)
p1 = ((1−β)²/24) · α² / fk^(1−β)
p2 = 0.25 · ρ · β · ν · α / fk_β
p3 = (2 − 3ρ²) · ν² / 24
σ_iv = (α / denom1) · xz · (1 + (p1+p2+p3)·t)
```

Operations: **2 pow (with non-integer exponent), 1 ln, 1 sqrt, ~25 muls,
~15 adds, 1 div, 1 ATM branch**. The two `powf` calls (`(1−β)/2` is
fractional, typically β=0.5) are the most expensive — they expand to
`exp(b·ln(a))`.

#### Black-76 + delta

```
sqrt_t = √t
d1 = (ln(F/K) + 0.5·σ²·t) / (σ·sqrt_t)
d2 = d1 − σ·sqrt_t
df = exp(−r·t)         [r=0 in production → df=1, save the exp]
theo = df·(F·N(d1) − K·N(d2))    or put-side equivalent
delta = df·N(d1)                 or N(d1)−1 for puts
```

Operations: **1 ln, 1 sqrt, 2 N(·), 1 exp (skipped if r=0), 4 mul,
3 add, 1 div**.

`N(x)` (`norm_cdf`) is the dominant transcendental. statrs uses the
standard erfc rational-approximation; in FPGA you'd implement either
Abramowitz–Stegun coefficient tables + polynomial, or a piecewise-linear
LUT.

**HW pipeline-ability**: Yes for **both** SVI and SABR, but it's a
**deeply pipelined fixed-function unit, not a 1-cycle block**. Realistic
estimate at 250 MHz, fp32 (see §4): SVI ~30–40 cycles end-to-end; SABR
~60–90 cycles end-to-end including the two `pow` ops.

The only data-dependent branch is SABR's ATM short-circuit
(`|F−K|<ε·F`); handle by computing both paths and selecting at the end
(~negligible cost; the ATM path is shorter so it's masked, not the
other way around).

### 2f. Brent's method (`implied_vol`)

`pricing.rs` line 97. Used to invert market mid → IV when there's no
surface. **It's a loop with up to 100 iterations, each calling
Black-76.** This is *not* a fixed-latency pipeline-friendly operation.
**Decision: don't put Brent on the FPGA.** Use it only in the broker
(Python or Rust on host CPU) for fitting.

In the trader hot path Brent isn't actually called — the trader uses
**the surface params** (a, b, ρ, m, σ for SVI; α, β, ρ, ν for SABR), not
Brent. So this is fine to cut.

### 2g. SVI/SABR calibration (`calibrate.rs`, 615 LOC)

Levenberg–Marquardt nonlinear least-squares. **Hard NO for FPGA.** Runs
at 60 s cadence on the broker, not on the hot path. Stays on host.

### 2h. SPAN margin (`span.rs`, 197 LOC)

16-scenario portfolio scan. Runs in `corsair_constraint` and
`corsair_position`, not on the hot path (called per-fill, not per-tick).
**Stays on host.**

### 2i. Order TX (msgpack encode + SHM ring write)

In Corsair this is `corsair_trader::msgpack_encode` +
`corsair_ipc::ring`. In an FPGA prototype the corresponding output is
**MAC frame construction for an outgoing order** (CME iLink3 SBE /
OUCH-like). MDP3 input ≠ order TX format; the trader never builds
CME-format order packets today (it sends to IBKR's wire format, which is
not what you'd colocate with). **For the prototype, define a synthetic
"order out" format** — fixed-layout binary record over UDP, since you're
not actually trading.

---

## 3. Resource estimation (Arty A7-100T budget: 63.4K LUTs, 126.8K FFs, 4.9 Mb BRAM, 240 DSPs)

All numbers are **engineering estimates**, assuming the design uses
**fp32** (not fp64 — see §4) and Xilinx Floating-Point Operator IP.
Error bars: ±50% on LUT counts, ±20% on DSP counts.

| Component | LUTs | FFs | BRAM (36Kb) | DSPs | Notes |
|---|---:|---:|---:|---:|---|
| 1Gb Tri-Mode MAC | 4–6K | 4K | 2 | 0 | Xilinx free IP |
| MDP3 SBE decoder (HG only, ~5 templates) | 3–5K | 3K | 2 | 0 | streaming |
| Tick FIFO + dispatch | 500 | 500 | 1 | 0 | |
| Strike/expiry filter + risk gates | 300 | 200 | 0 | 0 | combinational |
| Vol surface LUT (8 entries × ~80 bytes) | 200 | 100 | 1 | 0 | |
| **Black-76 (fp32)** | 4–6K | 5K | 4 (LUTs for `ln`, `exp`, `erf`) | 12–16 | dominant block |
| **SVI implied_vol (fp32)** | 2–3K | 3K | 2 | 6–8 | uses ln + sqrt |
| **SABR implied_vol (fp32)** | 5–7K | 6K | 4 | 14–18 | two `pow`, one `ln`, one `sqrt` |
| Taylor reprice + edge math | 1–1.5K | 1K | 0 | 4 | mul + add + min/max |
| Tick-jump + cross-protect + dead-band | 500 | 400 | 0 | 0 | comparators |
| Resting-order table (24 slots × 32B) | 400 | 200 | 1 | 0 | LUTRAM |
| Order TX framer + UDP/IP | 1–1.5K | 1K | 1 | 0 | |
| Latency timestamp counters | 200 | 200 | 0 | 0 | free-running counter |
| MIG DDR3 controller (if used) | 2K | 2K | 2 | 0 | optional — see §5 |
| **AXI/clock/reset glue** | 2–3K | 2K | 0 | 0 | |
| **Subtotal (full pipeline, fp32)** | **24–37K** | **28K** | **20** | **36–46** | |
| Margin / utilization | **38–58%** LUTs | **22%** | **15%** | **15–19%** | |

**This fits.** With margin to spare. The dominant consumer is Black-76
+ SABR fp32 transcendental machinery, exactly as expected.

The **pinch point is DSP density**, not LUTs. fp32 multiply is ~3 DSPs
per multiplier on Artix-7 (DSP48E1 is 25×18 signed; you compose three
of them for the 24×24 mantissa). The estimate above assumes you reuse
multipliers across pipeline stages. With aggressive reuse, you can cut
DSP count ~30–40%; with full unrolled pipelines (one tick per cycle), it
grows ~3×, which would still fit but eat most of the DSP budget.

---

## 4. Precision and data type analysis

### Where you can drop to fixed-point cleanly

| Quantity | Range | Fixed-point recommendation |
|---|---|---|
| Strike, forward, spot, prices | ~$0.0005–$10 grid in HG; 5 ticks of slack | Q16.16 (32-bit) — ample |
| Bid/ask sizes | int32 already | int16 plenty |
| Time stamps | u64 ns | keep u64 |
| Tick-jump edge math, mid cap | constants × tick_size | Q16.16 |
| Order qty | always 1 in production | u8 |

### Where you genuinely need floating-point

| Quantity | Why |
|---|---|
| `ln(F/K)` | log moneyness ranges -0.3..+0.3 in production but is squared in p1, fourth-powered in denom1, multiplied by ν/α etc. Fixed-point error compounds across ~25 multiplies. fp32 (24-bit mantissa) gives ~7 decimal digits, enough. |
| `σ_iv` | Range 0.001–5.0, multiplied/divided/squared throughout Black-76. fp32 fine. |
| Theo, delta | These ARE quantized to tick grid eventually, but the intermediate Black-76 result needs ~6 decimals of precision because edge can be 1 tick = $0.0005 = ~0.005% of theo at $0.10 strike. |
| `N(d1), N(d2)` | These are 0..1 and need ~5 decimal accuracy near the wings. fp32 is sufficient — Xilinx's float `erf` IP is single-precision and is fine here. |

### Can you just use fp64 in fabric?

Technically yes — Xilinx provides fp64 Floating-Point Operator IP — but
**fp64 multiply on Artix-7 needs ~10 DSPs and ~1.5K LUTs each**, vs ~3
DSPs and ~500 LUTs for fp32. With ~40 multiplies between SABR + Black76,
fp64 would consume ~400 DSPs (you only have 240) and would not fit on
Arty A7. **fp32 is the only option for prototype.**

### Numerical risk of fp32 vs production fp64

I'd want to validate this empirically against the Rust `f64` baseline
using `corsair_tick_replay`, but my prior is: the structural error from
fp32 in Black-76 is ~10⁻⁷ relative, well below the tick grid (0.0005 /
typical_theo ≈ 1%). The places I'd worry about:

1. **`(1 − 2ρz + z²)` near `ρ=±1`** — already clamped to 0 in Rust
   (`pricing.rs:280`). fp32 makes the clamp fire slightly more often.
   Acceptable.
2. **`ln(F/K)` near ATM (F=K)** — log → ~0, gets multiplied by tiny
   coefficients. fp32 loses precision here, but Hagan's ATM short-circuit
   takes over before it matters. Acceptable.
3. **Brent in `implied_vol`** — irrelevant, not on prototype's hot path.

### Lookup tables that should replace computation

- `erf` / `norm_cdf`: Xilinx already ships a polynomial-approximation
  IP. Alternatively: 2K-entry LUT with linear interp = 1 BRAM, 3-cycle
  pipeline.
- `exp(-r·t)`: in production `r=0` so this is the constant 1.0. **Don't
  even synthesize the exp unit** unless you plan to enable `r ≠ 0`.
- `sqrt`: Xilinx CORDIC or float-IP — both fine.
- `ln`: float-IP. ~12 cycles, ~500 LUTs, ~1 DSP.
- `pow(x, fractional)`: implement as `exp(b·ln(x))`. **This is the SABR
  cost driver.** β is constant (0.5) in production, so `(1−β)/2 = 0.25`
  is constant — `x^0.25` can be a custom unit (`sqrt(sqrt(x))`) instead
  of full pow. This saves ~50% of the SABR DSP cost. Recommended.

### Operations that won't fit DSP48E1 natively

- **fp64 multiply**: needs cascade as above. Avoid.
- **Variable-shift barrel shifters wider than 25 bits**: split across
  LUTs.
- **Integer divide** (the price-quantize: `(target / tick_size).round()
  * tick_size`): in HW, replace with multiply-by-reciprocal of tick_size
  (a constant); single mul + round.

---

## 5. Memory footprint

### Hot path working set (per-tick)

- TickMsg (after parse): ~96 bytes
- ScalarSnapshot: ~120 bytes
- Vol surface entry: ~80 bytes (1 of 8)
- Theo cache entry (if hit): 16 bytes
- Resting order entry: 32 bytes (up to 24)
- **Per-tick state touched: <1 KB.** Stays in registers + LUTRAM, no
  BRAM pressure.

### Persistent state

| Item | Size |
|---|---|
| Vol surface table (8 entries) | 640 B |
| Theo cache (200 entries × 16 B) | 3.2 KB |
| Resting orders table (24 × 32 B) | 768 B |
| Subscription window / strike index (~80 strikes) | 1 KB |
| Risk-state scalars | 256 B |
| Ring buffers (incoming tick FIFO 1024 × 96 B) | 96 KB |
| **Total** | **~110 KB** |

**Vs 4.9 Mb BRAM = 612 KB available.** Fits with 5× headroom. **No need
for DDR3L.** Don't instantiate the MIG controller — saves 2K LUTs and
removes a 30-50 ns variable-latency path from the pipeline.

### What would push you to DDR3L

Only if you wanted to:

- Buffer recorded MDP3 traces on-FPGA for replay (256 MB / 96 B/tick ≈
  2.6M ticks ≈ 30 min of HG data — useful for stress testing).
- Maintain a full L2 book across many strikes (you don't need this for
  the prototype; the trader gets pre-aggregated top-of-book + 2-level
  depth).

DDR3L latency on MIG: ~30–60 ns + occasional 200 ns refresh stalls.
**Don't put it on the hot path.** Use it strictly for trace storage.

---

## 6. Network limitations

### CME MDP3 message rates (HG specifically)

I can't query CME for current numbers from this machine, but the
publicly-cited orders of magnitude are:

- **All-products MDP3 peak**: ~10 Gbps across primary + secondary feeds
  (entire futures + options universe).
- **Per-channel peak**: typically <200–400 Mbps; HG (channel ~316 last
  I checked, **verify**) is well under that — HG futures + options are
  mid-volume products, not ES/CL.
- **Per-product avg msg rate**: kHz steady-state, hundreds of kHz in
  macro events. HG specifically is well under this.

**1Gb is sufficient for an HG-only prototype.** It is *not* sufficient
for ES + NQ + CL + GC + HG bundled, but you don't need that.

### What gets clipped at 1 Gb

If you wanted to flow the full primary+secondary MDP3 feed: yes, you'd
lose packets. Mitigations: (a) channel filter at the switch (CME
publishes per-product channel mappings), (b) drop secondary, (c) drop
instrument-definition replays, (d) drop products you don't quote.

### 1Gb vs 25/100GbE for production

For a single-product paper-trading prototype, the Arty's 1Gb is
realistic. **The reason production HFT goes 25GbE is jitter/PHY-latency,
not throughput** — 25G PHY adds ~50 ns; 1G RGMII adds ~300–500 ns. That
delta would be the dominant fixed offset in your wire-to-wire latency
for the prototype, but it's a **constant** that wouldn't invalidate the
prototype's research value (you measure the *internal* pipeline, then
add the known MAC offset).

### No PTP

The Arty A7-100T has no hardware PTP. **You can't do nanosecond-accurate
cross-machine timestamping.** For prototype: use a free-running on-FPGA
counter as the local time reference, and emit timestamps at ingress +
at TX. This gives you internal pipeline latency to ~clock-period
resolution (4 ns at 250 MHz), which is plenty for measuring the FPGA
path.

---

## 7. What won't fit / is a hard blocker

| Component | Why not |
|---|---|
| **Full Rust hot path as-is** | Mostly because of `dashmap`/`tokio`/`chrono`/`rmp-serde` — these aren't FPGA-portable concepts. The *algorithm* fits; the runtime doesn't. |
| **fp64 throughout** | DSP density blows the budget. Forced to fp32. |
| **SVI/SABR calibration (LM solver)** | Iterative solver with data-dependent termination. Belongs on host. |
| **Brent's `implied_vol`** | Same — iterative root finder. Not on the trader hot path anyway. |
| **SPAN portfolio margin** | 16-scenario × N-position scan, not hot-path, fp64. Host. |
| **Position book / VWAP P&L / hedge controller** | Not hot path; runs on broker; involves f64 + JSONL + execDetails reconciliation. Host. |
| **Risk monitor (1 Hz)** | Not hot path. Host. The *gates* derived from it (delta_ceiling, margin_pct, kill flags) are scalar inputs to the FPGA, refreshed at 1 Hz. |
| **Snapshot publisher / dashboard** | Streamlit poll. Host. |
| **IBKR wire client (4,754 LOC)** | Production-only. Replace with synthetic order TX or null sink for prototype. |
| **JSONL streams** | Host-side telemetry consumer pulls them off a UDP/PCIe sideband. |
| **Hedge subsystem** | 30 s cadence + reconciliation + execDetails. Host only. |

There is **no part of the hot path itself that fundamentally won't fit
on Arty A7-100T at fp32**.

---

## 8. Prototype decomposition — a "minimum viable hot path"

### Scope

**Goal**: One end-to-end pipelined path that reads recorded MDP3 frames,
computes a quote decision, and emits an order frame, with cycle-accurate
latency stamps at ingress and egress.

### Modules to port to HDL

1. **MDP3 SBE decoder, HG-only** — 5 templates (Snapshot,
   MDIncrementalRefreshBook, MDIncrementalRefreshTrade,
   SecurityDefinition, Heartbeat). Streaming, ~3–5K LUTs.
2. **Per-strike top-of-book / depth-2 maintainer** — small CAM keyed on
   (security_id), stores BBO + sizes + 2-level depth. ~1K LUTs + 1 BRAM.
3. **Tick → decide_on_tick gate stack** — strike-window, OTM-only,
   two-sided, thin-book, kill, risk_block. Combinational; ~500 LUTs.
4. **Vol surface table** — host preloads via UART/JTAG. 8 entries × 80
   B. ~200 LUTs + 1 BRAM.
5. **SVI implied_vol unit (fp32, pipelined)** — ~2–3K LUTs + 6–8 DSPs.
6. **Black-76 + delta unit (fp32, pipelined)** — ~4–6K LUTs + 12–16
   DSPs.
7. **Taylor reprice unit** — ~1K LUTs + 4 DSPs.
8. **Quote target + tick-jump + cross-protect + quantize** — ~500 LUTs.
9. **Order frame builder + UDP/IP/MAC** — ~1.5K LUTs + 1 BRAM.

**SVI only for the prototype**. Skip SABR initially — same precision
character, half the multiplier count, simpler ATM branch, and the
production trader supports both. Add SABR in v2 once fp32 numerical
accuracy is validated.

### Modules to simplify or stub

- **Theo cache** — skip entirely. Recompute theo every tick. Adds
  latency but removes a state-machine bug surface. Cache can land in v2.
- **Resting-order table** — implement as fixed 24-slot LUTRAM, but stub
  the dead-band/GTD/cooldown logic. Always emit a Place. Real production
  needs the throttling chain; the prototype does not.
- **L1/L2 self-fill filter** — for the prototype, accept that you might
  quote into your own resting order. Production-wrong, prototype-fine.
- **Risk gates** — read from a 32-bit register block written over JTAG
  by a host script. Fixed values per run. Don't try to wire 1 Hz
  risk_state ingress into the FPGA — it's host-only state.
- **Modify vs Place** — emit Place only.

### Modules to skip entirely on FPGA (host-only)

- Calibration (offline, host pre-loads surface)
- Brent / implied_vol from market mid
- SPAN margin
- Position book, P&L
- Hedge subsystem
- Risk monitor
- Snapshot publisher
- IBKR wire client
- All 8 JSONL streams (host consumer over UDP sidechannel)

### Synthetic test harness

- **Recorded MDP3 trace player**: stream a captured `.pcap` file from a
  Linux host's NIC into the Arty's 1G port, OR load it into DDR3L and
  replay from the FPGA itself. Latter is cleaner for repeatability.
- **Vol surface loader**: small Python script writes 8 entries to a
  memory-mapped register space over JTAG before each run.
- **Latency capture**: free-running 4 ns counter; log {ingress_ts,
  decision_ts, egress_ts} per tick that produces a Place. Stream over
  UART or UDP to host.
- **Reference implementation**: the existing Rust `decide_on_tick`, run
  on the host against the same trace. **Compare order-by-order: same
  trace must produce same decisions** (modulo numerical fp32-vs-fp64
  noise within ½ tick). This is the bit-equivalence test that earns the
  prototype's validity.
- **fp64-vs-fp32 deviation report**: for every tick, log |theo_fpga −
  theo_rust| and the fraction of decisions that differ. This is the
  actual research deliverable.

---

## 9. Latency projections (fp32 prototype, Arty A7-100T at 250 MHz = 4 ns/cycle)

All cycle counts are **engineering estimates assuming Xilinx float IP
standard latencies**. Real numbers will vary ±30%.

| Stage | Cycles | Time @ 250 MHz |
|---|---:|---:|
| RGMII PHY in (1Gb) | ~120 | 480 ns |
| MAC RX FIFO + UDP/IP strip | ~10 | 40 ns |
| MDP3 SBE decode + dispatch | 8 | 32 ns |
| BBO maintain + lookup | 4 | 16 ns |
| Strike/expiry/risk gates | 2 | 8 ns |
| Vol surface lookup | 2 | 8 ns |
| `ln(F/K)` | 12 | 48 ns |
| SVI compute | 30 | 120 ns |
| Black-76 (`sqrt`, `ln`, two `erf`/`N`, mults) | 40 | 160 ns |
| Taylor reprice | 6 | 24 ns |
| Tick-jump + cross-protect + quantize | 4 | 16 ns |
| Place gate + frame build | 6 | 24 ns |
| UDP/IP/MAC TX framing | 10 | 40 ns |
| RGMII PHY out (1Gb) | ~120 | 480 ns |
| **Total wire-to-wire** | **~374** | **~1.50 µs** |
| **Internal pipeline only (ingress→egress at MAC)** | ~134 | **~535 ns** |

**Reality check**: today the Rust trader's TTT p50 is **50 µs**, p99
**1–3 ms**. The FPGA prototype projects to **30–100× lower latency in
steady state**, with **deterministic** tail (no GC, no scheduler, no
DRAM cache miss).

**At ~535 ns internal pipeline, the limiting factor is RGMII PHY at
both ends (~960 ns combined).** Going to 25GbE on a UltraScale+ board
would shrink that to ~100 ns combined PHY. So **the same logic on a
Kintex UltraScale+ XCKU040 + 25GbE** would project to roughly:

| Stage | UltraScale+ projection |
|---|---:|
| Internal pipeline | ~135 cycles × 2.5 ns (400 MHz) = 340 ns |
| 25GbE PHY in + out | ~100 ns |
| **Wire-to-wire** | **~440 ns** |

This is the right ballpark for credible production HFT FPGA designs
(top-tier shops report 200–500 ns wire-to-wire on similar paths). **The
Arty prototype's pipeline architecture would scale to that without
redesign.** What changes is the PHY, the clock, and the DSP density
allowing wider unrolling.

---

## 10. Effort estimate

### Person-hours (assuming intermediate-to-advanced HDL skill)

| Phase | Hours |
|---|---:|
| Bring-up: MIG + MAC + UART on Arty | 20–40 |
| MDP3 SBE decoder (HG, 5 templates) | 60–100 |
| BBO maintainer + filter pipeline | 40–60 |
| **Black-76 fp32 unit (the hard part)** | **80–150** |
| **SVI fp32 unit** | 40–80 |
| Quote target + gates + frame builder | 30–60 |
| Latency instrumentation + UART/UDP debug egress | 20–40 |
| Recorded-trace replay harness (host + FPGA side) | 30–50 |
| **Bit-equivalence test (Rust vs FPGA on same trace)** | 40–80 |
| Timing closure + place-and-route iteration | 40–100 |
| **Total range** | **400–760 hours** |

That's **~2.5–4.5 person-months full-time**, or **6–12 calendar months**
as a side-project. **Bias toward the upper end** — Black-76 fp32 is
where prototype FPGA projects historically over-run, and you're
integrating Xilinx float IP which has its own quirks.

### Skill prerequisites

- Solid SystemVerilog or VHDL, plus the AXI-Stream / AXI-Lite idioms.
- Comfort with Xilinx Floating-Point Operator IP (configurable latency
  / DSP usage / max frequency).
- Ability to read a SBE template specification and produce a streaming
  decoder (this is a "novel HDL" task — there's no IP for this).
- Tcl scripting for Vivado build + bitstream packaging.
- Linux side: pcap manipulation, UDP socket programming, register-mapped
  FPGA control.

### Where Claude Code can realistically help

**Strongly:**

- **Bit-equivalence test harness** (Rust): adapting `corsair_tick_replay`
  to drive a UDP egress and compare outputs cycle-stamp by cycle-stamp.
- **MDP3 SBE decoder skeleton**: I can produce a SystemVerilog template
  you'd then tune for timing. Logic correctness from spec is in scope.
- **SVI / Black-76 datapath floorplan**: writing the SystemVerilog
  modules referencing Xilinx float IP. I can write the source; **timing
  closure is on you**.
- **Verification testbenches**: cocotb or UVM-lite. Auto-generating test
  vectors from the Rust reference is in scope.
- **Reference-vs-FPGA differential analysis** (Python/Rust): post-run,
  computing fp64-vs-fp32 deviation distributions.

**Weakly or not at all:**

- **Vivado synthesis/PnR**: I can't run it and can't observe its output.
  You drive Vivado.
- **Timing closure debugging**: requires interactive iteration on the
  actual report. Out of scope here.
- **Board bring-up**: physical wiring, JTAG, scope debugging — yours.
- **CME MDP3 connectivity** (if you went live): out of scope for this
  prototype anyway.

---

## 11. Recommendation

**Build the prototype. Arty A7-100T is correctly sized for it.**

Concretely:

1. **Scope it ruthlessly to the SVI + Black-76 + decision pipeline**,
   fp32, replay-driven, single-product (HG). Don't try to put the
   broker, position book, risk monitor, or hedge subsystem on FPGA —
   none of them belong there and they'll consume effort with no
   architectural payoff.

2. **The prototype's research value is the precision study**, not the
   latency number. The latency number will be impressive (sub-microsecond)
   and is a useful sanity check that the architecture works. But the
   deliverable that informs *production* FPGA decisions is: "given a
   fp32 implementation of SVI + Black-76, on a 60-day HG trace, what
   fraction of trader decisions disagree with the fp64 Rust reference,
   and is that disagreement correlated with adverse fills?" If fp32
   deviation costs you nothing in fill quality, the production board can
   be smaller and cheaper. If it costs you basis points, you need fp64
   (i.e., production board needs UltraScale+ class DSPs).

3. **Stepping-stone-ness is real**. Everything you'd do here transfers
   directly to a Kintex UltraScale+ board with 25GbE: same SBE decoder,
   same vol-surface lookup, same SVI unit (slightly retuned for higher
   clock), same Black-76 unit, same gate stack. The non-transferring
   parts are the MAC (1G→25G), the clocking, and the timing-closure
   work. **Roughly 70% of the HDL carries forward.**

4. **Effort is non-trivial.** Plan on 400–760 hours. If your time is
   more valuable spent elsewhere, this is a "hire someone with HDL
   muscle for 3 months" project, not a "I'll knock it out over a
   weekend" project. The Black-76 fp32 unit is where most projects
   discover this.

5. **Things that would change my mind on feasibility:**
   - If you require fp64 throughout (you'd outgrow Arty A7).
   - If you require multi-product simultaneous quoting (still fits, but
     the BBO state per-product crosses 4.9 Mb on ~50+ products).
   - If you require **latency under 200 ns wire-to-wire** (Arty's 1Gb
     RGMII PHY costs ~960 ns alone — physically can't get there on this
     board).
   - If you require ARP/IP/UDP stack with full RFC compliance (the
     prototype I described uses a fixed-target hardcoded packet — fine
     for replay, not fine for live).

6. **What I'd want before greenlighting the full effort:**
   - A 1-week spike: write the Rust-side bit-equivalence test harness,
     generate fp32 reference outputs for a 1-hour HG trace, count
     decision deviations. **If fp32 vs fp64 produces >0.5% disagreement
     on Place decisions, the prototype's value drops sharply.** This is
     a host-only experiment and you can do it without any FPGA work.

The 1-week spike result is the actual gating decision, not the FPGA
resource estimate. Resources fit. The open question is whether the
precision compromise is acceptable, and you can answer that on a laptop
in a week before committing to 3 months of HDL.
