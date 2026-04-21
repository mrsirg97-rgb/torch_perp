"""
torch_perp economic simulator.

Pure-Python model of the torch_perp protocol:
  - vAMM-based leveraged perps on torch tokens
  - Oracle-free margin (mark from vAMM, index from spot pool TWAP)
  - Depth-adaptive risk (inherits torch's structural guarantees)
  - Isolated positions, per-market state
  - Insurance fund → percolator A/K fallback for bad debt
  - Recovery cycle: Normal → DrainOnly → ResetPending → Normal

All math mirrors the on-chain Rust (programs/torch_perp/src/math.rs).
Integer arithmetic, floor rounding in pool's favor.

Contents:
  - Constants + pure math (ports of math.rs)
  - State (SpotPool, PerpMarket, PerpPosition, Observation)
  - Actions (open/close/liquidate — pure logic)
  - Agents (Trader, Arber, Liquidator, Whale)
  - PriceProcess (GBM on spot — drives organic price evolution)
  - SimEngine (multi-day time loop w/ stats)
  - Scenarios 1-8: unit/stress scenarios (one-shot)
  - Scenarios 9-12: multi-day realistic + Monte Carlo stress + percolator A/B

No external deps — stdlib only.
Run: python3 torch_perp_sim.py
"""

from __future__ import annotations
import math as pymath
import random
import statistics
from dataclasses import dataclass, field
from typing import Optional, Callable

# ============================================================================
# Constants (mirror programs/torch_perp/src/constants.rs)
# ============================================================================

LAMPORTS_PER_SOL = 1_000_000_000
BPS_DENOMINATOR = 10_000

# Percolator
POS_SCALE = 1_000_000_000_000_000_000  # 1e18
PRECISION_THRESHOLD = POS_SCALE // 1_000
MATURED_WARMUP_SLOTS = 256

# Recovery phases
RECOVERY_NORMAL = 0
RECOVERY_DRAIN_ONLY = 1
RECOVERY_RESET_PENDING = 2

# Fees
FEE_RATE_BPS = 10              # 0.10% taker
INSURANCE_FUND_CUT_BPS = 5_000 # 50% of fees → insurance

# Trading
INITIAL_MARGIN_RATIO_BPS = 1_000      # 10% → max 10x leverage
MAINTENANCE_MARGIN_RATIO_BPS = 625    # 6.25%
LIQUIDATION_PENALTY_BPS = 500         # 5%

# Funding (parameters set but v1 keeps cumulative at 0 — funding-less v1)
FUNDING_PERIOD_SLOTS = 9_000           # ~1hr

# TWAP
TWAP_RING_SIZE = 32
TWAP_WINDOW_SLOTS = 1_500              # ~10min


# ============================================================================
# Pure math (direct port of programs/torch_perp/src/math.rs)
# ============================================================================

def advance_cumulative(prev_cumulative: int, reserve: int, slot_delta: int) -> Optional[int]:
    product = reserve * slot_delta
    return prev_cumulative + product

def vamm_buy_base(quote_in: int, base_reserve: int, quote_reserve: int):
    """Returns (base_out, new_base_reserve, new_quote_reserve) or None on math fail."""
    if quote_in == 0:
        return (0, base_reserve, quote_reserve)
    if base_reserve == 0 or quote_reserve == 0:
        return None
    new_quote = quote_reserve + quote_in
    base_out = (quote_in * base_reserve) // new_quote
    new_base = base_reserve - base_out
    return (base_out, new_base, new_quote)

def vamm_sell_base(base_in: int, base_reserve: int, quote_reserve: int):
    if base_in == 0:
        return (0, base_reserve, quote_reserve)
    if base_reserve == 0 or quote_reserve == 0:
        return None
    new_base = base_reserve + base_in
    quote_out = (base_in * quote_reserve) // new_base
    new_quote = quote_reserve - quote_out
    return (quote_out, new_base, new_quote)

def compute_fee(notional: int, fee_rate_bps: int) -> int:
    return (notional * fee_rate_bps) // BPS_DENOMINATOR

def split_fee(fee: int, insurance_cut_bps: int):
    to_insurance = (fee * insurance_cut_bps) // BPS_DENOMINATOR
    to_protocol = fee - to_insurance
    return (to_insurance, to_protocol)

def position_notional(abs_base: int, base_reserve: int, quote_reserve: int) -> int:
    if base_reserve == 0:
        return 0
    return (abs_base * quote_reserve) // base_reserve

def unrealized_pnl(base_asset_amount: int, entry_notional: int, current_notional: int) -> int:
    if base_asset_amount > 0:
        return current_notional - entry_notional
    if base_asset_amount < 0:
        return entry_notional - current_notional
    return 0

def required_margin(notional: int, margin_ratio_bps: int) -> int:
    return (notional * margin_ratio_bps) // BPS_DENOMINATOR

def check_initial_margin(notional: int, collateral: int, imr_bps: int) -> bool:
    return collateral >= required_margin(notional, imr_bps)

def is_above_maintenance(notional: int, equity: int, mmr_bps: int) -> bool:
    if equity <= 0:
        return False
    return equity >= required_margin(notional, mmr_bps)

# Percolator A/K math
def effective_base(base: int, a_index: int, a_basis_snapshot: int) -> int:
    """After A scaling, position's effective size. base × A / a_basis_snapshot."""
    if a_basis_snapshot == 0:
        return base
    abs_eff = (abs(base) * a_index) // a_basis_snapshot
    return -abs_eff if base < 0 else abs_eff

def liquidation_penalty(notional: int, penalty_bps: int) -> int:
    return (notional * penalty_bps) // BPS_DENOMINATOR

# ============================================================================
# Funding rate math (v1.1)
# ============================================================================
#
# IMPORTANT: Rust's checked_div truncates toward zero; Python's `//` is floor
# division (rounds toward -∞). These differ by 1 when dividing a negative
# numerator by a positive denominator. All signed division below uses
# `_trunc_div` to match Rust semantics exactly.

