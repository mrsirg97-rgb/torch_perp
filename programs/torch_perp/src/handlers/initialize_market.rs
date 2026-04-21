use anchor_lang::prelude::*;

use crate::constants::*;
use crate::contexts::InitializeMarket;
use crate::errors::TorchPerpError;
use crate::pool::read_raydium_pool_reserves;
use crate::state::Observation;

// Permissionless market init. Seeds vAMM reserves from spot pool price so that
// vAMM's initial mark == spot. Caller picks the SOL-side depth
// (`vamm_quote_reserve`); the base-side reserve is derived deterministically
// to preserve the spot price.
//
// Validates:
//   - fee/leverage/margin params are within sensible bounds
//   - spot pool is a valid Raydium CPMM pool for this mint paired with WSOL
//   - vamm_quote_reserve > 0
//
// Initializes:
//   - vAMM reserves at spot price
//   - vamm_k_invariant = base × quote
//   - a_index = POS_SCALE, k_index = 0, recovery_phase = Normal, epoch = 0
//   - First TWAP observation
#[allow(clippy::too_many_arguments)]
pub fn handler(
    ctx: Context<InitializeMarket>,
    initial_margin_ratio_bps: u16,
    maintenance_margin_ratio_bps: u16,
    liquidation_penalty_bps: u16,
    funding_period_slots: u64,
    vamm_quote_reserve: u128,
) -> Result<()> {
    // Parameter sanity bounds.
    // Initial margin must be > 0 and ≤ 100%.
    require!(
        initial_margin_ratio_bps > 0 && initial_margin_ratio_bps <= BPS_DENOMINATOR as u16,
        TorchPerpError::InvalidMarketConfig
    );
    // Maintenance < initial (else liquidation at open). Both bounded.
    require!(
        maintenance_margin_ratio_bps > 0
            && maintenance_margin_ratio_bps < initial_margin_ratio_bps,
        TorchPerpError::InvalidMarketConfig
    );
    require!(
        liquidation_penalty_bps < BPS_DENOMINATOR as u16,
        TorchPerpError::InvalidMarketConfig
    );
    require!(
        funding_period_slots > 0,
        TorchPerpError::InvalidMarketConfig
    );
    require!(
        vamm_quote_reserve > 0,
        TorchPerpError::ZeroInput
    );

    // Validate spot pool + read current reserves
    let (pool_sol, pool_tokens, is_wsol_token_0) = read_raydium_pool_reserves(
        &ctx.accounts.spot_pool,
        &ctx.accounts.spot_vault_0,
        &ctx.accounts.spot_vault_1,
        &ctx.accounts.mint.key(),
    )?;

    // Seed vAMM base reserve so that vAMM price == spot price:
    //   vamm_base = vamm_quote × pool_tokens / pool_sol
    // (price_quote_per_base = quote / base = pool_sol / pool_tokens)
    let vamm_base_reserve: u128 = vamm_quote_reserve
        .checked_mul(pool_tokens as u128)
        .ok_or(TorchPerpError::MathOverflow)?
        .checked_div(pool_sol as u128)
        .ok_or(TorchPerpError::MathOverflow)?;
    require!(
        vamm_base_reserve > 0,
        TorchPerpError::InvalidMarketConfig
    );

    let vamm_k_invariant = vamm_base_reserve
        .checked_mul(vamm_quote_reserve)
        .ok_or(TorchPerpError::MathOverflow)?;

    let current_slot = Clock::get()?.slot;

    let market = &mut ctx.accounts.market;
    market.mint = ctx.accounts.mint.key();
    market.spot_pool = ctx.accounts.spot_pool.key();
    market.spot_vault_0 = ctx.accounts.spot_vault_0.key();
    market.spot_vault_1 = ctx.accounts.spot_vault_1.key();
    market.is_wsol_token_0 = is_wsol_token_0;

    market.base_asset_reserve = vamm_base_reserve;
    market.quote_asset_reserve = vamm_quote_reserve;
    market.vamm_k_invariant = vamm_k_invariant;

    market.initial_margin_ratio_bps = initial_margin_ratio_bps;
    market.maintenance_margin_ratio_bps = maintenance_margin_ratio_bps;
    market.liquidation_penalty_bps = liquidation_penalty_bps;

    market.cumulative_funding_long = 0;
    market.cumulative_funding_short = 0;
    market.last_funding_slot = current_slot;
    market.funding_period_slots = funding_period_slots;

    market.open_interest_long = 0;
    market.open_interest_short = 0;

    // Seed observation ring with a single observation at current slot.
    // Cumulative values start at 0 — TWAP readers compute deltas.
    market.twap_observations = [Observation::default(); TWAP_RING_SIZE];
    market.twap_observations[0] = Observation {
        slot: current_slot,
        cumulative_sol: 0,
        cumulative_token: 0,
    };
    market.twap_head = 1 % (TWAP_RING_SIZE as u16);

    market.insurance_balance = 0;

    // Percolator initial state: Normal, A at full precision, K zero, epoch 0.
    market.a_index = POS_SCALE;
    market.k_index = 0;
    market.recovery_phase = RECOVERY_NORMAL;
    market.epoch = 0;

    market.bump = ctx.bumps.market;

    Ok(())
}
