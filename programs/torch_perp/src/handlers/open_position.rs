use anchor_lang::prelude::*;
use anchor_lang::system_program;

use crate::constants::*;
use crate::contexts::OpenPosition;
use crate::errors::TorchPerpError;
use crate::handlers::write_observation::record_observation;
use crate::math::{check_initial_margin, compute_fee, split_fee, vamm_buy_base, vamm_sell_base};
use crate::pool::verify_and_read_reserves;

// Open a leveraged perp position. `base_amount` is signed:
//   +N → long  N base units
//   -N → short N base units
//
// Flow:
//   1. Validate inputs + market phase + pool refs
//   2. Compute vAMM swap (buy base for long, sell base for short)
//   3. Price-impact gate using max_price_impact_bps
//   4. Initial-margin gate (collateral ≥ notional × IMR)
//   5. Transfer collateral → position PDA, fees → insurance + protocol
//   6. Commit vAMM reserves, OI, position state
//   7. Record TWAP observation
pub fn handler(
    ctx: Context<OpenPosition>,
    base_amount: i64,
    collateral_lamports: u64,
    max_price_impact_bps: u16,
) -> Result<()> {
    require!(base_amount != 0, TorchPerpError::ZeroInput);
    require!(collateral_lamports > 0, TorchPerpError::ZeroInput);
    require!(
        max_price_impact_bps <= BPS_DENOMINATOR as u16,
        TorchPerpError::InvalidMarketConfig
    );

    let market = &mut ctx.accounts.market;
    require!(
        market.recovery_phase == RECOVERY_NORMAL,
        TorchPerpError::MarketInDrainOnly
    );

    // Pool refs must match market's stored references. Also reads spot reserves for TWAP.
    let (pool_sol, pool_tokens) = verify_and_read_reserves(
        &ctx.accounts.spot_pool,
        &ctx.accounts.spot_vault_0,
        &ctx.accounts.spot_vault_1,
        &market.spot_pool,
        &market.spot_vault_0,
        &market.spot_vault_1,
        market.is_wsol_token_0,
    )?;

    // Protocol treasury must match GlobalConfig
    require_keys_eq!(
        ctx.accounts.protocol_treasury.key(),
        ctx.accounts.global_config.protocol_treasury,
        TorchPerpError::Unauthorized
    );

    // Reject if there's an existing non-zero position (user must close first).
    require!(
        ctx.accounts.position.base_asset_amount == 0,
        TorchPerpError::PositionAlreadyExists
    );

    let base_r = market.base_asset_reserve;
    let quote_r = market.quote_asset_reserve;
    let abs_base = base_amount.unsigned_abs();

    // ----- vAMM swap -----
    let (quote_notional, new_base, new_quote) = if base_amount > 0 {
        // Long: find quote_in such that vamm_buy_base yields at least abs_base.
        // Formula: quote_in = ceil(abs_base × quote_r / (base_r - abs_base))
        require!(base_r > abs_base as u128, TorchPerpError::InvalidPool);
        let denom = base_r - (abs_base as u128);
        let num = (abs_base as u128)
            .checked_mul(quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        // ceil division
        let quote_in_u128 = (num + denom - 1) / denom;
        let quote_in: u64 = quote_in_u128
            .try_into()
            .map_err(|_| TorchPerpError::MathOverflow)?;
        let result = vamm_buy_base(quote_in, base_r, quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        (quote_in, result.1, result.2)
    } else {
        // Short: sell abs_base, receive quote_out.
        let result = vamm_sell_base(abs_base, base_r, quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        (result.0, result.1, result.2)
    };

    require!(quote_notional > 0, TorchPerpError::ZeroInput);

    // ----- Price impact check -----
    // Ratio equivalence: |new_quote × base_r - new_base × quote_r| × 10000 / (new_base × quote_r)
    let lhs = (new_quote)
        .checked_mul(base_r)
        .ok_or(TorchPerpError::MathOverflow)?;
    let rhs = (new_base)
        .checked_mul(quote_r)
        .ok_or(TorchPerpError::MathOverflow)?;
    let (larger, smaller) = if lhs > rhs { (lhs, rhs) } else { (rhs, lhs) };
    let diff = larger - smaller;
    if smaller > 0 {
        let impact_bps_u128 = diff
            .checked_mul(BPS_DENOMINATOR as u128)
            .ok_or(TorchPerpError::MathOverflow)?
            / smaller;
        require!(
            impact_bps_u128 <= max_price_impact_bps as u128,
            TorchPerpError::SlippageExceeded
        );
    }

    // ----- IMR check -----
    let passes_imr = check_initial_margin(
        quote_notional,
        collateral_lamports,
        market.initial_margin_ratio_bps,
    )
    .ok_or(TorchPerpError::MathOverflow)?;
    require!(passes_imr, TorchPerpError::MaxLeverageExceeded);

    // ----- Fees -----
    let fee = compute_fee(quote_notional, ctx.accounts.global_config.fee_rate_bps)
        .ok_or(TorchPerpError::MathOverflow)?;
    let (to_insurance, to_protocol) =
        split_fee(fee, ctx.accounts.global_config.insurance_fund_cut_bps)
            .ok_or(TorchPerpError::MathOverflow)?;

    // ----- SOL transfers -----
    // Collateral: user → position PDA
    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.user.to_account_info(),
                to: ctx.accounts.position.to_account_info(),
            },
        ),
        collateral_lamports,
    )?;

    // Fees: user → insurance_vault + user → protocol_treasury
    if to_insurance > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.user.to_account_info(),
                    to: ctx.accounts.insurance_vault.to_account_info(),
                },
            ),
            to_insurance,
        )?;
        market.insurance_balance = market
            .insurance_balance
            .checked_add(to_insurance)
            .ok_or(TorchPerpError::MathOverflow)?;
    }
    if to_protocol > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                system_program::Transfer {
                    from: ctx.accounts.user.to_account_info(),
                    to: ctx.accounts.protocol_treasury.to_account_info(),
                },
            ),
            to_protocol,
        )?;
    }

    // ----- Commit state -----
    market.base_asset_reserve = new_base;
    market.quote_asset_reserve = new_quote;

    if base_amount > 0 {
        market.open_interest_long = market
            .open_interest_long
            .checked_add(abs_base)
            .ok_or(TorchPerpError::MathOverflow)?;
    } else {
        market.open_interest_short = market
            .open_interest_short
            .checked_add(abs_base)
            .ok_or(TorchPerpError::MathOverflow)?;
    }

    let current_slot = Clock::get()?.slot;
    // Single-index funding design: we snapshot cumulative_funding_long for all
    // positions. At settlement, `funding_owed` multiplies by base_asset_amount
    // which is signed, so shorts automatically get the opposite sign.
    let funding_snapshot = market.cumulative_funding_long;

    let user_key = ctx.accounts.user.key();
    let market_key = market.key();
    let a_idx = market.a_index;
    let k_idx = market.k_index;
    let epoch = market.epoch;
    let position_bump = ctx.bumps.position;

    let position = &mut ctx.accounts.position;
    position.user = user_key;
    position.market = market_key;
    position.base_asset_amount = base_amount;
    position.quote_asset_collateral = collateral_lamports;
    position.entry_notional = quote_notional;
    position.last_cumulative_funding = funding_snapshot;
    position.a_basis_snapshot = a_idx;
    position.k_snapshot = k_idx;
    position.matured_pnl = 0;
    position.open_epoch = epoch;
    position.open_slot = current_slot;
    position.bump = position_bump;

    // Record observation for TWAP
    record_observation(market, pool_sol, pool_tokens, current_slot)?;

    Ok(())
}
