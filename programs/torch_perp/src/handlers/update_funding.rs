use anchor_lang::prelude::*;

use crate::constants::TWAP_RING_SIZE;
use crate::contexts::UpdateFunding;
use crate::errors::TorchPerpError;
use crate::handlers::write_observation::record_observation;
use crate::math::{
    funding_delta, mark_price_scaled, premium_signed, twap_price_scaled,
};
use crate::pool::verify_and_read_reserves;
use crate::state::{Observation, PerpMarket};

// Funding rate crank (v1.1 active).
//
// Flow:
//   1. Advance TWAP observation ring with current spot reserves.
//   2. Compute vAMM mark (quote/base × POS_SCALE).
//   3. Compute TWAP index price from ring's oldest-to-newest observation span.
//   4. premium = mark - index (signed, POS_SCALE-scaled).
//   5. Accrue premium × slots_elapsed / funding_period_slots into
//      cumulative_funding_long (single-index design; shorts auto-flip via
//      signed base_asset_amount on settlement).
//   6. Mirror cumulative_funding_short for symmetry (same value).
//   7. Bump last_funding_slot.
//
// Early-return no-op (without rejecting the tx) when the TWAP window has
// insufficient data — caller can't act on funding this tick, but the
// observation write always succeeds. This keeps the crank usable during
// initial warmup.
pub fn handler(ctx: Context<UpdateFunding>) -> Result<()> {
    let market = &mut ctx.accounts.market;

    let (pool_sol, pool_tokens) = verify_and_read_reserves(
        &ctx.accounts.spot_pool,
        &ctx.accounts.spot_vault_0,
        &ctx.accounts.spot_vault_1,
        &market.spot_pool,
        &market.spot_vault_0,
        &market.spot_vault_1,
        market.is_wsol_token_0,
    )?;

    let current_slot = Clock::get()?.slot;
    record_observation(market, pool_sol, pool_tokens, current_slot)?;

    // Compute TWAP index from ring (oldest-valid → newest).
    let index_scaled = match read_twap_index(market) {
        Some(p) => p,
        None => {
            // Insufficient ring history → skip funding accrual this tick.
            market.last_funding_slot = current_slot;
            return Ok(());
        }
    };

    let mark_scaled = mark_price_scaled(market.base_asset_reserve, market.quote_asset_reserve)
        .ok_or(TorchPerpError::MathOverflow)?;

    let premium = premium_signed(mark_scaled, index_scaled)
        .ok_or(TorchPerpError::MathOverflow)?;

    let slots_elapsed = current_slot.saturating_sub(market.last_funding_slot);
    if slots_elapsed > 0 && premium != 0 {
        let delta = funding_delta(premium, slots_elapsed, market.funding_period_slots)
            .ok_or(TorchPerpError::MathOverflow)?;
        market.cumulative_funding_long = market
            .cumulative_funding_long
            .checked_add(delta)
            .ok_or(TorchPerpError::MathOverflow)?;
        // Mirror for downstream readers; settlement only consults the long field.
        market.cumulative_funding_short = market.cumulative_funding_long;
    }
    market.last_funding_slot = current_slot;

    require!(
        market.funding_period_slots > 0,
        TorchPerpError::InvalidMarketConfig
    );
    Ok(())
}

// Find the oldest observation within the ring that precedes the newest one,
// then return the TWAP scaled price over (oldest → newest). Returns None if
// the ring doesn't have two valid observations yet (warmup).
fn read_twap_index(market: &PerpMarket) -> Option<u128> {
    let ring_size = TWAP_RING_SIZE;
    let head = market.twap_head as usize;
    let newest_idx = if head == 0 { ring_size - 1 } else { head - 1 };
    let newest: Observation = market.twap_observations[newest_idx];
    if newest.slot == 0 {
        return None;
    }

    // Walk backwards around the ring to find the oldest observation with slot > 0.
    let mut oldest = newest;
    for i in 1..ring_size {
        let idx = (newest_idx + ring_size - i) % ring_size;
        let obs = market.twap_observations[idx];
        if obs.slot > 0 && obs.slot < oldest.slot {
            oldest = obs;
        }
    }

    if oldest.slot >= newest.slot {
        return None; // only a single observation exists
    }

    let sol_delta = newest.cumulative_sol.checked_sub(oldest.cumulative_sol)?;
    let token_delta = newest.cumulative_token.checked_sub(oldest.cumulative_token)?;
    twap_price_scaled(sol_delta, token_delta)
}
