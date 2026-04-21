# torch_perp — Design

## Status

**v1 shipped.** 10 instructions, **57 Kani proofs**, **19 simulator scenarios**, TypeScript SDK, e2e test on surfpool.

- Program ID: `852yvbSWFCyVLRo8bWUPTiouM5amtw6JxctgS9P4ymdH`
- Source: `programs/torch_perp/`
- SDK: `packages/sdk/`
- Sim: `sim/torch_perp_sim.py`
- Formal proofs: see [verification.md](./verification.md)
- Anchor 0.32.1, rust edition 2021

What's in v1: vAMM perp with IMR/MMR margin, permissionless liquidation with liquidator bonus, percolator A/K proportional scaling for bad debt absorption, TWAP observation ring feeding a classic funding-rate mechanism (premium from mark vs spot TWAP accrues into a cumulative index; settled on close/liquidate/partial-close), partial position close.

## Goal

A universal vAMM perpetual futures primitive for Solana. Anyone can permissionlessly open a perp market for any SPL/Token-2022 mint that has a Raydium CPMM (or DeepPool) pool paired with WSOL. Users post SOL collateral, open long/short positions, marked-to-market via vAMM, liquidations permissionless and isolated per market.

**Two underlying tiers:**

1. **Torch tokens (premium tier).** Torch provides structural guarantees that make underwriting trivial: immutable supply (mint authority revoked), guaranteed deep pool (LP burned at migration), structured Token-2022 transfer-fee flow, known pool lifecycle. These become parameter affordances — higher safe leverage, tighter risk params, lower insurance requirement per unit of OI.
2. **Generic tokens (standard tier).** Any SPL/Token-2022 mint with a Raydium CPMM WSOL pair. The perp works identically at the math level; what changes is the *underwriting confidence*. Operators choosing to initialize markets on arbitrary mints are taking on the risk that supply could be inflated, pools could be pulled, or fees could be reconfigured.

The program makes no distinction at the handler level — `initialize_market` takes any mint + any CPMM pool. The distinction is economic, not structural. This keeps the program small (~1k LOC) and composable: torch is a *source of high-quality underlying*, not a dependency.

## Non-Goals

- No external collateral assets (no USDC, no wBTC). SOL only, to preserve torch's no-external-dependency ethos.
- No cross-margin across markets. Each position is isolated.
- No hedging mode. One position per user per market.
- No orderbook. vAMM only.
- No external oracles. Mark price comes from the vAMM; TWAP index for funding comes from the Raydium CPMM spot pool the market references.
- No governance. Market parameters are immutable per-market once initialized.
- No frozen-market failure mode. Bad debt beyond the insurance fund is absorbed by percolator-style proportional scaling, not socialized losses nor protocol shutdown.

## Core Concepts

### PerpMarket (one per underlying mint)
A vAMM with its own base/quote reserves. Base = the underlying mint, quote = SOL. Tracks cumulative funding, TWAP observations, open interest per side, and the Raydium CPMM pool it reads for spot price reference.

### PerpPosition (one per user per market)
A leveraged exposure: size (signed, + long / - short), collateral (SOL), entry price, last-seen cumulative funding snapshot. PnL computed on close/liquidate using current mark.

### Mark Price
vAMM price: `quote_reserve / base_reserve`. Moves with every open/close via standard x*y=k.

### Index Price (Oracle substitute)
TWAP of the spot pool (Raydium or DeepPool) reserves, computed from an on-chain ring buffer of observations. Used for funding rate calculation only — **not** for mark price. Settlement uses vAMM.

### Funding Rate
Classic perp funding with a single cumulative index.

- `premium_scaled = mark_price_scaled - index_price_scaled` (signed, POS_SCALE precision)
  - `mark_price_scaled = quote_reserve × POS_SCALE / base_reserve` (vAMM price)
  - `index_price_scaled` = TWAP of the spot pool's reserves across the ring window