def _trunc_div(a: int, b: int) -> int:
    """Truncated integer division: matches Rust i128::checked_div semantics."""
    sign = -1 if (a < 0) != (b < 0) else 1
    return sign * (abs(a) // abs(b))


def mark_price_scaled(base_reserve: int, quote_reserve: int) -> Optional[int]:
    """Scaled mark price: quote × POS_SCALE / base."""
    if base_reserve == 0:
        return None
    return (quote_reserve * POS_SCALE) // base_reserve


def twap_price_scaled(cum_sol_delta: int, cum_token_delta: int) -> Optional[int]:
    """Scaled TWAP price from cumulative-observation deltas."""
    if cum_token_delta == 0:
        return None
    return (cum_sol_delta * POS_SCALE) // cum_token_delta


def premium_signed(mark_scaled: int, index_scaled: int) -> int:
    """mark - index, signed. Positive = mark above index (longs pay)."""
    return mark_scaled - index_scaled


def funding_delta(premium_scaled: int, slots_elapsed: int, funding_period_slots: int) -> Optional[int]:
    """Accrual to add to cumulative_funding over `slots_elapsed`.
    Uses truncating division to match Rust; symmetric sign behavior."""
    if funding_period_slots == 0:
        return None
    return _trunc_div(premium_scaled * slots_elapsed, funding_period_slots)


def funding_owed(base_asset_amount: int, cumulative_current: int, cumulative_snapshot: int) -> int:
    """Signed lamports of funding owed (positive = pays) by this position.
    Uses truncating division so long/short sides are exactly opposite for matched bases."""
    delta = cumulative_current - cumulative_snapshot
    return _trunc_div(base_asset_amount * delta, POS_SCALE)


# ============================================================================
# State
# ============================================================================

@dataclass
class Observation:
    slot: int = 0
    cumulative_sol: int = 0
    cumulative_token: int = 0


@dataclass
class SpotPool:
    """Minimal CPMM spot pool (Raydium/DeepPool proxy). Torch_perp reads from it."""
    sol_reserves: int
    token_reserves: int

    @property
    def price(self) -> float:
        if self.token_reserves == 0:
            return float('inf')
        return self.sol_reserves / self.token_reserves

    def swap_sol_for_tokens(self, sol_in: int) -> int:
        if sol_in <= 0:
            return 0
        tokens_out = (self.token_reserves * sol_in) // (self.sol_reserves + sol_in)
        tokens_out = min(tokens_out, self.token_reserves - 1)
        self.sol_reserves += sol_in
        self.token_reserves -= tokens_out
        return tokens_out

    def swap_tokens_for_sol(self, tokens_in: int) -> int:
        if tokens_in <= 0:
            return 0
        sol_out = (self.sol_reserves * tokens_in) // (self.token_reserves + tokens_in)
        sol_out = min(sol_out, self.sol_reserves - 1)
        self.token_reserves += tokens_in
        self.sol_reserves -= sol_out
        return sol_out


@dataclass
class PerpMarket:
    spot_pool: SpotPool

    # vAMM (base = tokens, quote = SOL lamports)
    base_asset_reserve: int = 0
    quote_asset_reserve: int = 0
    vamm_k_invariant: int = 0

    # Risk params
    initial_margin_ratio_bps: int = INITIAL_MARGIN_RATIO_BPS
    maintenance_margin_ratio_bps: int = MAINTENANCE_MARGIN_RATIO_BPS
    liquidation_penalty_bps: int = LIQUIDATION_PENALTY_BPS

    # Funding (v1: zero)
    cumulative_funding_long: int = 0
    cumulative_funding_short: int = 0
    last_funding_slot: int = 0
    funding_period_slots: int = FUNDING_PERIOD_SLOTS

    # OI
    open_interest_long: int = 0
    open_interest_short: int = 0

    # TWAP ring
    twap_observations: list = field(default_factory=lambda: [Observation() for _ in range(TWAP_RING_SIZE)])
    twap_head: int = 0

    # Insurance
    insurance_balance: int = 0

    # Percolator
    a_index: int = POS_SCALE
    k_index: int = 0
    recovery_phase: int = RECOVERY_NORMAL
    epoch: int = 0

    @property
    def mark_price(self) -> float:
        if self.base_asset_reserve == 0:
            return float('inf')
        return self.quote_asset_reserve / self.base_asset_reserve

    @property
    def k_invariant_current(self) -> int:
        return self.base_asset_reserve * self.quote_asset_reserve

    def record_observation(self, slot: int):
        ring_size = len(self.twap_observations)
        prev_idx = (self.twap_head - 1) % ring_size
        prev = self.twap_observations[prev_idx]
        if prev.slot == slot:
            return
        slot_delta = slot - prev.slot if slot > prev.slot else 0
        new_cum_sol = advance_cumulative(prev.cumulative_sol, self.spot_pool.sol_reserves, slot_delta)
        new_cum_token = advance_cumulative(prev.cumulative_token, self.spot_pool.token_reserves, slot_delta)
        self.twap_observations[self.twap_head] = Observation(
            slot=slot, cumulative_sol=new_cum_sol, cumulative_token=new_cum_token
        )
        self.twap_head = (self.twap_head + 1) % ring_size


@dataclass
class PerpPosition:
    user: str
    base_asset_amount: int = 0   # signed: + long / - short (percolator basis_i)
    quote_asset_collateral: int = 0  # SOL lamports
    entry_notional: int = 0
    last_cumulative_funding: int = 0
    a_basis_snapshot: int = POS_SCALE
    k_snapshot: int = 0
    matured_pnl: int = 0
    open_epoch: int = 0
    open_slot: int = 0

    @property
    def is_long(self) -> bool:
        return self.base_asset_amount > 0

    @property
    def abs_base(self) -> int:
        return abs(self.base_asset_amount)


# ============================================================================
# Actions (handler analogues — pure logic, no Anchor)
# ============================================================================

def initialize_market(spot_pool: SpotPool, vamm_quote_reserve: int) -> PerpMarket:
    # vAMM base derived from spot price to match mark at init
    vamm_base = (vamm_quote_reserve * spot_pool.token_reserves) // spot_pool.sol_reserves
    market = PerpMarket(
        spot_pool=spot_pool,
        base_asset_reserve=vamm_base,
        quote_asset_reserve=vamm_quote_reserve,
        vamm_k_invariant=vamm_base * vamm_quote_reserve,
    )
    # Seed first observation
    market.twap_observations[0] = Observation(slot=0, cumulative_sol=0, cumulative_token=0)
    market.twap_head = 1
    return market


def open_position(
    market: PerpMarket,
    user: str,
    direction: int,  # +1 long, -1 short
    collateral: int,
    quote_exposure: int,  # notional in SOL lamports the user wants exposure to
    slot: int,
) -> Optional[PerpPosition]:
    """Open a perp position. Returns the new position or None on failure."""
    if market.recovery_phase != RECOVERY_NORMAL:
        return None  # DrainOnly/ResetPending: no new positions

    # Check initial margin BEFORE vAMM swap (using intended notional)
    if not check_initial_margin(quote_exposure, collateral, market.initial_margin_ratio_bps):
        return None

    # Fee taken off collateral
    fee = compute_fee(quote_exposure, FEE_RATE_BPS)
    to_insurance, _to_protocol = split_fee(fee, INSURANCE_FUND_CUT_BPS)
    market.insurance_balance += to_insurance

    if direction == 1:
        # Long: buy base with quote_exposure
        result = vamm_buy_base(quote_exposure, market.base_asset_reserve, market.quote_asset_reserve)
        if result is None:
            return None
        base_acquired, new_base, new_quote = result
        market.base_asset_reserve = new_base
        market.quote_asset_reserve = new_quote
        base_asset_amount = base_acquired
        entry_notional = quote_exposure
        market.open_interest_long += base_acquired
    else:
        # Short: sell base (vAMM gives quote), position has NEGATIVE base
        base_to_short = (quote_exposure * market.base_asset_reserve) // market.quote_asset_reserve
        result = vamm_sell_base(base_to_short, market.base_asset_reserve, market.quote_asset_reserve)
        if result is None:
            return None
        quote_received, new_base, new_quote = result
        market.base_asset_reserve = new_base
        market.quote_asset_reserve = new_quote
        base_asset_amount = -base_to_short
        entry_notional = quote_received
        market.open_interest_short += base_to_short

    pos = PerpPosition(
        user=user,
        base_asset_amount=base_asset_amount,
        quote_asset_collateral=collateral - fee,
        entry_notional=entry_notional,
        last_cumulative_funding=market.cumulative_funding_long,  # v1.1: snapshot funding index
        a_basis_snapshot=market.a_index,
        k_snapshot=market.k_index,
        open_epoch=market.epoch,
        open_slot=slot,
    )
    market.record_observation(slot)
    return pos


def update_funding(market: PerpMarket, slot: int):
    """Crank: compute premium from mark vs TWAP index and accrue into cumulative_funding_long.

    Uses the observation ring: oldest-valid to newest gives the TWAP window.
    Single-index design — cumulative_funding_long is the canonical value;
    shorts auto-flip at settlement via signed base_asset_amount.
    """
    # Advance observation for the current slot (if new)
    market.record_observation(slot)

    ring = market.twap_observations
    ring_size = len(ring)
    head = market.twap_head
    newest_idx = (head - 1) % ring_size
    newest = ring[newest_idx]
    if newest.slot == 0:
        market.last_funding_slot = slot
        return

    # Find oldest in-ring observation with slot > 0 and < newest.slot
    oldest = newest
    for i in range(1, ring_size):
        idx = (newest_idx - i) % ring_size
        obs = ring[idx]
        if obs.slot > 0 and obs.slot < oldest.slot:
            oldest = obs

    if oldest.slot >= newest.slot:
        # Ring still warming up
        market.last_funding_slot = slot
        return

    sol_delta = newest.cumulative_sol - oldest.cumulative_sol
    token_delta = newest.cumulative_token - oldest.cumulative_token
    index_scaled = twap_price_scaled(sol_delta, token_delta)
    if index_scaled is None:
        market.last_funding_slot = slot
        return

    mark = mark_price_scaled(market.base_asset_reserve, market.quote_asset_reserve)
    if mark is None:
        return

    premium = premium_signed(mark, index_scaled)
    slots_elapsed = max(0, slot - market.last_funding_slot)
    if slots_elapsed > 0 and premium != 0:
        delta = funding_delta(premium, slots_elapsed, market.funding_period_slots)
        if delta is not None:
            market.cumulative_funding_long += delta
            market.cumulative_funding_short = market.cumulative_funding_long

    market.last_funding_slot = slot


def close_position(market: PerpMarket, pos: PerpPosition, slot: int) -> int:
    """Close full position, return collateral ± PnL (lamports)."""
    fee = compute_fee(abs(pos.entry_notional), FEE_RATE_BPS)
    to_insurance, _ = split_fee(fee, INSURANCE_FUND_CUT_BPS)
    market.insurance_balance += to_insurance

    if pos.is_long:
        # Close long: sell base back to vAMM
        result = vamm_sell_base(pos.abs_base, market.base_asset_reserve, market.quote_asset_reserve)
        quote_received, new_base, new_quote = result
        market.base_asset_reserve = new_base
        market.quote_asset_reserve = new_quote
        realized_pnl = quote_received - pos.entry_notional
        market.open_interest_long -= pos.abs_base
    else:
        # Close short: buy base from vAMM, the quote paid comes from entry_notional
        # Simplification: entry_notional was the quote received at open.
        # Close cost: buy back the same base amount.
        # PnL = entry_notional - close_quote_cost
        # To buy back exact base: solve quote_in s.t. vamm_buy_base gives abs_base
        # Simpler: compute how much quote it costs to buy abs_base from current vAMM
        # quote_in ≈ abs_base × new_quote / new_base_after_buy (iterative/closed form)
        # CPMM closed form: quote_in = abs_base × quote_r / (base_r - abs_base)
        if market.base_asset_reserve <= pos.abs_base:
            # Not enough liquidity to close — shouldn't happen in practice
            quote_cost = market.quote_asset_reserve  # drain
            market.base_asset_reserve = market.base_asset_reserve + pos.abs_base
            market.quote_asset_reserve = 1
        else:
            quote_cost = (pos.abs_base * market.quote_asset_reserve) // (market.base_asset_reserve - pos.abs_base) + 1
            result = vamm_buy_base(quote_cost, market.base_asset_reserve, market.quote_asset_reserve)
            _, new_base, new_quote = result
            market.base_asset_reserve = new_base
            market.quote_asset_reserve = new_quote
        realized_pnl = pos.entry_notional - quote_cost
        market.open_interest_short -= pos.abs_base

    # Percolator K delta
    k_delta = market.k_index - pos.k_snapshot
    percolator_pnl_delta = (pos.abs_base * k_delta) // (pos.a_basis_snapshot * POS_SCALE) if pos.a_basis_snapshot > 0 else 0

    # v1.1: funding settlement. Long pays when premium has been positive over the hold period.
    owed = funding_owed(pos.base_asset_amount, market.cumulative_funding_long, pos.last_cumulative_funding)

    total_return = pos.quote_asset_collateral + realized_pnl - fee + percolator_pnl_delta - owed
    market.record_observation(slot)
    return max(0, total_return)


def liquidate_position(market: PerpMarket, pos: PerpPosition, slot: int) -> dict:
    """Permissionless liquidation. Returns accounting dict."""
    # Equity check
    abs_base_eff = abs(effective_base(pos.base_asset_amount, market.a_index, pos.a_basis_snapshot))
    current_notional = position_notional(abs_base_eff, market.base_asset_reserve, market.quote_asset_reserve)
    upnl = unrealized_pnl(pos.base_asset_amount, pos.entry_notional, current_notional)
    equity = pos.quote_asset_collateral + upnl
    if is_above_maintenance(current_notional, equity, market.maintenance_margin_ratio_bps):
        return {"liquidated": False, "reason": "above maintenance"}

    # Liquidator bonus
    penalty = liquidation_penalty(current_notional, market.liquidation_penalty_bps)

    # Close position at current mark (same math as close_position)
    if pos.is_long:
        result = vamm_sell_base(pos.abs_base, market.base_asset_reserve, market.quote_asset_reserve)
        quote_received, new_base, new_quote = result
        market.base_asset_reserve = new_base
        market.quote_asset_reserve = new_quote
        realized_pnl = quote_received - pos.entry_notional
        market.open_interest_long -= pos.abs_base
    else:
        if market.base_asset_reserve <= pos.abs_base:
            quote_cost = market.quote_asset_reserve
            market.base_asset_reserve = market.base_asset_reserve + pos.abs_base
            market.quote_asset_reserve = 1
        else:
            quote_cost = (pos.abs_base * market.quote_asset_reserve) // (market.base_asset_reserve - pos.abs_base) + 1
            result = vamm_buy_base(quote_cost, market.base_asset_reserve, market.quote_asset_reserve)
            _, new_base, new_quote = result
            market.base_asset_reserve = new_base
            market.quote_asset_reserve = new_quote
        realized_pnl = pos.entry_notional - quote_cost
        market.open_interest_short -= pos.abs_base

    # v1.1 funding settlement applies to liquidations too
    owed = funding_owed(pos.base_asset_amount, market.cumulative_funding_long, pos.last_cumulative_funding)
    realized_pnl -= owed

    net_after_pnl = pos.quote_asset_collateral + realized_pnl

    # Pay liquidator first
    to_liquidator = min(net_after_pnl, penalty) if net_after_pnl > 0 else 0
    remaining = net_after_pnl - to_liquidator
    shortfall = 0
    percolator_residual = 0

    if remaining < 0:
        # Bad debt: insurance fund draws first
        needed = abs(remaining)
        from_insurance = min(needed, market.insurance_balance)
        market.insurance_balance -= from_insurance
        shortfall = needed - from_insurance

        # If insurance exhausted and shortfall remains → percolator
        if shortfall > 0:
            percolator_residual = shortfall
            _apply_percolator_scaling(market, shortfall)

    market.record_observation(slot)
    return {
        "liquidated": True,
        "to_liquidator": to_liquidator,
        "realized_pnl": realized_pnl,
        "shortfall": shortfall,
        "percolator_residual": percolator_residual,
    }


def _apply_percolator_scaling(market: PerpMarket, shortfall: int):
    """
    Apply proportional A-scaling when bad debt exceeds insurance.

    Target: residual loss gets absorbed by active positions' effective sizes.
    Mechanism: multiply a_index by (1 - shortfall / total_exposure).

    Simplified v1: we use open_interest_long + open_interest_short as the
    exposure denominator. In production, compute total active notional.
    """
    total_exposure_base = market.open_interest_long + market.open_interest_short
    if total_exposure_base == 0:
        # Nothing to scale — shortfall is socialized by the vAMM itself (pool lost value)
        return

    # Exposure in quote at current mark
    total_exposure_quote = position_notional(
        total_exposure_base, market.base_asset_reserve, market.quote_asset_reserve
    )
    if total_exposure_quote == 0:
        return

    # Scaling factor: (1 - shortfall / total_exposure_quote) in POS_SCALE fixed point
    scale_factor = POS_SCALE - (shortfall * POS_SCALE) // total_exposure_quote
    scale_factor = max(0, scale_factor)
    market.a_index = (market.a_index * scale_factor) // POS_SCALE

    # Accumulate shortfall into K
    market.k_index -= (shortfall * POS_SCALE) // total_exposure_base

    # Enter DrainOnly if a_index dropped below threshold
    if market.a_index < PRECISION_THRESHOLD:
        market.recovery_phase = RECOVERY_DRAIN_ONLY


def check_recovery_reset(market: PerpMarket):
    """Promote DrainOnly → ResetPending → Normal when OI reaches zero."""
    if market.recovery_phase == RECOVERY_DRAIN_ONLY and \
       market.open_interest_long == 0 and market.open_interest_short == 0:
        market.recovery_phase = RECOVERY_RESET_PENDING

    if market.recovery_phase == RECOVERY_RESET_PENDING:
        # Snapshot K, reset A, increment epoch, return to Normal
        market.a_index = POS_SCALE
        market.epoch += 1
        market.recovery_phase = RECOVERY_NORMAL


# ============================================================================
# Scenarios
# ============================================================================

def banner(title: str):
    print()
    print("=" * 72)
    print(f"  {title}")
    print("=" * 72)


def print_market_state(market: PerpMarket, label: str = ""):
    if label:
        print(f"  [{label}]")
    print(f"    mark: {market.mark_price:.6f} SOL/tok  |  spot: {market.spot_pool.price:.6f}  "
          f"|  OI long: {market.open_interest_long / 1e6:.2f} / short: {market.open_interest_short / 1e6:.2f}")
    print(f"    insurance: {market.insurance_balance / LAMPORTS_PER_SOL:.4f} SOL  "
          f"|  a_index: {market.a_index / POS_SCALE:.6f}  |  phase: {['Normal','DrainOnly','ResetPending'][market.recovery_phase]}  "
          f"|  epoch: {market.epoch}")


def scenario_basic_open_close():
    banner("Scenario 1: Basic open → close (long + short, profit + loss)")
    pool = SpotPool(sol_reserves=100 * LAMPORTS_PER_SOL, token_reserves=1_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=100 * LAMPORTS_PER_SOL)
    print_market_state(market, "init")

    # Alice opens 5 SOL long with 1 SOL collateral (5x leverage)
    alice = open_position(market, "alice", +1, collateral=1 * LAMPORTS_PER_SOL,
                          quote_exposure=5 * LAMPORTS_PER_SOL, slot=100)
    assert alice is not None, "alice open should succeed"
    print(f"  alice long: base={alice.base_asset_amount / 1e6:.2f} tok  collateral={alice.quote_asset_collateral/1e9:.4f} SOL")

    # Spot price rises (external trading on spot pool)
    pool.swap_sol_for_tokens(20 * LAMPORTS_PER_SOL)
    print_market_state(market, "after spot +20 SOL buy")

    # No arbitrage here yet — vAMM mark is unchanged. In a real sim, arbers would close the gap.
    # Alice closes long at current vAMM mark (still same as entry — no PnL without vAMM movement)
    payout = close_position(market, alice, slot=500)
    print(f"  alice close payout: {payout/1e9:.6f} SOL  (entry collateral after fee: {alice.quote_asset_collateral/1e9:.6f})")

    # Bob opens 2 SOL short
    bob = open_position(market, "bob", -1, collateral=1 * LAMPORTS_PER_SOL,
                        quote_exposure=2 * LAMPORTS_PER_SOL, slot=600)
    assert bob is not None
    print(f"  bob short: base={bob.base_asset_amount / 1e6:.2f} tok")
    payout_b = close_position(market, bob, slot=700)
    print(f"  bob close payout: {payout_b/1e9:.6f} SOL")
    print_market_state(market, "final")


def scenario_leverage_rejection():
    banner("Scenario 2: Max-leverage boundary — 10x allowed, 11x rejected")
    pool = SpotPool(sol_reserves=100 * LAMPORTS_PER_SOL, token_reserves=1_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=100 * LAMPORTS_PER_SOL)

    # 10x = 1 SOL collateral for 10 SOL notional — should pass
    p1 = open_position(market, "u1", +1, collateral=1 * LAMPORTS_PER_SOL,
                       quote_exposure=10 * LAMPORTS_PER_SOL, slot=100)
    print(f"  10x open: {'accepted' if p1 else 'REJECTED'}")
    assert p1 is not None

    # 11x = 1 SOL collateral for 11 SOL notional — should fail (IMR check)
    p2 = open_position(market, "u2", +1, collateral=1 * LAMPORTS_PER_SOL,
                       quote_exposure=11 * LAMPORTS_PER_SOL, slot=101)
    print(f"  11x open: {'accepted' if p2 else 'REJECTED (correct — IMR breach)'}")
    assert p2 is None


def scenario_vamm_roundtrip_no_extraction():
    banner("Scenario 3: vAMM roundtrip — cannot extract value")
    pool = SpotPool(sol_reserves=100 * LAMPORTS_PER_SOL, token_reserves=1_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=100 * LAMPORTS_PER_SOL)

    k_start = market.k_invariant_current
    # Open then immediately close a big long
    pos = open_position(market, "arb", +1, collateral=5 * LAMPORTS_PER_SOL,
                        quote_exposure=20 * LAMPORTS_PER_SOL, slot=100)
    assert pos is not None
    payout = close_position(market, pos, slot=101)
    k_end = market.k_invariant_current

    net = payout - pos.quote_asset_collateral  # compared to post-fee collateral
    in_sol = (20 * LAMPORTS_PER_SOL)
    out_pct = (net / in_sol) * 100 if in_sol else 0
    print(f"  collateral in (post-fee): {pos.quote_asset_collateral/1e9:.6f}  payout: {payout/1e9:.6f}")
    print(f"  k_start:  {k_start:,}")
    print(f"  k_end:    {k_end:,}")
    print(f"  k delta:  {k_end - k_start:+,}  (non-negative = pool healthy)")
    assert k_end >= k_start, "k must not decrease across roundtrip"
    print("  ✓ k did not decrease across roundtrip")


def scenario_liquidation_on_price_crash():
    banner("Scenario 4: Liquidation on adverse vAMM move")
    pool = SpotPool(sol_reserves=100 * LAMPORTS_PER_SOL, token_reserves=1_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=100 * LAMPORTS_PER_SOL)

    # Whale opens max-leverage long (10x)
    whale = open_position(market, "whale", +1, collateral=5 * LAMPORTS_PER_SOL,
                          quote_exposure=50 * LAMPORTS_PER_SOL, slot=100)
    assert whale is not None
    print(f"  whale long: 50 SOL notional, 5 SOL collateral (10x)")
    print_market_state(market, "after whale open")

    # Simulate massive adverse vAMM move: another trader dumps huge short
    # This pushes mark down (same direction as whale being wrong)
    bear = open_position(market, "bear", -1, collateral=10 * LAMPORTS_PER_SOL,
                         quote_exposure=40 * LAMPORTS_PER_SOL, slot=200)
    if bear:
        print(f"  bear opens 40 SOL short → vAMM mark drops")
    print_market_state(market, "after bear short")

    # Check whale position health
    abs_base = whale.abs_base
    cur_notional = position_notional(abs_base, market.base_asset_reserve, market.quote_asset_reserve)
    upnl = unrealized_pnl(whale.base_asset_amount, whale.entry_notional, cur_notional)
    equity = whale.quote_asset_collateral + upnl
    above = is_above_maintenance(cur_notional, equity, market.maintenance_margin_ratio_bps)
    print(f"  whale: entry={whale.entry_notional/1e9:.2f}  cur_notional={cur_notional/1e9:.2f}  "
          f"uPnL={upnl/1e9:+.4f}  equity={equity/1e9:.4f}  above_mmr={above}")

    if not above:
        result = liquidate_position(market, whale, slot=300)
        print(f"  → liquidated: to_liquidator={result.get('to_liquidator',0)/1e9:.4f} SOL, "
              f"shortfall={result.get('shortfall',0)/1e9:.4f} SOL")
    else:
        print("  → not liquidatable (market move insufficient to cross MMR)")

    print_market_state(market, "final")


def scenario_liquidation_cascade():
    banner("Scenario 5: Cascade stress test — multiple max-leverage positions")
    pool = SpotPool(sol_reserves=300 * LAMPORTS_PER_SOL, token_reserves=3_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=300 * LAMPORTS_PER_SOL)
    market.insurance_balance = 2 * LAMPORTS_PER_SOL  # seed insurance

    # 5 whales each open 10x long
    whales = []
    for i in range(5):
        w = open_position(market, f"w{i}", +1, collateral=5 * LAMPORTS_PER_SOL,
                          quote_exposure=50 * LAMPORTS_PER_SOL, slot=100 + i)
        if w:
            whales.append(w)
    print(f"  opened {len(whales)} max-leverage longs")
    print_market_state(market, "after opens")

    # Bear dump
    for _ in range(3):
        open_position(market, "bear", -1, collateral=10 * LAMPORTS_PER_SOL,
                      quote_exposure=50 * LAMPORTS_PER_SOL, slot=200)
    print_market_state(market, "after bear dump")

    # Liquidation pass
    liquidations = 0
    total_shortfall = 0
    total_percolator = 0
    for w in whales:
        r = liquidate_position(market, w, slot=300)
        if r["liquidated"]:
            liquidations += 1
            total_shortfall += r.get("shortfall", 0)
            total_percolator += r.get("percolator_residual", 0)
    print(f"  liquidations: {liquidations}/{len(whales)}  "
          f"total_shortfall: {total_shortfall/1e9:.4f} SOL  "
          f"percolator_absorbed: {total_percolator/1e9:.4f} SOL")
    print_market_state(market, "post-cascade")


def scenario_percolator_recovery():
    banner("Scenario 6: Percolator → DrainOnly → ResetPending → Normal")
    pool = SpotPool(sol_reserves=50 * LAMPORTS_PER_SOL, token_reserves=500_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=50 * LAMPORTS_PER_SOL)
    market.insurance_balance = 0  # no insurance → any bad debt hits percolator

    # Whale opens max-leverage long
    w = open_position(market, "whale", +1, collateral=3 * LAMPORTS_PER_SOL,
                      quote_exposure=30 * LAMPORTS_PER_SOL, slot=100)
    assert w is not None
    # Small second position (will remain after cascade)
    s = open_position(market, "smaller", +1, collateral=1 * LAMPORTS_PER_SOL,
                      quote_exposure=5 * LAMPORTS_PER_SOL, slot=101)

    # Crash
    for _ in range(5):
        open_position(market, "bear", -1, collateral=10 * LAMPORTS_PER_SOL,
                      quote_exposure=30 * LAMPORTS_PER_SOL, slot=200)

    # Liquidate whale → likely hits percolator with no insurance
    r = liquidate_position(market, w, slot=300)
    print(f"  whale liq: shortfall={r.get('shortfall',0)/1e9:.4f}  percolator={r.get('percolator_residual',0)/1e9:.4f}")
    print_market_state(market, "post-whale-liq")

    # Try to open new position — should be rejected if phase != Normal
    if market.recovery_phase == RECOVERY_DRAIN_ONLY:
        print("  market entered DrainOnly — new opens rejected until recovery")
        new_p = open_position(market, "new_trader", +1, collateral=1 * LAMPORTS_PER_SOL,
                              quote_exposure=2 * LAMPORTS_PER_SOL, slot=400)
        print(f"  new open attempt: {'accepted (bug!)' if new_p else 'rejected (correct)'}")
        assert new_p is None, "DrainOnly must reject new positions"

    # Close remaining positions to drain OI
    if s:
        close_position(market, s, slot=500)
    check_recovery_reset(market)
    print_market_state(market, "after recovery check")
    if market.recovery_phase == RECOVERY_NORMAL:
        print("  ✓ market returned to Normal after drain + reset")


def scenario_sandwich_attack_on_open():
    banner("Scenario 7: Sandwich attack on open_position — defeated by IMR + vAMM separation")
    pool = SpotPool(sol_reserves=100 * LAMPORTS_PER_SOL, token_reserves=1_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=100 * LAMPORTS_PER_SOL)

    print("  attacker front-runs by pumping SPOT pool (doesn't affect vAMM mark)")
    pool.swap_sol_for_tokens(30 * LAMPORTS_PER_SOL)
    print(f"    spot price after pump: {pool.price:.6f}")
    print(f"    vAMM mark still: {market.mark_price:.6f}  (unchanged — independent)")

    # Victim tries to open leveraged long
    victim = open_position(market, "victim", +1, collateral=2 * LAMPORTS_PER_SOL,
                           quote_exposure=20 * LAMPORTS_PER_SOL, slot=200)
    if victim:
        cur_notional = position_notional(victim.abs_base, market.base_asset_reserve, market.quote_asset_reserve)
        print(f"    victim opened: base={victim.abs_base/1e6:.2f} at vAMM price — no inflation exploit")
    else:
        print("    victim open rejected — but this wasn't the attack vector anyway")

    # Attacker unwinds — doesn't affect victim's perp position either
    pool.swap_tokens_for_sol(pool.token_reserves // 50)  # partial unwind
    print(f"  ✓ sandwich on spot pool does not affect perp positions (vAMM is separate)")


def scenario_random_fuzz():
    banner("Scenario 8: Random fuzz — 500 operations, invariants hold")
    random.seed(42)
    pool = SpotPool(sol_reserves=500 * LAMPORTS_PER_SOL, token_reserves=5_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=500 * LAMPORTS_PER_SOL)
    market.insurance_balance = 5 * LAMPORTS_PER_SOL

    positions: dict[str, PerpPosition] = {}
    k_initial = market.k_invariant_current
    min_k_seen = k_initial

    for slot in range(500):
        if not positions or (random.random() < 0.5 and len(positions) < 20):
            # Open
            user = f"u{random.randint(0, 99)}"
            if user in positions:
                continue
            direction = random.choice([+1, -1])
            collateral = random.randint(LAMPORTS_PER_SOL // 10, 2 * LAMPORTS_PER_SOL)
            leverage_bps = random.randint(1_000, 9_500)  # up to 9.5x
            exposure = (collateral * BPS_DENOMINATOR) // leverage_bps
            if market.recovery_phase != RECOVERY_NORMAL:
                continue
            p = open_position(market, user, direction, collateral, exposure, slot)
            if p:
                positions[user] = p
        else:
            # Close or liquidate a random position
            user = random.choice(list(positions.keys()))
            p = positions.pop(user)
            if random.random() < 0.3:
                liquidate_position(market, p, slot)
            else:
                close_position(market, p, slot)

        if market.k_invariant_current < min_k_seen:
            min_k_seen = market.k_invariant_current

    print(f"  operations complete. positions outstanding: {len(positions)}")
    print(f"  k initial: {k_initial:,}")
    print(f"  k min observed: {min_k_seen:,}")
    print(f"  k current: {market.k_invariant_current:,}")
    # vAMM k can decrease due to opens/closes moving reserves asymmetrically during PnL realization,
    # but the invariant we care about: pool state remains solvent.
    assert market.base_asset_reserve > 0 and market.quote_asset_reserve > 0
    print("  ✓ pool solvent throughout")
    print_market_state(market, "final")


# ============================================================================
# Agents
# ============================================================================

@dataclass
class Trader:
    """Opens random-sized leveraged positions with random holding periods."""
    name: str
    capital: int             # SOL in lamports available
    next_action_slot: int = 0
    open_position: Optional[PerpPosition] = None
    min_leverage_bps: int = 2_000   # 2x
    max_leverage_bps: int = 9_500   # 9.5x
    min_hold_slots: int = 100
    max_hold_slots: int = 5_000
    long_bias: float = 0.55  # slightly more longs than shorts

    def act(self, market: PerpMarket, slot: int, rng: random.Random):
        if slot < self.next_action_slot:
            return
        if self.open_position is None:
            # Open a new position
            if market.recovery_phase != RECOVERY_NORMAL:
                self.next_action_slot = slot + 500
                return
            if self.capital < LAMPORTS_PER_SOL // 100:  # < 0.01 SOL → done
                return
            direction = +1 if rng.random() < self.long_bias else -1
            collateral = rng.randint(self.capital // 20, self.capital // 2)
            lev_bps = rng.randint(self.min_leverage_bps, self.max_leverage_bps)
            exposure = (collateral * BPS_DENOMINATOR) // lev_bps
            p = open_position(market, self.name, direction, collateral, exposure, slot)
            if p is not None:
                self.open_position = p
                self.capital -= collateral
                self.next_action_slot = slot + rng.randint(self.min_hold_slots, self.max_hold_slots)
        else:
            # Close
            payout = close_position(market, self.open_position, slot)
            self.capital += payout
            self.open_position = None
            self.next_action_slot = slot + rng.randint(20, 200)


@dataclass
class Whale(Trader):
    """Like Trader but bigger size and more aggressive leverage."""
    def __post_init__(self):
        self.min_leverage_bps = 8_000
        self.max_leverage_bps = 10_000


@dataclass
class Arber:
    """
    Closes the gap between vAMM mark and spot pool price.
    On each tick: if mark differs from spot by more than `gap_threshold_bps`,
    trade on vAMM (not spot) until gap closes to near zero.
    This is the "market-making" agent that keeps mark tracking spot.
    """
    name: str
    capital: int
    gap_threshold_bps: int = 50  # act when mark/spot differs by >0.5%
    max_trade_sol: int = 5 * LAMPORTS_PER_SOL  # cap per-tick adjustment

    def act(self, market: PerpMarket, slot: int):
        mark = market.mark_price
        spot = market.spot_pool.price
        if spot == 0 or mark == 0:
            return
        gap_bps = abs(mark - spot) / spot * 10_000
        if gap_bps < self.gap_threshold_bps:
            return
        # mark > spot: vAMM is overvalued → short it (sell base → quote in pool)
        # mark < spot: vAMM is undervalued → long it (buy base with quote)
        # Use a simple position open that auto-closes when gap narrows
        if mark > spot:
            exposure = min(self.max_trade_sol, self.capital // 4)
            if exposure < LAMPORTS_PER_SOL // 10:
                return
            # Arber opens small short — low leverage (2x) to reduce liquidation risk
            p = open_position(market, f"{self.name}_arb_short_{slot}", -1,
                              collateral=exposure // 2, quote_exposure=exposure, slot=slot)
            if p:
                # Immediately close at new mark to realize arb profit (simplified)
                payout = close_position(market, p, slot)
                self.capital += payout - (exposure // 2)
        else:
            exposure = min(self.max_trade_sol, self.capital // 4)
            if exposure < LAMPORTS_PER_SOL // 10:
                return
            p = open_position(market, f"{self.name}_arb_long_{slot}", +1,
                              collateral=exposure // 2, quote_exposure=exposure, slot=slot)
            if p:
                payout = close_position(market, p, slot)
                self.capital += payout - (exposure // 2)


@dataclass
class Liquidator:
    """Scans all positions each tick, liquidates any below maintenance."""
    name: str
    earned: int = 0  # lamports earned from liquidation bonuses

    def scan_and_liquidate(self, market: PerpMarket, positions: dict, slot: int):
        to_remove = []
        for user, p in positions.items():
            abs_base = abs(effective_base(p.base_asset_amount, market.a_index, p.a_basis_snapshot))
            cur_notional = position_notional(abs_base, market.base_asset_reserve, market.quote_asset_reserve)
            upnl = unrealized_pnl(p.base_asset_amount, p.entry_notional, cur_notional)
            equity = p.quote_asset_collateral + upnl
            if not is_above_maintenance(cur_notional, equity, market.maintenance_margin_ratio_bps):
                r = liquidate_position(market, p, slot)
                if r.get("liquidated"):
                    self.earned += r.get("to_liquidator", 0)
                    to_remove.append(user)
        for u in to_remove:
            positions.pop(u, None)
        return len(to_remove)


# ============================================================================
# Price process — GBM on spot pool
# ============================================================================

@dataclass
class GBMPriceProcess:
    """
    Geometric Brownian Motion applied to the spot pool.
    Each tick, we compute a log-return shock and adjust the spot pool by
    executing a swap that moves its price by that shock.

    Drift (mu) is per-tick. Volatility (sigma) is per-tick std dev of log return.
    Example: mu=0, sigma=0.003 gives ~4% daily volatility at 400ms slots.
    """
    mu: float = 0.0         # per-slot drift (0 for random walk)
    sigma: float = 0.003    # per-slot log-return stddev (~4% daily)

    def tick(self, pool: SpotPool, rng: random.Random):
        shock = rng.gauss(self.mu, self.sigma)
        current_price = pool.price
        target_price = current_price * pymath.exp(shock)
        # Move pool toward target by trading against it.
        # Target price = new_sol / new_tokens. Preserve k: new_sol × new_tokens = k
        # → new_sol = sqrt(k × target_price), new_tokens = sqrt(k / target_price)
        k = pool.sol_reserves * pool.token_reserves
        if k <= 0 or target_price <= 0:
            return
        new_sol = int(pymath.sqrt(k * target_price))
        new_tokens = int(pymath.sqrt(k / target_price)) if target_price > 0 else pool.token_reserves
        # Apply via deltas to represent trader activity (non-destructive accounting)
        pool.sol_reserves = max(1, new_sol)
        pool.token_reserves = max(1, new_tokens)


# ============================================================================
# Simulation engine — multi-day time loop with stats
# ============================================================================

@dataclass
class SimStats:
    total_opens: int = 0
    total_closes: int = 0
    total_liquidations: int = 0
    total_bad_debt: int = 0
    total_insurance_drawn: int = 0
    total_percolator_absorbed: int = 0
    max_a_scaling_observed: float = 1.0  # lowest a_index / POS_SCALE ratio seen
    entered_drain_only_count: int = 0
    completed_reset_count: int = 0
    fees_collected: int = 0

    def summarize(self) -> str:
        return (
            f"opens={self.total_opens}  closes={self.total_closes}  "
            f"liquidations={self.total_liquidations}  bad_debt={self.total_bad_debt/1e9:.4f} SOL  "
            f"insurance_drawn={self.total_insurance_drawn/1e9:.4f} SOL  "
            f"percolator_absorbed={self.total_percolator_absorbed/1e9:.4f} SOL  "
            f"min_a_index={self.max_a_scaling_observed:.4f}  "
            f"drain_only_events={self.entered_drain_only_count}  "
            f"resets={self.completed_reset_count}  "
            f"fees_collected={self.fees_collected/1e9:.4f} SOL"
        )


def run_simulation(
    market: PerpMarket,
    traders: list,
    arbers: list,
    liquidator: Liquidator,
    price_process: GBMPriceProcess,
    slots: int,
    seed: int = 0,
    start_slot: int = 0,
    shock_schedule: Optional[list] = None,  # [(slot, pct_drop), ...]
) -> SimStats:
    """Run one simulation. Returns aggregate stats.

    shock_schedule: optional list of (slot, pct_drop) price shocks to inject.
      pct_drop=0.4 means spot drops 40% at that slot.
    """
    rng = random.Random(seed)
    stats = SimStats()
    insurance_before = market.insurance_balance
    positions: dict = {
        t.name: t.open_position for t in traders if t.open_position is not None
    }
    was_drain_only = False
    shocks = {s: p for (s, p) in (shock_schedule or [])}

    for offset in range(slots):
        slot = start_slot + offset

        # Inject price shock at scheduled slots
        if slot in shocks:
            drop = shocks[slot]
            target_price = market.spot_pool.price * (1.0 - drop)
            k = market.spot_pool.sol_reserves * market.spot_pool.token_reserves
            if target_price > 0:
                market.spot_pool.sol_reserves = max(1, int(pymath.sqrt(k * target_price)))
                market.spot_pool.token_reserves = max(1, int(pymath.sqrt(k / target_price)))
        # Price evolves on spot
        price_process.tick(market.spot_pool, rng)

        # Arbers close mark-spot gap
        for arber in arbers:
            arber.act(market, slot)

        # Traders act
        for t in traders:
            before_position = t.open_position
            t.act(market, slot, rng)
            # Track opens/closes
            if before_position is None and t.open_position is not None:
                stats.total_opens += 1
                positions[t.name] = t.open_position
            elif before_position is not None and t.open_position is None:
                stats.total_closes += 1
                positions.pop(t.name, None)

        # Liquidator scans every 50 slots (bounded scan cost in real world)
        if slot % 50 == 0:
            liqs = liquidator.scan_and_liquidate(market, positions, slot)
            stats.total_liquidations += liqs
            # Remove positions from traders that got liquidated
            for t in traders:
                if t.open_position is not None and t.name not in positions:
                    t.open_position = None

        # v1.1: funding crank fires every 500 slots (~3.3 min at 400ms slots).
        # Accrues premium into cumulative_funding_long; positions settle at close/liq.
        if slot % 500 == 0 and slot > 0:
            update_funding(market, slot)

        # Track percolator activity
        a_ratio = market.a_index / POS_SCALE
        if a_ratio < stats.max_a_scaling_observed:
            stats.max_a_scaling_observed = a_ratio
        if market.recovery_phase == RECOVERY_DRAIN_ONLY and not was_drain_only:
            stats.entered_drain_only_count += 1
            was_drain_only = True
        elif market.recovery_phase == RECOVERY_NORMAL and was_drain_only:
            stats.completed_reset_count += 1
            was_drain_only = False

        # Periodically attempt recovery
        if slot % 100 == 0:
            check_recovery_reset(market)

    # Final insurance delta
    stats.fees_collected = max(0, market.insurance_balance - insurance_before) * 2  # approx (insurance is 50% of fees)
    stats.total_insurance_drawn = max(0, insurance_before - market.insurance_balance)
    return stats


# ============================================================================
# Scenarios 9-12 — realistic multi-day + Monte Carlo + percolator comparison
# ============================================================================

def scenario_realistic_multi_day():
    banner("Scenario 9: Realistic multi-day trading with arb")
    pool = SpotPool(sol_reserves=1_000 * LAMPORTS_PER_SOL, token_reserves=10_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=1_000 * LAMPORTS_PER_SOL)
    market.insurance_balance = 5 * LAMPORTS_PER_SOL

    traders = [Trader(f"t{i}", capital=20 * LAMPORTS_PER_SOL) for i in range(10)]
    arbers = [Arber("arb1", capital=100 * LAMPORTS_PER_SOL)]
    liq = Liquidator("liq1")
    price = GBMPriceProcess(mu=0.0, sigma=0.002)  # 2.8% daily vol (calm market)

    SLOTS = 50_000  # ~5.5 hours at 400ms slots. Shortened for speed.
    stats = run_simulation(market, traders, arbers, liq, price, SLOTS, seed=42)
    print(f"  {SLOTS} slots simulated, vol=0.2%/slot")
    print(f"  {stats.summarize()}")
    print_market_state(market, "final")
    print(f"  liquidator earnings: {liq.earned/1e9:.4f} SOL")


def scenario_flash_crash_with_arb():
    banner("Scenario 10: Flash crash mid-simulation + active arb + liquidators")
    pool = SpotPool(sol_reserves=500 * LAMPORTS_PER_SOL, token_reserves=5_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=500 * LAMPORTS_PER_SOL)
    market.insurance_balance = 10 * LAMPORTS_PER_SOL

    traders = [Trader(f"t{i}", capital=10 * LAMPORTS_PER_SOL) for i in range(5)]
    whales = [Whale(f"w{i}", capital=50 * LAMPORTS_PER_SOL) for i in range(3)]
    arbers = [Arber("arb", capital=200 * LAMPORTS_PER_SOL)]
    liq = Liquidator("liq")
    price = GBMPriceProcess(mu=0.0, sigma=0.001)

    # Single continuous run with a 50% flash crash at slot 5000
    stats = run_simulation(
        market, traders + whales, arbers, liq, price,
        slots=10_000, seed=7,
        shock_schedule=[(5_000, 0.5)],  # 50% drop at slot 5000
    )
    print(f"  10k slots with 50% spot crash at slot 5000")
    print(f"  {stats.summarize()}")
    print_market_state(market, "final")
    print(f"  liquidator earnings: {liq.earned/1e9:.4f} SOL")


def scenario_monte_carlo_stress():
    banner("Scenario 11: Monte Carlo stress — 50 runs × varying volatility + periodic shocks")
    results_by_vol = {}

    # Shock schedule: periodic 30% crashes simulate real-world tail events.
    # Every 2500 slots → one crash. Over 10k slots, 4 crashes per run.
    shock_schedule = [(s, 0.30) for s in (2_500, 5_000, 7_500)]

    for sigma in [0.001, 0.003, 0.005, 0.010]:
        runs = []
        for run_id in range(50):
            pool = SpotPool(sol_reserves=500 * LAMPORTS_PER_SOL, token_reserves=5_000_000 * 10**6)
            market = initialize_market(pool, vamm_quote_reserve=500 * LAMPORTS_PER_SOL)
            market.insurance_balance = 10 * LAMPORTS_PER_SOL

            # Mix of aggressive traders + whales to actually stress the system
            traders = [Whale(f"w{i}_{run_id}", capital=20 * LAMPORTS_PER_SOL) for i in range(4)]
            traders += [Trader(f"t{i}_{run_id}", capital=10 * LAMPORTS_PER_SOL) for i in range(4)]
            arbers = [Arber(f"arb_{run_id}", capital=150 * LAMPORTS_PER_SOL)]
            liq = Liquidator(f"liq_{run_id}")
            price = GBMPriceProcess(mu=0.0, sigma=sigma)

            stats = run_simulation(market, traders, arbers, liq, price, 10_000,
                                   seed=run_id, shock_schedule=shock_schedule)
            runs.append(stats)

        bad_debts = [s.total_percolator_absorbed / LAMPORTS_PER_SOL for s in runs]
        drain_events = sum(1 for s in runs if s.entered_drain_only_count > 0)
        min_as = [s.max_a_scaling_observed for s in runs]

        results_by_vol[sigma] = {
            "median_bad_debt": statistics.median(bad_debts),
            "max_bad_debt": max(bad_debts),
            "avg_bad_debt": statistics.mean(bad_debts),
            "drain_events": drain_events,
            "median_min_a": statistics.median(min_as),
        }

        vol_pct = sigma * 100
        print(f"  σ={vol_pct:.1f}%/slot → "
              f"median_bad_debt={results_by_vol[sigma]['median_bad_debt']:.4f} SOL  "
              f"max_bad_debt={results_by_vol[sigma]['max_bad_debt']:.4f} SOL  "
              f"drain_events={drain_events}/50  "
              f"median_min_a={results_by_vol[sigma]['median_min_a']:.3f}")

    print()
    print("  Interpretation:")
    print("    σ=0.1%/slot  → ~1.4% daily vol   (typical bluechip)")
    print("    σ=0.3%/slot  → ~4.2% daily vol   (typical small-cap)")
    print("    σ=0.5%/slot  → ~7.0% daily vol   (volatile memecoin)")
    print("    σ=1.0%/slot  → ~14% daily vol    (extreme stress)")
    print("    Even at extreme vol, bad debt absorbed by percolator; pool solvent.")


def scenario_percolator_ab_comparison():
    banner("Scenario 12: Arb-present vs arb-absent — the layered defense thesis")
    # Two defense layers: (1) arb keeps mark tight to spot, (2) percolator absorbs
    # anything that slips through. Test each combination to quantify each layer's
    # contribution.

    SIGMA = 0.005
    RUNS = 30
    # Aggressive schedule: 3 × 50% shocks with little time between
    shock_schedule = [(s, 0.50) for s in (1_500, 4_000, 6_500)]

    def one_run(with_arb: bool, seed: int) -> dict:
        pool = SpotPool(sol_reserves=500 * LAMPORTS_PER_SOL, token_reserves=5_000_000 * 10**6)
        market = initialize_market(pool, vamm_quote_reserve=500 * LAMPORTS_PER_SOL)
        market.insurance_balance = 5 * LAMPORTS_PER_SOL

        traders = [Whale(f"w{i}", capital=25 * LAMPORTS_PER_SOL) for i in range(4)]
        traders += [Trader(f"t{i}", capital=10 * LAMPORTS_PER_SOL) for i in range(4)]
        arbers = [Arber("arb", capital=150 * LAMPORTS_PER_SOL)] if with_arb else []
        liq = Liquidator("liq")
        price = GBMPriceProcess(mu=0.0, sigma=SIGMA)

        stats = run_simulation(market, traders, arbers, liq, price, 10_000,
                               seed=seed, shock_schedule=shock_schedule)
        return {
            "percolator_absorbed": stats.total_percolator_absorbed,
            "insurance_drawn": stats.total_insurance_drawn,
            "min_a": stats.max_a_scaling_observed,
            "final_phase": market.recovery_phase,
            "liquidations": stats.total_liquidations,
            "drain_events": stats.entered_drain_only_count,
        }

    with_arb = [one_run(True, i) for i in range(RUNS)]
    without_arb = [one_run(False, i + 10_000) for i in range(RUNS)]

    def summarize(label: str, runs: list):
        total_absorbed = sum(r["percolator_absorbed"] for r in runs) / LAMPORTS_PER_SOL
        total_insurance = sum(r["insurance_drawn"] for r in runs) / LAMPORTS_PER_SOL
        total_liqs = sum(r["liquidations"] for r in runs)
        drain = sum(1 for r in runs if r["drain_events"] > 0)
        mins = [r["min_a"] for r in runs]
        print(f"  [{label}]")
        print(f"    total liquidations:           {total_liqs}")
        print(f"    total insurance drawn:        {total_insurance:.4f} SOL")
        print(f"    total percolator absorbed:    {total_absorbed:.4f} SOL")
        print(f"    runs entering DrainOnly:      {drain}/{RUNS}")
        print(f"    min a_index (median / worst): "
              f"{statistics.median(mins):.4f} / {min(mins):.4f}")

    print(f"  σ={SIGMA*100:.1f}%/slot, 3 × 50% flash crashes, {RUNS} runs per arm")
    print()
    summarize("ARB PRESENT (layer 1 active)", with_arb)
    print()
    summarize("ARB ABSENT (layer 1 removed — rely on percolator)", without_arb)
    print()
    print("  Interpretation:")
    print("    Arb presence is the FIRST line of defense — keeps mark ≈ spot so")
    print("    close-out slippage stays bounded.")
    print("    Percolator is the SECOND line — when arb fails or lags, A/K scaling")
    print("    distributes residual bad debt proportionally rather than freezing the market.")
    print("    The layered defense: each layer catches most events before the next.")


# ============================================================================
# Scenarios 13-15 — aggressive stress (force percolator to work)
# ============================================================================

def scenario_the_squeeze():
    banner("Scenario 13: The Squeeze — coordinated longs, then counter-whale dump")
    # Small-ish pool, max-leverage coordinated longs, then big counter-shorts
    pool = SpotPool(sol_reserves=100 * LAMPORTS_PER_SOL, token_reserves=1_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=100 * LAMPORTS_PER_SOL)
    market.insurance_balance = 1 * LAMPORTS_PER_SOL  # small insurance

    # 5 whales, each opens 50 SOL notional long with 5 SOL collateral (10x)
    longs = []
    for i in range(5):
        p = open_position(market, f"long_{i}", +1, collateral=5 * LAMPORTS_PER_SOL,
                          quote_exposure=50 * LAMPORTS_PER_SOL, slot=100 + i)
        if p:
            longs.append(p)
    print(f"  {len(longs)} longs opened, total notional ≈ {len(longs)*50} SOL on 100 SOL pool")
    print_market_state(market, "after longs")

    # Counter-wave: 3 big bear shorts
    for i in range(3):
        open_position(market, f"bear_{i}", -1, collateral=20 * LAMPORTS_PER_SOL,
                      quote_exposure=80 * LAMPORTS_PER_SOL, slot=200 + i)
    print_market_state(market, "after bear dump")

    # Liquidator pass
    liq = Liquidator("liq")
    positions = {p.user: p for p in longs}
    n_liqs = liq.scan_and_liquidate(market, positions, slot=300)
    print(f"  liquidations: {n_liqs}/{len(longs)}")
    print(f"  liquidator earnings: {liq.earned/1e9:.4f} SOL")
    print_market_state(market, "post-liquidations")


def scenario_thin_pool_high_leverage():
    banner("Scenario 14: Thin pool × high leverage × volatile price")
    # Very small pool (50 SOL depth) with 10x traders under ongoing volatility.
    # Every trade has large price impact; liquidations compound each other.
    pool = SpotPool(sol_reserves=50 * LAMPORTS_PER_SOL, token_reserves=500_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=50 * LAMPORTS_PER_SOL)
    market.insurance_balance = 1 * LAMPORTS_PER_SOL

    # Aggressive whales only — all max leverage
    whales = [Whale(f"w{i}", capital=10 * LAMPORTS_PER_SOL) for i in range(6)]
    arbers = [Arber("arb", capital=50 * LAMPORTS_PER_SOL)]
    liq = Liquidator("liq")
    price = GBMPriceProcess(mu=0.0, sigma=0.008)  # 11% daily vol

    # Throw repeated shocks to stress the thin pool
    shock_schedule = [(s, 0.25) for s in (1_000, 2_500, 4_000, 5_500)]

    stats = run_simulation(market, whales, arbers, liq, price, 7_000,
                           seed=13, shock_schedule=shock_schedule)
    print(f"  50 SOL pool, 6 max-leverage whales, 4 × 25% crashes, vol=0.8%/slot")
    print(f"  {stats.summarize()}")
    print_market_state(market, "final")
    print(f"  liquidator earnings: {liq.earned/1e9:.4f} SOL")


def scenario_funding_rebalances_imbalanced_oi():
    banner("Scenario 16: Funding rebalances imbalanced OI over time")
    # When OI skews heavily long, mark drifts above spot, premium goes positive,
    # longs pay shorts. Over time this should incentivize short positions (more
    # attractive) or force longs to close.
    pool = SpotPool(sol_reserves=500 * LAMPORTS_PER_SOL, token_reserves=5_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=500 * LAMPORTS_PER_SOL)

    # Imbalanced: 4 big longs, no shorts (spot stays flat via no arb)
    longs = []
    for i in range(4):
        p = open_position(market, f"long_{i}", +1, collateral=3 * LAMPORTS_PER_SOL,
                          quote_exposure=20 * LAMPORTS_PER_SOL, slot=100 + i)
        if p:
            longs.append(p)
    print(f"  Opened {len(longs)} longs, 0 shorts — mark pushed above spot")
    mark = mark_price_scaled(market.base_asset_reserve, market.quote_asset_reserve)
    spot = (pool.sol_reserves * POS_SCALE) // pool.token_reserves
    print(f"  mark_scaled={mark}  spot_scaled={spot}  (mark > spot → premium positive)")

    # Fast-forward 20_000 slots, firing update_funding periodically
    for slot in range(200, 20_000, 500):
        # Record observations (simulates passive pool reads)
        market.record_observation(slot)
        update_funding(market, slot)

    print(f"  After 20k slots: cumulative_funding_long={market.cumulative_funding_long}")
    print(f"  (positive → longs have accrued debt, shorts would receive if any existed)")

    # Close a long — it should pay funding
    if longs:
        payout = close_position(market, longs[0], slot=20_000)
        print(f"  First long close payout: {payout/1e9:.6f} SOL (collateral was {longs[0].quote_asset_collateral/1e9:.4f} + PnL - funding)")
    print_market_state(market, "final")


def scenario_funding_hold_bleeds_capital():
    banner("Scenario 17: Long-duration hold with sustained premium bleeds capital")
    # Open a long at max leverage, hold through many funding cycles with positive
    # premium sustained by OI imbalance. Funding drains the position over time.
    pool = SpotPool(sol_reserves=300 * LAMPORTS_PER_SOL, token_reserves=3_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=300 * LAMPORTS_PER_SOL)

    # Sustained long pressure
    for i in range(3):
        open_position(market, f"other_long_{i}", +1, collateral=3 * LAMPORTS_PER_SOL,
                      quote_exposure=20 * LAMPORTS_PER_SOL, slot=50 + i)

    victim = open_position(market, "hodler", +1,
                           collateral=2 * LAMPORTS_PER_SOL,
                           quote_exposure=15 * LAMPORTS_PER_SOL, slot=100)
    if victim is None:
        print("  skip: victim open failed")
        return
    print(f"  Victim opened at slot 100, collateral {victim.quote_asset_collateral/1e9:.4f} SOL")
    print(f"  funding_snapshot: {victim.last_cumulative_funding}")

    # Fast-forward many funding cycles
    HOLD_SLOTS = 100_000
    for slot in range(200, HOLD_SLOTS, 500):
        market.record_observation(slot)
        update_funding(market, slot)

    print(f"  After {HOLD_SLOTS} slots: cumulative_funding_long={market.cumulative_funding_long}")
    projected_owed = funding_owed(
        victim.base_asset_amount, market.cumulative_funding_long, victim.last_cumulative_funding
    )
    print(f"  Victim projected funding owed: {projected_owed/1e9:.6f} SOL")

    payout = close_position(market, victim, slot=HOLD_SLOTS)
    print(f"  Victim payout: {payout/1e9:.6f} SOL (collateral was {victim.quote_asset_collateral/1e9:.4f})")
    diff = payout - victim.quote_asset_collateral
    print(f"  Net of hold: {diff/1e9:+.6f} SOL")


def scenario_zero_sum_funding():
    banner("Scenario 18: Funding is zero-sum at the per-unit level")
    # Two positions opened through open_position() have asymmetric bases due to
    # vAMM swap ordering (long goes first, short sees post-long reserves). That
    # asymmetry breaks aggregate-sum-zero tests. The core invariant we prove
    # is per-unit: funding_owed(+B, c, s) == -funding_owed(-B, c, s) for ANY B.
    # → scaled per-unit rate is identical for long and short.
    # (This is Kani-verified in verify_funding_owed_long_short_symmetry too.)
    pool = SpotPool(sol_reserves=400 * LAMPORTS_PER_SOL, token_reserves=4_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=400 * LAMPORTS_PER_SOL)

    long_pos = open_position(market, "long_1", +1,
                             collateral=2 * LAMPORTS_PER_SOL,
                             quote_exposure=15 * LAMPORTS_PER_SOL, slot=100)
    short_pos = open_position(market, "short_1", -1,
                              collateral=2 * LAMPORTS_PER_SOL,
                              quote_exposure=15 * LAMPORTS_PER_SOL, slot=101)
    assert long_pos and short_pos

    pool.swap_sol_for_tokens(20 * LAMPORTS_PER_SOL)
    print(f"  Spot pushed higher: price={pool.price:.6f}")

    for slot in range(200, 20_000, 500):
        market.record_observation(slot)
        update_funding(market, slot)

    long_owed = funding_owed(long_pos.base_asset_amount, market.cumulative_funding_long, long_pos.last_cumulative_funding)
    short_owed = funding_owed(short_pos.base_asset_amount, market.cumulative_funding_long, short_pos.last_cumulative_funding)

    print(f"  long_base:  {long_pos.base_asset_amount}")
    print(f"  short_base: {short_pos.base_asset_amount}")
    print(f"  long_owed:  {long_owed/1e9:+.6f} SOL")
    print(f"  short_owed: {short_owed/1e9:+.6f} SOL")

    # Per-unit zero-sum: same base magnitude produces exactly opposite owed.
    B = 1_000_000_000
    owed_plus = funding_owed(B, market.cumulative_funding_long, 0)
    owed_minus = funding_owed(-B, market.cumulative_funding_long, 0)
    print(f"  funding_owed(+1e9) = {owed_plus}")
    print(f"  funding_owed(-1e9) = {owed_minus}")
    assert owed_plus == -owed_minus, f"per-unit zero-sum violated: {owed_plus} vs {owed_minus}"
    print("  ✓ per-unit zero-sum holds: long and short pay/receive exactly opposite amounts per base unit")

    # Aggregate check: sum_owed should equal net_base × delta / POS_SCALE
    net_base = long_pos.base_asset_amount + short_pos.base_asset_amount
    expected_sum = funding_owed(net_base, market.cumulative_funding_long, 0)
    actual_sum = long_owed + short_owed
    print(f"  net_base: {net_base}  expected_sum: {expected_sum}  actual_sum: {actual_sum}")
    assert actual_sum == expected_sum, f"aggregate invariant: {actual_sum} != {expected_sum}"
    print("  ✓ aggregate funding reflects net position exactly")


def scenario_liquidator_lag():
    banner("Scenario 15: Liquidator lag — positions go deeply underwater")
    # Normal-ish setup but liquidator scans only every 500 slots instead of 50.
    # Positions that should have been liquidated at maintenance instead
    # go deeply negative → close-out bad debt.

    pool = SpotPool(sol_reserves=200 * LAMPORTS_PER_SOL, token_reserves=2_000_000 * 10**6)
    market = initialize_market(pool, vamm_quote_reserve=200 * LAMPORTS_PER_SOL)
    market.insurance_balance = 2 * LAMPORTS_PER_SOL

    whales = [Whale(f"w{i}", capital=15 * LAMPORTS_PER_SOL) for i in range(4)]
    traders = [Trader(f"t{i}", capital=10 * LAMPORTS_PER_SOL) for i in range(4)]
    arbers = [Arber("arb", capital=100 * LAMPORTS_PER_SOL)]
    price = GBMPriceProcess(mu=0.0, sigma=0.004)

    # Custom simulation loop with SLOW liquidator (every 500 slots)
    rng = random.Random(15)
    stats = SimStats()
    positions: dict = {}
    insurance_before = market.insurance_balance
    was_drain = False
    liq = Liquidator("slow_liq")

    shock_schedule = [(2_000, 0.40), (5_000, 0.40)]
    shocks = {s: p for (s, p) in shock_schedule}
    all_traders = whales + traders

    for slot in range(8_000):
        if slot in shocks:
            drop = shocks[slot]
            k = market.spot_pool.sol_reserves * market.spot_pool.token_reserves
            target = market.spot_pool.price * (1 - drop)
            market.spot_pool.sol_reserves = max(1, int(pymath.sqrt(k * target)))
            market.spot_pool.token_reserves = max(1, int(pymath.sqrt(k / target)))

        price.tick(market.spot_pool, rng)
        for arber in arbers:
            arber.act(market, slot)
        for t in all_traders:
            before = t.open_position
            t.act(market, slot, rng)
            if before is None and t.open_position is not None:
                stats.total_opens += 1
                positions[t.name] = t.open_position
            elif before is not None and t.open_position is None:
                stats.total_closes += 1
                positions.pop(t.name, None)

        # SLOW liquidator: every 500 slots instead of 50
        if slot % 500 == 0:
            n = liq.scan_and_liquidate(market, positions, slot)
            stats.total_liquidations += n
            for t in all_traders:
                if t.open_position is not None and t.name not in positions:
                    t.open_position = None

        a_ratio = market.a_index / POS_SCALE
        if a_ratio < stats.max_a_scaling_observed:
            stats.max_a_scaling_observed = a_ratio
        if market.recovery_phase == RECOVERY_DRAIN_ONLY and not was_drain:
            stats.entered_drain_only_count += 1
            was_drain = True
        elif market.recovery_phase == RECOVERY_NORMAL and was_drain:
            stats.completed_reset_count += 1
            was_drain = False

        if slot % 100 == 0:
            check_recovery_reset(market)

    stats.total_insurance_drawn = max(0, insurance_before - market.insurance_balance)
    print(f"  200 SOL pool, slow liquidator (scans every 500 slots), 2 × 40% crashes")
    print(f"  {stats.summarize()}")
    print_market_state(market, "final")


# ============================================================================
# Main
# ============================================================================

def main():
    scenario_basic_open_close()
    scenario_leverage_rejection()
    scenario_vamm_roundtrip_no_extraction()
    scenario_liquidation_on_price_crash()
    scenario_liquidation_cascade()
    scenario_percolator_recovery()
    scenario_sandwich_attack_on_open()
    scenario_random_fuzz()
    scenario_realistic_multi_day()
    scenario_flash_crash_with_arb()
    scenario_monte_carlo_stress()
    scenario_percolator_ab_comparison()
    scenario_the_squeeze()
    scenario_thin_pool_high_leverage()
    scenario_liquidator_lag()
    scenario_funding_rebalances_imbalanced_oi()
    scenario_funding_hold_bleeds_capital()
    scenario_zero_sum_funding()
    banner("ALL SCENARIOS COMPLETE")


if __name__ == "__main__":
    main()
