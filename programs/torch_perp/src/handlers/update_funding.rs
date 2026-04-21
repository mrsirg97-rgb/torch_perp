use anchor_lang::prelude::*;

use crate::contexts::UpdateFunding;
use crate::errors::TorchPerpError;
use crate::handlers::write_observation::record_observation;
use crate::pool::verify_and_read_reserves;

// Funding rate update crank.
//
// v1: funding is DISABLED. cumulative_funding_long/short stay at 0. This
// handler only advances the TWAP observation ring and bumps last_funding_slot
// so the crank is still usable by bots.
//
// v1.1: compute premium from mark vs index (TWAP of spot), apply per-slot
// delta to cumulative_funding_* since last_funding_slot.
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
    // Advance observation ring
    record_observation(market, pool_sol, pool_tokens, current_slot)?;

    // v1: no funding accrual. Just advance last_funding_slot so v1.1 upgrade
    // can start from a clean baseline.
    market.last_funding_slot = current_slot;

    // Sanity: funding period must not be zero (would divide by zero in v1.1).
    require!(
        market.funding_period_slots > 0,
        TorchPerpError::InvalidMarketConfig
    );

    Ok(())
}