- `update_funding` crank: accrues `premium × slots_elapsed / funding_period_slots` into `cumulative_funding_long`
- Single-index design: shorts auto-flip sign via signed `base_asset_amount` on settlement. `cumulative_funding_short` is mirrored as a convenience field for downstream readers.
- Settlement: `funding_owed = base × (cumulative_current - snapshot) / POS_SCALE` applied on close, partial close, and liquidation.

Per-unit zero-sum verified: `funding_owed(+B, c, s) == -funding_owed(-B, c, s)` exactly. Kani-proven.

### Insurance Fund
A SOL pool owned by the program, funded by a fraction of trading fees. First line of defense against bad debt: liquidation shortfalls draw from insurance before any position is touched. If the fund is exhausted, solvency falls to the percolator layer below rather than freezing the market.

### Percolator Solvency Layer
When bad debt exceeds the insurance fund, torch_perp uses the A/K proportional-scaling math from [@aeyakovenko's percolator research](https://github.com/aeyakovenko/percolator). Losses are distributed across active positions via the A and K indices; the market moves through Normal → DrainOnly → ResetPending → Normal recovery phases rather than freezing. Our use of the math is re-verified in torch_perp's context via Kani.

## Architecture

torch_perp has **zero code-level dependency on torch_market**. The program:
- Makes no CPI into torch_market
- Reads no torch_market account (no `BondingCurve`, no `Treasury`)
- Has no hardcoded torch_market program ID
- Shares no accounts with torch_market

What it does read:
- The caller-provided `mint` account (for decimals lookup)
- The caller-provided `spot_pool` + vault accounts for a Raydium CPMM pool (validated: owner = Raydium CPMM program, vault linkage, WSOL pairing)
- `Clock` sysvar for slot-based math

That's it. The `initialize_market` handler validates the pool's *shape* (Raydium CPMM, WSOL-paired), not the token's *provenance* (torch-launched vs. anything else). Any mint with a valid Raydium CPMM WSOL pool can have a perp market.

**The composition with torch is economic, not structural.** When the underlying IS a torch token, the market operator can lean on torch's guarantees to set higher safe leverage, lower insurance requirements, or tighter risk bands. When it isn't, the operator has to underwrite the risk themselves (is the mint authority revoked? is LP burned? what's the supply cap?).

Pool-agnostic at the code layer: same program supports any Raydium-CPMM-style pool. DeepPool is plug-compatible via the same pool_state + vaults interface.

## Accounts

```
GlobalConfig (singleton)
├── authority: Pubkey          // market-init authority (immutable after init)
├── protocol_treasury: Pubkey  // SOL vault receiving non-insurance fee share
├── fee_rate_bps: u16          // taker fee on opens/closes
├── insurance_fund_cut_bps: u16 // % of fees → per-market insurance
├── bump: u8

PerpMarket (one per underlying mint)
├── mint: Pubkey               // SPL or Token-2022 mint with a WSOL-paired CPMM pool
├── spot_pool: Pubkey          // Raydium CPMM or DeepPool-compatible pool
├── spot_vault_0: Pubkey
├── spot_vault_1: Pubkey
├── is_wsol_token_0: bool      // pool orientation cache
├── base_asset_reserve: u128   // vAMM base (tokens)
├── quote_asset_reserve: u128  // vAMM quote (lamports)
├── vamm_k_invariant: u128     // base × quote, immutable per market
├── initial_margin_ratio_bps: u16      // e.g., 1000 = 10% min collateral → 10x max leverage
├── maintenance_margin_ratio_bps: u16  // e.g., 625 = 6.25%
├── liquidation_penalty_bps: u16       // bonus to liquidator
├── cumulative_funding_long: i128
├── cumulative_funding_short: i128
├── last_funding_slot: u64
├── funding_period_slots: u64
├── open_interest_long: u64    // base asset
├── open_interest_short: u64   // base asset
├── twap_observations: [Observation; N]  // ring buffer
├── twap_head: u16
├── insurance_balance: u64     // lamports held for this market's bad debt
├── // percolator solvency layer
├── a_index: u128              // percolator A — starts at POS_SCALE (1e18), decreases on scaling events
├── k_index: i128              // percolator K — cumulative PnL events
├── recovery_phase: u8         // 0 = Normal, 1 = DrainOnly, 2 = ResetPending
├── epoch: u32                 // increments on ResetPending → Normal transition
├── bump: u8

Observation
├── slot: u64
├── cumulative_sol: u128       // sum of (sol_reserve × slot_delta)
├── cumulative_token: u128     // sum of (token_reserve × slot_delta)

PerpPosition (one per user per market)
├── user: Pubkey
├── market: Pubkey
├── base_asset_amount: i64     // + long, - short (this is percolator's basis_i)
├── quote_asset_collateral: u64 // SOL in lamports (percolator's C_i)
├── entry_notional: u64        // abs(base) × entry_price at open
├── last_cumulative_funding: i128 // funding snapshot at last update
├── a_basis_snapshot: u128     // percolator A at entry/last update
├── k_snapshot: i128           // percolator K at entry/last update
├── matured_pnl: i64           // realized PnL held in reserve (subject to haircut H)
├── open_epoch: u32            // market.epoch at position open
├── open_slot: u64
├── bump: u8
```

## Instructions

```
initialize_global_config(fee_rate_bps, insurance_fund_cut_bps)
    → One-time. Records authority, protocol_treasury, fee params.
      Bounded: fee_rate_bps ≤ 200, insurance_fund_cut_bps ≤ 10_000.

initialize_market(imr_bps, mmr_bps, liq_penalty_bps, funding_period_slots, vamm_quote_reserve)
    → Permissionless after token migration. Validates Raydium CPMM pool shape
      (owner + vault linkage + WSOL pair). Caller specifies SOL-side vAMM depth;
      base-side derived deterministically to match spot price at init.
      Bounds: mmr < imr, both > 0.

open_position(base_amount, collateral_lamports, max_price_impact_bps)
    → Signed base_amount: +N long, -N short. vAMM swap + IMR gate + price-impact gate +
      fee collection split between insurance + protocol_treasury.
      Snapshots market.a_index and market.k_index to the position (percolator tracking).
      Rejected when market.recovery_phase != Normal.

close_position(min_quote_out)
    → Inverse vAMM swap. Realized PnL = swap output - entry_notional (long) or
      entry_notional - swap cost (short). Applies percolator K delta for
      accumulated bad-debt share. Insurance funds winner PnL / absorbs loser losses.
      Position account closed (rent back to user).

deposit_collateral(amount)
    → Move SOL from user to position PDA. Always allowed (adding margin is safe).

withdraw_collateral(amount)
    → Requires post-withdrawal equity ≥ IMR × notional (stricter than maintenance,
      prevents withdrawing into a state you couldn't open).

liquidate_position()
    → Permissionless. Gate: position below maintenance margin.
      Liquidator receives liquidation_penalty from collateral.
      Residual loss: insurance fund first, then percolator A-scaling if insufficient.
      Position account closed (rent to position_owner).

update_funding()
    → Permissionless crank. Advances TWAP observation ring, reads oldest-to-
      newest observation span as the TWAP index, computes premium = mark - index,
      accrues premium × slots_elapsed / funding_period into cumulative_funding_long.

write_observation()
    → Permissionless crank. Appends new observation to the TWAP ring buffer.
      Also called internally by any handler that touches the spot pool.
```

9 instructions total.

## Economic Model

### Fees
- Taker fee on `open_position` and `close_position`: e.g., 10 bps of notional.
- Fee split: `(1 - insurance_fund_cut_bps)` to protocol (torch), `insurance_fund_cut_bps` to insurance fund.

### Funding
- Premium: `mark_scaled - index_scaled` where both are POS_SCALE-scaled prices
- Accrual: `premium × slots_elapsed / funding_period_slots` per `update_funding` call
- Settlement: applied on close / partial_close / liquidation via `funding_owed`
- Zero-sum at the per-unit level (Kani-proven)
- No external funds; internal transfer between position holders via the cumulative index

### Liquidation
- Position liquidatable when: `collateral + unrealized_pnl - accrued_funding < maintenance_margin`
- Liquidator receives `liquidation_penalty_bps × notional`
- If position has shortfall (debt exceeds collateral): draw from insurance fund first
- If insurance fund exhausted: fall through to percolator layer

### Insurance Fund
- Accumulates from fee cut on every trade
- First line of defense against bad debt
- Drawn down during liquidation shortfalls before any position is touched

### Percolator Layer — Bad Debt Beyond Insurance
When a liquidation shortfall exceeds the insurance balance, the excess is distributed via percolator's math:

- **A scaling:** market's `a_index` decreases proportionally. Every active position's effective size shrinks when read: `effective_base_i = floor(base_asset_amount_i × a_index / a_basis_snapshot_i)`
- **K accumulation:** residual PnL delta is folded into `k_index`. Positions realize this on close via: `pnl_delta_i = floor(|base_asset_amount_i| × (k_index - k_snapshot_i) / (a_basis_snapshot_i × POS_SCALE))`
- **H haircut:** matured PnL in `PerpPosition.matured_pnl` is senior to unrealized PnL but junior to collateral. When `Residual = max(0, V - C_tot - I) > 0`, applies haircut `h = min(Residual, matured_pnl_tot) / matured_pnl_tot` before further A-scaling is needed.
- **Recovery phases:**
  - `Normal`: full trading allowed, `a_index ≥ PRECISION_THRESHOLD`
  - `DrainOnly`: `a_index` has dropped below threshold — no new OI, positions close only
  - `ResetPending`: open interest reached zero in DrainOnly — snapshot `k_index`, increment `epoch`, reset `a_index = POS_SCALE`, return to `Normal`
- **Invariant preserved:** sum of all effective PnL never exceeds what exists in the vault (Kani-verified)

### Composition Note
The protocol is universal at the code level. When the underlying is a torch token, torch's ex-ante guarantees (immutable supply, guaranteed deep pool, structured fees) give the market operator more room to set aggressive leverage / risk params — because the worst-case scenarios torch eliminates (supply dilution, rug pull, mutable fees) can't happen. When the underlying is a generic SPL/T22 mint, the operator has to underwrite those risks directly, typically by setting more conservative params.

The A/K math handles residual bad debt fairly regardless of underlying — the scaling doesn't care whether the mint is torch-launched or not. What changes is how often it's needed: on torch tokens, the upstream defenses catch more events before the A/K layer sees them.

## Empirical Properties (from `sim/torch_perp_sim.py`)

Full scenario-by-scenario breakdown lives in [sim.md](./sim.md). The simulator runs 19 scenarios ranging from unit tests to Monte Carlo stress to coordinated adversarial squeezes, using the same math ported to pure Python. Key findings that shape design choices:

1. **Protocol is quiet and profitable under realistic conditions.** Scenarios 9, 10, 14 consistently show hundreds of opens, single-digit liquidations, **zero bad debt**, and tens of SOL in fees accruing to the insurance fund. Insurance grows faster than bad-debt events drain it. This is what justifies v1 shipping without funding — basic leveraged exposure + self-compounding safety buffer.

2. **Percolator is tail-risk insurance, not a frequent mechanism.** Across 200 Monte Carlo runs (Scenario 11) and all random-flow scenarios, percolator never triggered. It only activates under engineered adversarial coordination (Scenarios 4, 5, 6, 13). This is the correct property for a solvency layer: dormant under normal operation, bounded under extreme stress.

3. **Bad debt requires coordinated adversarial behavior, not volatility.** Scenario 12 (arb present vs absent) and Scenario 11 (periodic shocks up to σ = 14% daily) both show zero bad debt under random directional flow. The vAMM is self-consistent when trade flow is balanced; pressure that wrecks positions has to be *directed* (e.g., the squeeze in Scenario 13 — 5 coordinated longs followed by a counter-dump).

4. **Close-out slippage is the real tail risk.** Mark-to-market margin uses instantaneous vAMM price (ignoring slippage on close), but actual liquidation has to execute a swap. A position that looks "safe" at mark can have worse realized PnL. Torch_perp's answer: accept slippage exists, bound it via insurance → percolator → recovery cycle. Scenario 4 shows a whale with +1.7 SOL mark-equity ending at −11.5 SOL shortfall after the close-out swap; percolator absorbed it, pool kept operating.

5. **Defense layers are independently sufficient.** Scenario 15 (10x slower liquidator) and Scenario 14 (thin pool + max leverage + high vol) each stress one defense layer to failure without producing bad debt. The redundancy is real — no single layer is load-bearing alone. Formal defense stack:

   | Layer | Applies to | Catches |
   |---|---|---|
   | Structural underlying (torch) | Torch tokens only | Supply dilution, rug, fee manipulation — eliminated by torch's mint-authority revoke + LP burn + immutable Token-2022 config |
   | Initial margin ratio (market param) | All markets | Overleveraged opens |
   | Price-impact gate on open | All markets | Fat-finger opens in thin pools |
   | Permissionless liquidation w/ bonus | All markets | Price moves against positions |
   | Insurance fund (50% fee share) | All markets | Shortfalls under ~10% of TVL per market |
   | Percolator A/K scaling | All markets | Coordinated squeezes (tail risk) |

   Only the first layer is torch-conditional. Markets on generic tokens inherit every layer from row 2 down; they just lack the upstream structural guarantees that torch tokens bring for free.

These findings shape several design commitments:
- **No funding rate in v1** — the sim shows the market self-regulates without it. Funding is an optimization for the wrong problem.
- **Percolator included from day 1** — Scenario 13 proves the attack it catches is real. Not including it would mean the one engineered adversarial pattern takes down the market.
- **Insurance fund from fees** — no need for external capital, token emissions, or governance-controlled reserves. The fee stream compounds the buffer.
- **Permissionless liquidation is sufficient** — Scenario 15 shows a slow liquidator still preserves solvency as long as it exists. No need for specialized keepers with staked collateral.

## Composition with torch

The code-level answer: torch_perp has no dependency on torch_market. The program is a standalone vAMM perp layer for any Raydium CPMM pool.

The economic answer: torch tokens are the *best underlying* the perp can run on, for specific structural reasons.

### What torch tokens give you (when the underlying is a torch token)
- **Immutable supply** — mint authority revoked at migration. No dilution attack against a position.
- **Guaranteed deep pool** — LP burned at migration. Liquidity cannot be pulled out from under a position.
- **Structured Token-2022 transfer fee** — predictable per-transfer haircut, feeds insurance growth across the token's circulation (not just perp trades).
- **Known pool lifecycle** — torch controls the migration target PDA. You know exactly which Raydium pool backs the token, derived deterministically.
- **Deep-audited underlying protocol** — 71 Kani proofs + formal audit on torch_market, 16 on DeepPool. The token itself is verified substrate.

### What torch_perp does not touch
- Torch's Treasury (separate program, separate funds — perp has its own per-market insurance)
- Torch's lending market
- Torch's bonding curve
- `torch_market` program ID (not hardcoded anywhere in torch_perp)

### Settlement flow when a position closes:
- vAMM handles the perp accounting internally
- No actual swap against the spot pool on close — just internal settlement
- Collateral paid out from program-owned SOL vault
- The spot pool is only **read**, never written to by torch_perp

This preserves isolation: torch_perp bugs cannot affect torch_market positions, and vice versa. It also means non-torch tokens are first-class — a perp market on BONK reads BONK's Raydium pool the exact same way a torch-token market reads torch's migrated pool.

**Market init is permissionless for any mint.** The handler validates pool *shape* (Raydium CPMM owner, vault linkage, WSOL pair), not token *provenance*. Market operators choose the risk params. Conservative: generic tokens. Aggressive: torch tokens with structural guarantees backing the underwriting.

## Parameters (shipped v1)

All values are constants in `programs/torch_perp/src/constants.rs` and mirrored in `packages/sdk/src/constants.ts`.

| Parameter | Value | Rationale |
|---|---|---|
| `INITIAL_MARGIN_RATIO_BPS` | 1_000 (10%) | Max 10x leverage at open. |
| `MAINTENANCE_MARGIN_RATIO_BPS` | 625 (6.25%) | Industry baseline (matches Drift). Strictly less than IMR. |
| `LIQUIDATION_PENALTY_BPS` | 500 (5%) | Standard bonus to incentivize permissionless liquidation. |
| `FEE_RATE_BPS` | 10 (0.10%) | Taker fee on open + close. |
| `INSURANCE_FUND_CUT_BPS` | 5_000 (50%) | Half of fees feed insurance; other half to protocol treasury. |
| `FUNDING_PERIOD_SLOTS` | 9_000 (~1hr) | Denominator in premium accrual. |
| `TWAP_RING_SIZE` | 32 | Ring buffer of 32 observations, drives funding index. |
| `TWAP_WINDOW_SLOTS` | 1_500 (~10min) | Target smoothing window for funding index. |
| `POS_SCALE` | 1e18 | Percolator A/K precision (standard 18-decimal fixed point). |
| `PRECISION_THRESHOLD` | POS_SCALE / 1_000 | A-index floor before entering DrainOnly. |
| `MATURED_WARMUP_SLOTS` | 256 (~100s) | Reserve-to-matured PnL conversion delay (percolator H layer). |

**Market init is permissionless for any valid CPMM pool.** Caller specifies the vAMM's SOL-side depth (`vamm_quote_reserve`); base-side is derived deterministically from the spot pool's price to align mark with spot at init. The defaults above reflect a balanced choice suitable for torch tokens; for markets on riskier underlyings, initializers may pass tighter IMR/MMR/penalty params within the protocol bounds.

**Partial close is v2.** v1 closes the full position or liquidates the full position — no partial closes.

## Out of Scope

- Cross-collateral (one collateral, many positions)
- Portfolio margin
- Limit orders
- UI (SDK returns `TransactionInstruction`s; UI is client concern)
- Oracle fallbacks
- Governance
- Program upgrade path (immutable once deployed)

## Shipped state

1. ✅ Design locked. Parameters frozen for v1.
2. ✅ Contracts: `state.rs`, `contexts.rs`, `errors.rs`, `constants.rs`, `pool.rs`.
3. ✅ Math: `math.rs` with 11 pure functions, `Option<T>` returns, no Anchor types.
4. ✅ Handlers: 9 instructions, all `cargo check` clean, `anchor build` passes BPF stack checks (all large accounts boxed).
5. ✅ Kani proofs: **57 harnesses** over the math layer. All passing. See [verification.md](./verification.md).
6. ✅ Simulator: `sim/torch_perp_sim.py` — 19 scenarios including Monte Carlo stress, percolator activation, sandwich defense, funding zero-sum, sustained-premium drain, partial-close symmetry.
7. ✅ SDK: `packages/sdk/` — 10 files, all 9 transaction builders, quote previews, liquidation scanner, state decoders.
8. ✅ E2E test: `packages/sdk/tests/test_e2e.ts` — 7-phase surfpool test composing torchsdk (create/bond/migrate) with torch_perp SDK (init/open/close/crank/liquidate). Verifies vAMM isolation from spot DEX trading empirically.
9. ✅ Program keypairs: `keys/program.json` (`852yvbSWFCyVLRo8bWUPTiouM5amtw6JxctgS9P4ymdH`), `keys/deploy.json`. Deployed on surfpool.

## What's next

- Devnet deployment + devnet integration testing
- Mainnet deployment (pending audit + bug bounty)
- Future exploration (not committed): partial liquidation, cross-margin, observation-buffer tuning based on prod data
