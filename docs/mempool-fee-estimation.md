# Mempool fee estimation (rbitcoin)

## Product opinion: 10-minute inclusion is the standard API

The **default** fee estimate this node advertises answers:

> If current mempool pressure and admit/evict/relay flows continue, what feerate
> still gets a package into the next few blocks **after about 10 minutes**?

| Surface | Role |
|---------|------|
| Electrum `blockchain.estimatefee` (default / primary) | **10-minute inclusion** |
| Esplora fee endpoints (primary) | Same |
| Optional target-depth knobs | **Near:** flow invert. **Far:** block history. Blended. |

`estimatesmartfee` and Electrum `blockchain.estimatefee` treat target 1 as 2,
as Core's `estimateSmartFee` does, while flow is not warm (fullness α < 1,
below), and `estimatesmartfee` then reports `blocks` 2. Until α = 1 the
1-block quote still blends in the cold 99.9% history, which a quiet network
such as signet keeps far above target 2. At α = 1 target 1 is the warm
1-block quote. Electrum target 0 stays the 1-block quote. `estimaterawfee`
and Esplora `/fee-estimates` key `1` keep the 1-block quote below.

The published rate blends a cold quote and a warm quote by flow fullness α.
α = 0 is history, raised by the live frontier when the pool reaches that
depth. α = 1 is the warm quote: the 1-block target is live stock and capped
admit-EMA at 99.9% confidence, conditional on the next block arriving within
10 minutes, and history answers only when flow has nothing to say. Targets
from 2 blocks blend the flow rate with the historical rate as
`w·R_flow + (1-w)·R_hist`, `w(N)=exp(-(N-1)/6)`, so neither is a floor for
the other (`w(2)≈0.85`). In between, the quote is `(1-α)·cold + α·warm`.
A missing warm side leaves the cold quote. Flow with no history answers
only at α = 1, so a thin pool cannot invent a rate. The N=1 confirm-memory
floor is inside the warm quote only.

## Non-blocking vs accept (published snapshot)

Fee APIs **do not** walk the live mempool graph on every Electrum/Esplora request.

| Path | Behavior |
|------|----------|
| Accept / remove | Marks fee cache **dirty** only (no recompute on the admit critical path). |
| Refresh | Singleflight: at most one recompute; **one** `mining_chunks_best_first` under a short hub read lock, then pure math off-lock for all depths. |
| Request | Loads a published `Arc` snapshot (≤ **~1 s** stale when dirty/max-age). Histogram and frontier use the **same** published chunk list. |

