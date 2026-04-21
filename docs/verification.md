# Formal Verification Report

## TL;DR

torch_perp's core math is formally verified with [Kani](https://model-checking.github.io/kani/), the Rust model checker from AWS. The pure-math layer — vAMM swap arithmetic, margin and PnL calculations, fee math, TWAP cumulative updates, and liquidation penalty — has been proven correct for every input within realistic protocol ranges. No SOL can be created from nothing, no output can exceed pool reserves, no fresh position can be opened into an immediately-liquidatable state.

This is **not** a security audit. It proves the arithmetic is correct, but does not cover access control, account validation, CPI composition, or economic attacks. See [What Is NOT Verified](#what-is-not-verified) for full scope limitations.

**41 proof harnesses. All passing. Zero failures.**

Composed across the torch stack:

| Layer | Proofs |
|---|---|
| [torch_market](https://github.com/mrsirg97-rgb/torch_market) | 71 |
| [deep_pool](https://github.com/mrsirg97-rgb/deep_pool) | 16 |
| **torch_perp** (this repo) | **41** |
| **Stack total** | **128 formally verified invariants** |

---

## Overview

torch_perp's core arithmetic is formally verified using Kani. Kani exhaustively proves properties hold for **all** valid inputs within constrained ranges — not just sampled test cases. Every proof harness uses concrete values spanning the protocol's operating range (dust → typical → max-scale) to avoid SAT solver explosion on wide-integer arithmetic while still covering the spectrum.

**Tool:** Kani Rust Verifier
**Target:** `torch_perp` v0.1.0
**Harnesses:** 41, all passing
**Source:** `programs/torch_perp/src/kani_proofs.rs`
**Math under proof:** `programs/torch_perp/src/math.rs`

The math module is a pure-function layer with no Anchor types, no I/O, no dependencies beyond primitive integer arithmetic. Handlers invoke this math via `.ok_or(MathOverflow)?` to convert pure `Option<T>` returns into on-chain `Result<T>`. This separation means Kani reasons about exactly the math that runs on-chain, not a simplified replica.

## What Is Formally Verified

### vAMM Swap (Harnesses 1-12)

The constant-product swap mechanism that determines mark price and settles positions.

| Harness | Property | Notes |
|---|---|---|
| `verify_vamm_buy_base_k_non_decreasing` | `new_base × new_quote ≥ base_r × quote_r` after any buy | k invariant cannot decrease — rounding stays in pool |
| `verify_vamm_sell_base_k_non_decreasing` | Same for sells | |
| `verify_vamm_buy_zero_input_is_identity` | Zero quote in → zero base out, reserves unchanged | |
| `verify_vamm_sell_zero_input_is_identity` | Zero base in → zero quote out, reserves unchanged | |
| `verify_vamm_roundtrip_quote_base_quote_no_extraction` | `quote → base → quote` yields ≤ original quote | No value extraction via round-trip trading |
| `verify_vamm_roundtrip_base_quote_base_no_extraction` | Same for reverse | |
| `verify_vamm_buy_base_output_bounded_by_reserve` | `base_out ≤ base_reserve` | Swap never pays more than pool holds |
| `verify_vamm_sell_base_output_bounded_by_reserve` | `quote_out ≤ quote_reserve` | |
| `verify_vamm_buy_pushes_price_up` | After buy, `new_price ≥ old_price` | Direction safety for price-impact gate |
| `verify_vamm_sell_pushes_price_down` | After sell, `new_price ≤ old_price` | |
| `verify_vamm_buy_output_monotonic_in_input` | Larger quote in → larger-or-equal base out | Can't grief via split trades |
| `verify_vamm_sell_output_monotonic_in_input` | Larger base in → larger-or-equal quote out | |
| `verify_vamm_buy_price_impact_monotonic` | Larger trade → larger-or-equal price impact | `max_price_impact_bps` gate cannot be bypassed via split trades |

### TWAP Cumulative Math (Harnesses 13-16)

Observation ring buffer arithmetic used by `write_observation`.

| Harness | Property |
|---|---|
| `verify_advance_cumulative_additivity` | `cum_new == cum_prev + reserve × slot_delta` exactly |
| `verify_advance_cumulative_monotonicity` | `cum_new ≥ cum_prev` always |
| `verify_advance_cumulative_zero_delta_is_identity` | Zero slot delta → cumulative unchanged |
| `verify_advance_cumulative_zero_reserve_is_identity` | Zero reserve → cumulative unchanged |

### Fees (Harnesses 17-21)

Fee computation + insurance/treasury split math used by every open and close.

| Harness | Property |
|---|---|
| `verify_compute_fee_bounded_by_notional` | `fee ≤ notional` when `fee_rate_bps ≤ BPS_DENOMINATOR` |
| `verify_compute_fee_monotonic_in_notional` | Larger notional → larger-or-equal fee |
| `verify_split_fee_conservation` | `to_insurance + to_protocol == fee` for any valid `cut_bps` |
| `verify_split_fee_zero_cut_is_all_protocol` | `cut_bps = 0` → all fee to protocol |
| `verify_split_fee_full_cut_is_all_insurance` | `cut_bps = 10000` → all fee to insurance |

### Position Valuation & PnL (Harnesses 22-28)

Mark-to-market notional + unrealized PnL math that drives margin decisions.

| Harness | Property |
|---|---|
| `verify_position_notional_zero_base` | Zero position size → zero notional |
| `verify_position_notional_formula` | `notional == |base| × quote / base` (floor) |
| `verify_unrealized_pnl_long_profits_when_price_rises` | Long with `current > entry` has positive PnL |
| `verify_unrealized_pnl_long_loses_when_price_falls` | Long with `current < entry` has negative PnL |
| `verify_unrealized_pnl_short_profits_when_price_falls` | Short with `current < entry` has positive PnL |
| `verify_unrealized_pnl_short_loses_when_price_rises` | Short with `current > entry` has negative PnL |
| `verify_unrealized_pnl_flat_is_zero` | Zero position → zero PnL regardless of prices |
| `verify_pnl_long_short_symmetry` | Long and short at identical params have exactly opposite PnL |
| `verify_unrealized_pnl_bounded_by_max_notional` | `|pnl| ≤ max(entry, current)` — no absurd values from overflow/sign bugs |

### Margin Requirements (Harnesses 29-36)

Initial margin + maintenance margin + liquidation penalty math.

| Harness | Property |
|---|---|
| `verify_required_margin_formula` | `required == notional × ratio_bps / 10_000` (floor) |
| `verify_required_margin_monotonic_in_notional` | Larger notional → larger-or-equal requirement |
| `verify_required_margin_monotonic_in_ratio` | Higher ratio → larger-or-equal requirement (raising MMR can never relax a position) |
| `verify_check_initial_margin_threshold` | `collateral ≥ required ⇔ passes IMR check` |
| `verify_is_above_maintenance_negative_equity_is_liquidatable` | Zero or negative equity → always liquidatable |
| `verify_is_above_maintenance_threshold` | `equity ≥ required ⇔ above maintenance` |
| `verify_imr_implies_above_maintenance` | Any position passing IMR with `collateral > 0` is above MMR when `mmr < imr` — fresh positions cannot be opened into an already-liquidatable state |
| `verify_liquidation_penalty_formula` | `penalty == notional × penalty_bps / 10_000` |
| `verify_liquidation_penalty_bounded_by_notional` | `penalty ≤ notional` when `penalty_bps ≤ BPS_DENOMINATOR` |
| `verify_liquidation_penalty_monotonic_in_notional` | Larger notional → larger-or-equal penalty |

## What Is NOT Verified

Kani proofs cover the **pure math layer** — the arithmetic that any handler calls. They do **not** cover:

- **Access control** — whether the right signer is required for each instruction (Anchor `Signer<'info>` constraints handle this; covered by audit, not Kani)
- **Account validation** — whether Anchor's `seeds`/`bump`/`has_one`/`close` constraints are correctly specified
- **CPI composition** — interactions with the system program (SOL transfers) or the spot pool (Raydium CPMM / DeepPool) are out of scope
- **TWAP observation invariants** — the ring buffer write pattern is covered via the per-update `advance_cumulative` math, but cross-observation properties (e.g., window size × slot delta) are not
- **Percolator state machine** — the `apply_percolator_scaling` function lives inside `handlers/liquidate_position.rs` rather than `math.rs`. Its formula is informal at this layer; in practice, the sim (`sim/torch_perp_sim.py`) validates the percolator behavior across scenarios.
- **Funding rate** — v1 ships with funding disabled (cumulative funding indices stay at 0). v1.1 will introduce funding math and corresponding Kani proofs.
- **Reentrancy or race conditions** — Solana's single-threaded per-tx execution makes traditional reentrancy impossible, but CPI return-value handling should still be reviewed by audit.
- **Economic attacks** — sandwich attacks, coordinated squeezes, and adversarial liquidation timing are validated in the simulator (`sim/torch_perp_sim.py`), not in the formal proofs. The sim runs 15 scenarios including Monte Carlo stress across 200+ simulation runs.

## Running the Proofs

Install [Kani](https://model-checking.github.io/kani/install-guide.html), then from the repo root:

```bash
cargo kani
```

Kani will compile the `torch_perp` crate with `cfg(kani)` enabled and run every `#[kani::proof]` harness. All 41 must pass. Failures indicate a real correctness issue in the math (as found once already during torch_perp development — a vAMM rounding direction bug caught by the k-invariant proofs before it could ship).

## Simulator

Complementary to the formal proofs is `sim/torch_perp_sim.py` — a pure-Python economic simulator that ports the same math and runs scenario-based and Monte Carlo stress tests:

```bash
python3 sim/torch_perp_sim.py
```

15 scenarios cover: basic open/close, leverage enforcement, vAMM roundtrip, liquidation cascades, percolator activation, DrainOnly recovery, sandwich defense, realistic multi-day trading with arb, flash crashes, Monte Carlo across volatility regimes, arb-present vs arb-absent, and three engineered stress scenarios. See inline docstrings for scenario-specific findings.

## Versioning

Proofs are versioned with the program. Every change to `math.rs` requires either an updated proof or a documented reason why existing proofs still cover it. New math functions must ship with at least a formula proof and any bounds/monotonicity properties relevant to their use site.
