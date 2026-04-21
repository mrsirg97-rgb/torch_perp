// Pure math for torch_perp.
//
// Every function here is a pure function of primitive types:
//   - No Anchor accounts, no Pubkey, no I/O
//   - Returns Option<T> where overflow → None (Kani-friendly)
//   - Deterministic, no floats, no randomness
//
// This gives us three properties:
//   1. Directly portable to a Python simulation
//   2. Directly reasoned about in Kani formal proofs (see kani_proofs.rs)
//   3. Testable in isolation with unit tests
//
// Handlers convert Option<T> → Result<T> at call sites via
// `.ok_or(TorchPerpError::MathOverflow)?`.

// ==============================================================================
// TWAP — cumulative observations
// ==============================================================================

// Advance a running cumulative by `reserve × slot_delta`.
// Used by write_observation to build time-weighted cumulative reserves.
// Any two observations give the time-averaged reserve over their slot span:
//   avg = (cum_new - cum_old) / (slot_new - slot_old)
pub fn advance_cumulative(
    prev_cumulative: u128,
    reserve: u64,
    slot_delta: u64,
) -> Option<u128> {
    let product = (reserve as u128).checked_mul(slot_delta as u128)?;
    prev_cumulative.checked_add(product)
}

// ==============================================================================
// vAMM constant-product swap
// ==============================================================================

// Buy `base` with `quote_in` quote tokens. Returns (base_out, new_base_reserve, new_quote_reserve).
// Uniswap-V2 constant-product formula:
//   new_quote_reserve = quote_reserve + quote_in
//   base_out          = floor(quote_in × base_reserve / new_quote_reserve)
//   new_base_reserve  = base_reserve - base_out
// Floor on base_out keeps the rounding residual in the pool, so k is non-decreasing.
pub fn vamm_buy_base(
    quote_in: u64,
    base_reserve: u128,
    quote_reserve: u128,
) -> Option<(u64, u128, u128)> {
    if quote_in == 0 {
        return Some((0, base_reserve, quote_reserve));
    }
    let new_quote = quote_reserve.checked_add(quote_in as u128)?;
    let base_out_u128 = (quote_in as u128)
        .checked_mul(base_reserve)?
        .checked_div(new_quote)?;
    let base_out: u64 = base_out_u128.try_into().ok()?;
    let new_base = base_reserve.checked_sub(base_out as u128)?;
    Some((base_out, new_base, new_quote))
}

// Sell `base_in` base tokens for quote. Returns (quote_out, new_base_reserve, new_quote_reserve).
// Uniswap-V2 constant-product formula:
//   new_base_reserve  = base_reserve + base_in
//   quote_out         = floor(base_in × quote_reserve / new_base_reserve)
//   new_quote_reserve = quote_reserve - quote_out
// Floor on quote_out keeps the rounding residual in the pool, so k is non-decreasing.
pub fn vamm_sell_base(
    base_in: u64,
    base_reserve: u128,
    quote_reserve: u128,
) -> Option<(u64, u128, u128)> {
    if base_in == 0 {
        return Some((0, base_reserve, quote_reserve));
    }
    let new_base = base_reserve.checked_add(base_in as u128)?;
    let quote_out_u128 = (base_in as u128)
        .checked_mul(quote_reserve)?
        .checked_div(new_base)?;
    let quote_out: u64 = quote_out_u128.try_into().ok()?;
    let new_quote = quote_reserve.checked_sub(quote_out as u128)?;
    Some((quote_out, new_base, new_quote))
}

// ==============================================================================
// Fees
// ==============================================================================

// Compute taker fee on a notional amount. fee = notional × rate_bps / 10_000.
// Floor rounding in the pool's favor.
pub fn compute_fee(notional: u64, fee_rate_bps: u16) -> Option<u64> {
    let fee_u128 = (notional as u128)
        .checked_mul(fee_rate_bps as u128)?
        .checked_div(10_000)?;
    fee_u128.try_into().ok()
}

// Split a fee between insurance and protocol treasury by bps.
// Returns (to_insurance, to_protocol). Conservation: to_insurance + to_protocol == fee.
pub fn split_fee(fee: u64, insurance_cut_bps: u16) -> Option<(u64, u64)> {
    let to_insurance_u128 = (fee as u128)
        .checked_mul(insurance_cut_bps as u128)?
        .checked_div(10_000)?;
    let to_insurance: u64 = to_insurance_u128.try_into().ok()?;
    let to_protocol = fee.checked_sub(to_insurance)?;
    Some((to_insurance, to_protocol))
}

// ==============================================================================
// Position valuation & margin
// ==============================================================================

// Quote-denominated value of a position at current vAMM mark (ignores slippage).
// notional = |base| × quote_reserve / base_reserve
//
// Used for margin/PnL calculation, NOT for actual settlement.
// Settlement uses vamm_sell_base / vamm_buy_base which include price impact.
pub fn position_notional(
    abs_base: u64,
    base_reserve: u128,
    quote_reserve: u128,
) -> Option<u64> {
    if base_reserve == 0 {
        return None;
    }
    let notional_u128 = (abs_base as u128)
        .checked_mul(quote_reserve)?
        .checked_div(base_reserve)?;
    notional_u128.try_into().ok()
}

// Signed unrealized PnL from a position's entry notional vs current notional.
// Long (base > 0): PnL = current - entry (profit when price rises)
// Short (base < 0): PnL = entry - current (profit when price falls)
// Zero base: PnL = 0 (no position).
pub fn unrealized_pnl(
    base_asset_amount: i64,
    entry_notional: u64,
    current_notional: u64,
) -> Option<i64> {
    let entry_i = entry_notional as i128;
    let current_i = current_notional as i128;
    let pnl_i128 = if base_asset_amount > 0 {
        current_i.checked_sub(entry_i)?
    } else if base_asset_amount < 0 {
        entry_i.checked_sub(current_i)?
    } else {
        0
    };
    pnl_i128.try_into().ok()
}

// Required margin in quote (SOL) for a given notional and margin ratio.
// required = notional × margin_ratio_bps / 10_000. Floor rounding.
pub fn required_margin(notional: u64, margin_ratio_bps: u16) -> Option<u64> {
    let result = (notional as u128)
        .checked_mul(margin_ratio_bps as u128)?
        .checked_div(10_000)?;
    result.try_into().ok()
}

// At open: collateral must be ≥ initial_margin_ratio × notional.
// Returns None on math overflow, Some(true) if opening is permitted.
pub fn check_initial_margin(
    notional: u64,
    collateral: u64,
    initial_margin_ratio_bps: u16,
) -> Option<bool> {
    let required = required_margin(notional, initial_margin_ratio_bps)?;
    Some(collateral >= required)
}

// Liquidation gate: position stays safe while equity ≥ maintenance_ratio × notional.
// equity = collateral + unrealized_pnl - funding_owed (caller computes).
// Negative or zero equity → always liquidatable.
// Returns Some(true) if position is above maintenance (not liquidatable).
pub fn is_above_maintenance(
    notional: u64,
    equity: i64,
    maintenance_margin_ratio_bps: u16,
) -> Option<bool> {
    if equity <= 0 {
        return Some(false);
    }
    let required = required_margin(notional, maintenance_margin_ratio_bps)?;
    Some((equity as u64) >= required)
}

// Liquidator bonus: penalty_bps × notional / 10_000.
pub fn liquidation_penalty_for_notional(
    notional: u64,
    penalty_bps: u16,
) -> Option<u64> {
    let result = (notional as u128)
        .checked_mul(penalty_bps as u128)?
        .checked_div(10_000)?;
    result.try_into().ok()
}