This avoids fee-estimates holding the hub lock for multi-second full-pool linearizes (which previously blocked accepts and vice versa). Estimates may lag a short bound after fee spikes. A request floors each rate at the live admission floor (`mempoolminfee`: the configured min relay, or a full pool's rolling minimum), read under a short hub read lock when it answers, so a raised floor applies at once, even while a refresh runs. Confirm-memory clips **N=1** only.

## Engine v2 (shipped)

**Temporal flow projection** under the same APIs as v1:

1. **Clock is mempool flow, not last-block age.** Always plan block *k* at
   **T + 10 k minutes** (`capacity_wu = N × 4_000_000`). Never stretch by wall
   time since the last tip.
2. **Live stock:** mining-chunk weight strictly above candidate rate R
   (`weight_above_feerate` / frontier).
3. **Inflow EMA:** per feerate bucket, exponential moving average of admitted
   package/chunk weight per second (`FeeFlowMeter` on successful accept).
4. **Include at R** when
   `stock_above(R) + λ(c)·projected_inflow(R, min(N×600s, 600s)) ≤ fill(c) × N × 4e6`.
   `c(N)` is 0.999 at N=1 and 0.99 for N≥2. `fill(0.999)=0.80`,
   `fill(0.99)=0.95`; `λ(0.999)=2`, `λ(0.99)=1`. Inflow horizon is capped at
   ~4 admit half-lives so a 150 s EMA is not stretched to a week. When no
   ladder rate fits, there is no projected rate: the ladder cannot price that
   demand, so the depth falls back to the frontier, then to history.
5. **Frontier** is the marginal chunk at `N×4e6` WU. The flow rate is the
   higher of the projected rate and the frontier. If the pool is thinner
   than N blocks, stock does **not** set a far rate (no last-chunk-as-far),
   and near depths (`w≥0.5`, N=1–5) keep the projected rate alone.
6. **Blend (warm flow):** N=1 is `R_flow`, or `R_hist` when flow is undefined.
   N≥2 is `w·R_flow + (1-w)·R_hist` with `w=exp(-(N-1)/6)`; a missing side
   drops out. Then enforce `R(1)≥R(2)≥…`.
7. **N=1** with warm flow may additionally clip to the confirm-memory **p90**
   (64-sample ring; not max-of-64), and falls back to it when neither flow nor
   history has a rate. Long N does not.

**Flow fullness.** α is decayed admitted weight divided by the weight a full
block every 10 minutes leaves in the 150 s half-life (~1.44e6 WU). It starts
at 0, so time in IBD does not age it, and it falls back toward 0 when admits
stop. The scalar adds each admit's weight. It does not use the per-bucket
time-gap update, so one transaction after a long pause does not mark the
meter full. A catch-up faster than one block per 10 minutes fills α sooner.
The meter is not persisted. A target whose history is not ready
has no rate. The refresh evaluates eleven depths (1, 2, 3, 4, 5, 6, 10, 20,
144, 504, 1008) and no others. If **no** depth has a rate, APIs return
insufficient (RPC / Electrum `-1`; Esplora leaves the target out and, when no
target has a rate, answers **503** on mainnet and **200** `{}` on every other
network). If a nearer depth is defined and a later
one is not, **hold the last defined rate** through the later computed depths
so those depths do not drop out. A confirm target between two computed depths
that both have rates is the straight line between those rates in block count,
in whole sat/kvB (nearest, halves away from zero, kept between the two rates).
A target past the last defined depth holds that rate. A target before every
defined depth stays insufficient. Esplora `/fee-estimates` answers each integer
from 1 through 25, plus 144, 504, and 1008, when that target has a rate. The
node logs when the flow-fullness decile changes and how many targets' history is ready.

### Parameters (code constants, not env)

| Parameter | Value |
|-----------|--------|
| Block weight capacity | 4_000_000 WU |
| Seconds per planned block | 600 |
| Capacity fill at c=0.999 / 0.99 | 80% / 95% of N×4e6 |
| Inflow λ multiplier at c=0.999 / 0.99 | 2.0 / 1.0 |
| Inclusion confidence | 0.999 at N=1; 0.99 at N≥2 |
| Admit EMA half-life | ~150 s |
| Inflow horizon cap | 600 s |
| Blend N0 | 6 blocks |
| Flow buckets and candidates | 100 geometric steps per decade from min relay through 1000 sat/vB, plus an open top |
| Flow fullness | decayed admitted WU / ~1.44e6 WU (150 s half-life) |
| Historical confidence | 0.999 at N=1; 0.95 at N≥2, interpolated in log-rate |
| Analog lookback | `clamp(N/4, 3, 144)` hurdle blocks |
| Analog band / min neighbors / ready | ×1.25 / 200 / 2000 windows |
| History budget | 1 GiB of `txstat.body` cells |
| History file | snapshot every 144 connects + per-connect journal |


### Confirm-memory / block history

Confirmed mempool entries' feerates on `remove_for_block` fill a 64-sample
ring (**N=1 p90 clip**). Process-local.

`R_hist` reads the **chain**, not this pool. Each block's hurdle is the
vsize-weighted p10 of individual confirmed transaction feerates, from its
stored `txstat` rows alone; rates below min relay (out-of-band or zero-fee
inclusions) are dropped first. A block with no hurdle left (coinbase-only, or
every tx below min relay) is not an observation: windows count only blocks
that carried transactions, so an N-block confidence is conditional on those
blocks carrying transactions. On mainnet such blocks are ~0.1% and almost
always isolated; counting them as misses made the 99.9% 1-block target
unanswerable about half the time.

**Analog windows.** For target N, each window of N hurdle blocks pairs the
median hurdle of the `L = clamp(N/4, 3, 144)` blocks before it with the lowest
hurdle inside it (a tx at that rate would have beaten some block's p10). An
estimate keeps the windows whose lookback median is within ×1.25 of the
current lookback median (at least the 200 nearest) and takes the 95% quantile
of their lowest hurdles (99.9% at N=1). When that percentile falls between two
adjacent hurdles, the rate is the log-rate interpolation, rounded to the
nearest sat/kvB. A repeated hurdle stays that hurdle. Targets 4 through 10
publish the same rate when most blocks share one: a longer window's minimum
moves only after a cheaper hurdle is common. Windows that started inside a spike
stop steering a calm market, and a spike in progress finds the windows that
started inside past spikes. A target answers once it holds 2000 windows: at
least 2000 + L + N − 1 hurdle blocks.

On a mainnet backtest over ~2.9 years of `txstat` (every 36 blocks, each
estimate from the 1 GiB before it, scored against the next N hurdle blocks),
analog windows covered 99.1–99.5% at 2–144 blocks and 99.91% at 1 block
(99.9% target), at a median 2.0–3.3× the realized hurdle (7.1× at 1 block).
Those far-target multiples are the previous 99% quantile.
Flat quantiles over the same history covered 98.5–99.8% at 9–20× (46× at 1
block), because a spike kept steering estimates for months. Long targets
(504/1008) cover ~97.7%; their windows overlap so heavily that about a
hundred are independent. These are empirical predictions from overlapping
windows, not formal statistical guarantees.

**Budget and preload.** History is keyed by height and bounded by 1 GiB of
`txstat.body` cells (8 B per tx, coinbase included): ~31k mainnet blocks at a
2026 tip. When relay turns on, the node restores the history file, then scans
backward from the tip until the budget is met, reading no `spent.body` data or
transaction bodies and no height it already holds (~12 s per GiB cold on the
agent VM). A connect at `h` replaces any branch above it. Analog windows are
kept in step as blocks are added or dropped at either end; an insert between
held heights rebuilds them once before the next estimate, and estimates are
recomputed only after the history changes. RAM: ~5.5 MiB of windows at 11
targets; CPU: ~2 ms per new block for all targets.

**History file.** `mempool/fee_history` is a snapshot (height, hurdle, cells
per height, plus the newest 144 block hashes), rewritten after each preload
and every 144 connects (~500 KiB, one fsync). `mempool/fee_history.log` is a
journal of 52-byte records (height, cells, hurdle, block hash, check) appended
per connect without fsync. On load the journal replays onto the snapshot only
if it extends that snapshot, stops at a torn record, and everything above the
newest stored hash still on the best chain is dropped (a reorg while down).
The file is a cache of `txstat`: a damaged, foreign, or other-version file is
dropped with a log line and those heights are read from the chain again. The
preload log line reports heights from the file, heights read, skips,
failures, ready targets, and elapsed time.

### Histogram / relayfee

Live chunk histogram remains a **stock** snapshot (transparency). Every
defined number is at least `max(mempoolminfee, 0.1 sat/vB)` on every
surface (RPC, Electrum, Esplora): Libre min relay floors the computed table,
and `mempoolminfee` is applied when a request reads it.

## Template readiness (non-goal: full mining)

Frontier/chunk snapshots are shaped so a later block-template consumer can reuse
mining order. This document does **not** specify `getblocktemplate`, coinbase
construction, or witness nonces.

## Non-goals

- Full Core `estimatesmartfee` Bayesian wait-time tracker (we use per-block
  included p10s, not first-seen delay buckets)
- Full multi-node flow aggregation / peer bandwidth models
- Changing Libre min relay, dust, or full-RBF defaults
- Persisting flow meters across process restart (process-local; fee
  history is persisted, flow is not)

## Related

- Mempool admission correctness: findings [010](./external_findings/010-mempool-confirmed-spentness.md),
  [011](./external_findings/011-mempool-structural-chain-context.md)
- Policy: `rbitcoin-consensus::policy`, OPERATOR Libre table
- Accept path: staged prepare / script_pool / commit; coalesced durable writes
