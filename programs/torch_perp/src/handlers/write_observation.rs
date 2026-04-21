use anchor_lang::prelude::*;

use crate::constants::TWAP_RING_SIZE;
use crate::contexts::WriteObservation;
use crate::errors::TorchPerpError;
use crate::math::advance_cumulative;
use crate::pool::verify_and_read_reserves;
use crate::state::Observation;

// Appends a new observation to the TWAP ring buffer. Idempotent within a slot
// (no-op if head already points to an observation with the current slot).
//
// Permissionless crank. Also called internally by handlers that touch the pool
// (see `record_observation` helper below) so the ring stays fresh during active
// trading; the external ix exists to keep observations flowing during quiet
// periods (no trade activity → no TWAP updates).
pub fn handler(ctx: Context<WriteObservation>) -> Result<()> {
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
    Ok(())
}

// Internal helper — callable from other handlers that touch the pool to keep
// observations fresh on every trade. Idempotent within a slot.
pub fn record_observation(
    market: &mut crate::state::PerpMarket,
    pool_sol: u64,
    pool_tokens: u64,
    current_slot: u64,
) -> Result<()> {
    let ring_size = TWAP_RING_SIZE;
    // Most recent observation = one slot behind twap_head.
    let prev_idx = if market.twap_head == 0 {
        ring_size - 1
    } else {
        (market.twap_head as usize) - 1
    };
    let prev = market.twap_observations[prev_idx];

    // No-op if we've already written an observation this slot.
    if prev.slot == current_slot {
        return Ok(());
    }

    let slot_delta = current_slot.saturating_sub(prev.slot);
    let cumulative_sol = advance_cumulative(prev.cumulative_sol, pool_sol, slot_delta)
        .ok_or(TorchPerpError::MathOverflow)?;
    let cumulative_token = advance_cumulative(prev.cumulative_token, pool_tokens, slot_delta)
        .ok_or(TorchPerpError::MathOverflow)?;

    let head_idx = market.twap_head as usize;
    market.twap_observations[head_idx] = Observation {
        slot: current_slot,
        cumulative_sol,
        cumulative_token,
    };
    market.twap_head = ((head_idx + 1) % ring_size) as u16;
    Ok(())
}
