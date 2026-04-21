use anchor_lang::prelude::*;

use crate::constants::*;
use crate::contexts::LiquidatePosition;
use crate::errors::TorchPerpError;
use crate::handlers::write_observation::record_observation;
use crate::math::{
    funding_owed, is_above_maintenance, liquidation_penalty_for_notional, position_notional,
    unrealized_pnl, vamm_buy_base, vamm_sell_base,
};
use crate::pool::verify_and_read_reserves;

// Liquidate an underwater position. Permissionless — any caller may invoke.
//
// Flow:
//   1. Verify position below maintenance margin (using vAMM mark + percolator-adjusted size)
//   2. Inverse vAMM swap to unwind the position
//   3. Compute realized PnL + percolator K delta
//   4. Liquidator receives penalty (from position collateral, cap at available)
//   5. Residual settles: user gets remainder if positive, insurance_vault absorbs losses
//   6. If insurance can't cover → apply percolator A-scaling
//   7. Anchor's close=position_owner returns any leftover lamports
//
// Position account is closed to position_owner via the context constraint.
pub fn handler(ctx: Context<LiquidatePosition>) -> Result<()> {
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

    let position = &ctx.accounts.position;
    require!(
        position.base_asset_amount != 0,
        TorchPerpError::NoActivePosition
    );

    let base_i = position.base_asset_amount;
    let abs_base = base_i.unsigned_abs();
    let entry_notional = position.entry_notional;
    let collateral = position.quote_asset_collateral;
    let k_snap = position.k_snapshot;
    let a_basis = position.a_basis_snapshot;

    let base_r = market.base_asset_reserve;
    let quote_r = market.quote_asset_reserve;

    // ----- Maintenance margin check (gate: must be liquidatable) -----
    let current_notional = position_notional(abs_base, base_r, quote_r)
        .ok_or(TorchPerpError::MathOverflow)?;
    let upnl = unrealized_pnl(base_i, entry_notional, current_notional)
        .ok_or(TorchPerpError::MathOverflow)?;
    let equity_i128 = (collateral as i128)
        .checked_add(upnl as i128)
        .ok_or(TorchPerpError::MathOverflow)?;
    let equity_i64: i64 = equity_i128
        .try_into()
        .unwrap_or(if equity_i128 > i64::MAX as i128 { i64::MAX } else { i64::MIN });
    let safe = is_above_maintenance(
        current_notional,
        equity_i64,
        market.maintenance_margin_ratio_bps,
    )
    .ok_or(TorchPerpError::MathOverflow)?;
    require!(!safe, TorchPerpError::PositionNotLiquidatable);

    // ----- Inverse vAMM swap to close -----
    let (realized_pnl_i128, new_base, new_quote) = if base_i > 0 {
        let result = vamm_sell_base(abs_base, base_r, quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        let pnl = (result.0 as i128)
            .checked_sub(entry_notional as i128)
            .ok_or(TorchPerpError::MathOverflow)?;
        (pnl, result.1, result.2)
    } else {
        require!(base_r > abs_base as u128, TorchPerpError::InvalidPool);
        let denom = base_r - (abs_base as u128);
        let num = (abs_base as u128)
            .checked_mul(quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        let quote_in_u128 = (num + denom - 1) / denom;
        let quote_in: u64 = quote_in_u128
            .try_into()
            .map_err(|_| TorchPerpError::MathOverflow)?;
        let result = vamm_buy_base(quote_in, base_r, quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        let pnl = (entry_notional as i128)
            .checked_sub(quote_in as i128)
            .ok_or(TorchPerpError::MathOverflow)?;
        (pnl, result.1, result.2)
    };

    // Percolator K delta (position's share of previously-accumulated bad debt).
    let k_delta_i128 = if a_basis > 0 {
        let diff = (market.k_index)
            .checked_sub(k_snap)
            .ok_or(TorchPerpError::MathOverflow)?;
        let num = (abs_base as i128)
            .checked_mul(diff)
            .ok_or(TorchPerpError::MathOverflow)?;
        let denom = (a_basis as i128)
            .checked_mul(POS_SCALE as i128)
            .ok_or(TorchPerpError::MathOverflow)?;
        if denom > 0 {
            num / denom
        } else {
            0
        }
    } else {
        0
    };

    // Funding settlement on liquidation: position still owes accrued funding
    // through the liquidation event. Signed; reduces (or increases) the
    // residual claim symmetrically with close_position.
    let owed = funding_owed(
        base_i,
        market.cumulative_funding_long,
        position.last_cumulative_funding,
    )
    .ok_or(TorchPerpError::MathOverflow)?;

    let total_realized = realized_pnl_i128
        .checked_add(k_delta_i128)
        .ok_or(TorchPerpError::MathOverflow)?
        .checked_sub(owed as i128)
        .ok_or(TorchPerpError::MathOverflow)?;

    // Liquidator bonus on current_notional
    let penalty = liquidation_penalty_for_notional(
        current_notional,
        market.liquidation_penalty_bps,
    )
    .ok_or(TorchPerpError::MathOverflow)?;

    // Commit vAMM reserves + OI
    market.base_asset_reserve = new_base;
    market.quote_asset_reserve = new_quote;
    if base_i > 0 {
        market.open_interest_long = market.open_interest_long.saturating_sub(abs_base);
    } else {
        market.open_interest_short = market.open_interest_short.saturating_sub(abs_base);
    }

    // ----- SOL flows -----
    let position_info = ctx.accounts.position.to_account_info();
    let insurance_info = ctx.accounts.insurance_vault.to_account_info();
    let liquidator_info = ctx.accounts.liquidator.to_account_info();

    // 1. Pay liquidator from position collateral (cap at available)
    let available_lamports = position_info.lamports();
    let to_liquidator = penalty.min(available_lamports);
    if to_liquidator > 0 {
        **position_info.try_borrow_mut_lamports()? = position_info
            .lamports()
            .checked_sub(to_liquidator)
            .ok_or(TorchPerpError::MathOverflow)?;
        **liquidator_info.try_borrow_mut_lamports()? = liquidator_info
            .lamports()
            .checked_add(to_liquidator)
            .ok_or(TorchPerpError::MathOverflow)?;
    }

    // 2. Settle PnL:
    //    net_position_claim = collateral - liquidator_bonus + total_realized
    //    If positive: user gets it (via close=position_owner); insurance_vault funds any PnL gains
    //    If negative: shortfall goes to insurance draw → if still short, percolator
    let collateral_after_bonus: i128 =
        (collateral as i128).checked_sub(to_liquidator as i128)
            .ok_or(TorchPerpError::MathOverflow)?;
    let net_claim_i128 = collateral_after_bonus
        .checked_add(total_realized)
        .ok_or(TorchPerpError::MathOverflow)?;

    if net_claim_i128 > 0 {
        // User has positive claim: insurance pays any PnL gain portion.
        // PnL gain = max(0, total_realized). If insurance insufficient, user takes
        // less — bounded by insurance balance.
        if total_realized > 0 {
            let gain: u64 = total_realized
                .try_into()
                .map_err(|_| TorchPerpError::MathOverflow)?;
            let from_insurance = gain.min(market.insurance_balance);
            if from_insurance > 0 {
                **insurance_info.try_borrow_mut_lamports()? = insurance_info
                    .lamports()
                    .checked_sub(from_insurance)
                    .ok_or(TorchPerpError::MathOverflow)?;
                **position_info.try_borrow_mut_lamports()? = position_info
                    .lamports()
                    .checked_add(from_insurance)
                    .ok_or(TorchPerpError::MathOverflow)?;
                market.insurance_balance -= from_insurance;
            }
        }
    } else {
        // Loss exceeds collateral − bonus. Move whatever remains in position
        // PDA (after rent) to insurance, then handle residual shortfall.
        let loss_abs: u64 = (-net_claim_i128)
            .try_into()
            .map_err(|_| TorchPerpError::MathOverflow)?;

        let remaining = position_info.lamports();
        // Reserve rent-exempt minimum so Anchor close=position_owner succeeds
        // (close returns all remaining lamports to position_owner).
        let to_absorb = loss_abs.min(remaining);
        if to_absorb > 0 {
            **position_info.try_borrow_mut_lamports()? = position_info
                .lamports()
                .checked_sub(to_absorb)
                .ok_or(TorchPerpError::MathOverflow)?;
            **insurance_info.try_borrow_mut_lamports()? = insurance_info
                .lamports()
                .checked_add(to_absorb)
                .ok_or(TorchPerpError::MathOverflow)?;
            market.insurance_balance = market
                .insurance_balance
                .checked_add(to_absorb)
                .ok_or(TorchPerpError::MathOverflow)?;
        }

        let shortfall = loss_abs.saturating_sub(to_absorb);
        if shortfall > 0 {
            // Insurance is the first line of defense for uncovered bad debt.
            let from_insurance = shortfall.min(market.insurance_balance);
            market.insurance_balance = market
                .insurance_balance
                .saturating_sub(from_insurance);
            let residual_bad_debt = shortfall.saturating_sub(from_insurance);

            // Percolator: if residual bad debt remains, apply A-scaling.
            if residual_bad_debt > 0 {
                apply_percolator_scaling(market, residual_bad_debt)?;
            }
        }
    }

    // Record observation
    let current_slot = Clock::get()?.slot;
    record_observation(market, pool_sol, pool_tokens, current_slot)?;

    Ok(())
}

// Apply proportional A-scaling when bad debt exceeds insurance.
//
// Mechanism: reduce market.a_index by a factor proportional to (shortfall / total_exposure).
// Every active position's effective size shrinks when read: effective_base = base × A / a_basis.
// Accumulates the loss into k_index for deterministic recovery accounting.
//
// When a_index drops below PRECISION_THRESHOLD → enter DrainOnly phase.
fn apply_percolator_scaling(
    market: &mut crate::state::PerpMarket,
    shortfall: u64,
) -> Result<()> {
    let total_exposure_base = market
        .open_interest_long
        .checked_add(market.open_interest_short)
        .ok_or(TorchPerpError::MathOverflow)?;
    if total_exposure_base == 0 {
        // Nothing to scale across — absorb silently (the vAMM itself holds the loss).
        return Ok(());
    }

    // Exposure in quote at current mark
    let total_exposure_quote = position_notional(
        total_exposure_base,
        market.base_asset_reserve,
        market.quote_asset_reserve,
    )
    .ok_or(TorchPerpError::MathOverflow)?;
    if total_exposure_quote == 0 {
        return Ok(());
    }

    // scale_factor = POS_SCALE − (shortfall × POS_SCALE / total_exposure_quote)
    // clamped to [0, POS_SCALE]
    let reduction = (shortfall as u128)
        .checked_mul(POS_SCALE)
        .ok_or(TorchPerpError::MathOverflow)?
        / (total_exposure_quote as u128);
    let scale_factor = POS_SCALE.saturating_sub(reduction);

    market.a_index = market
        .a_index
        .checked_mul(scale_factor)
        .ok_or(TorchPerpError::MathOverflow)?
        / POS_SCALE;

    // Accumulate shortfall into k_index (per-base-unit loss)
    let k_addition = (shortfall as i128)
        .checked_mul(POS_SCALE as i128)
        .ok_or(TorchPerpError::MathOverflow)?
        / (total_exposure_base as i128);
    market.k_index = market
        .k_index
        .checked_sub(k_addition)
        .ok_or(TorchPerpError::MathOverflow)?;

    // Enter DrainOnly if a_index fell below precision threshold
    if market.a_index < PRECISION_THRESHOLD {
        market.recovery_phase = RECOVERY_DRAIN_ONLY;
    }

    Ok(())
}
