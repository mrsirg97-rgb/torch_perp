use anchor_lang::prelude::*;

use crate::constants::*;
use crate::contexts::PartialClosePosition;
use crate::errors::TorchPerpError;
use crate::handlers::write_observation::record_observation;
use crate::math::{
    compute_fee, funding_owed, proportional_entry, split_fee, vamm_buy_base, vamm_sell_base,
};
use crate::pool::verify_and_read_reserves;

// Close a SUBSET of a position. Keeps the position account open with reduced
// base_asset_amount and proportionally-reduced entry_notional.
//
// Flow:
//   1. Validate 0 < base_to_close < abs_base (strict — full close uses close_position)
//   2. Inverse vAMM swap on base_to_close (same direction logic as close)
//   3. Compute realized PnL on the CLOSED portion using proportional entry
//   4. Settle funding + K delta for the CLOSED portion; reset snapshots so the
//      remaining position starts fresh from current cumulative indices
//   5. Fee on closed notional
//   6. Settle payout/loss through insurance_vault (same pattern as close_position)
//   7. Shrink position: base_asset_amount and entry_notional reduced proportionally
pub fn handler(ctx: Context<PartialClosePosition>, base_to_close: u64, min_quote_out: u64) -> Result<()> {
    require!(base_to_close > 0, TorchPerpError::ZeroInput);

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

    require_keys_eq!(
        ctx.accounts.protocol_treasury.key(),
        ctx.accounts.global_config.protocol_treasury,
        TorchPerpError::Unauthorized
    );

    let position = &ctx.accounts.position;
    require!(
        position.base_asset_amount != 0,
        TorchPerpError::NoActivePosition
    );

    let base_i = position.base_asset_amount;
    let abs_base = base_i.unsigned_abs();
    require!(
        base_to_close < abs_base,
        TorchPerpError::InvalidMarketConfig
    );

    let entry_notional = position.entry_notional;
    let collateral = position.quote_asset_collateral;
    let k_snap = position.k_snapshot;
    let a_basis = position.a_basis_snapshot;
    let funding_snap = position.last_cumulative_funding;

    let base_r = market.base_asset_reserve;
    let quote_r = market.quote_asset_reserve;

    // ----- Inverse vAMM swap on the closed portion -----
    let (realized_pnl_i128, new_base, new_quote, close_notional) = if base_i > 0 {
        // Partial long close: sell base_to_close
        let result = vamm_sell_base(base_to_close, base_r, quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        let quote_received = result.0;
        require!(
            quote_received >= min_quote_out,
            TorchPerpError::SlippageExceeded
        );
        let prop_entry = proportional_entry(entry_notional, base_to_close, abs_base)
            .ok_or(TorchPerpError::MathOverflow)?;
        let pnl = (quote_received as i128)
            .checked_sub(prop_entry as i128)
            .ok_or(TorchPerpError::MathOverflow)?;
        (pnl, result.1, result.2, quote_received)
    } else {
        // Partial short close: buy base_to_close via ceil-divided quote_in
        require!(base_r > base_to_close as u128, TorchPerpError::InvalidPool);
        let denom = base_r - (base_to_close as u128);
        let num = (base_to_close as u128)
            .checked_mul(quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        let quote_in_u128 = (num + denom - 1) / denom;
        let quote_in: u64 = quote_in_u128
            .try_into()
            .map_err(|_| TorchPerpError::MathOverflow)?;
        let result = vamm_buy_base(quote_in, base_r, quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        let prop_entry = proportional_entry(entry_notional, base_to_close, abs_base)
            .ok_or(TorchPerpError::MathOverflow)?;
        let pnl = (prop_entry as i128)
            .checked_sub(quote_in as i128)
            .ok_or(TorchPerpError::MathOverflow)?;
        (pnl, result.1, result.2, quote_in)
    };

    // ----- K delta for the CLOSED portion -----
    let k_delta_i128 = if a_basis > 0 {
        let diff = market
            .k_index
            .checked_sub(k_snap)
            .ok_or(TorchPerpError::MathOverflow)?;
        let num = (base_to_close as i128)
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

    // ----- Funding settlement for the CLOSED portion only -----
    // Signed base for the closed slice matches original direction.
    let closed_signed: i64 = if base_i > 0 {
        base_to_close as i64
    } else {
        -(base_to_close as i64)
    };
    let owed = funding_owed(closed_signed, market.cumulative_funding_long, funding_snap)
        .ok_or(TorchPerpError::MathOverflow)?;

    let total_realized = realized_pnl_i128
        .checked_add(k_delta_i128)
        .ok_or(TorchPerpError::MathOverflow)?
        .checked_sub(owed as i128)
        .ok_or(TorchPerpError::MathOverflow)?;

    // ----- Fee on closing notional -----
    let fee = compute_fee(close_notional, ctx.accounts.global_config.fee_rate_bps)
        .ok_or(TorchPerpError::MathOverflow)?;
    let (to_insurance_fee, to_protocol_fee) =
        split_fee(fee, ctx.accounts.global_config.insurance_fund_cut_bps)
            .ok_or(TorchPerpError::MathOverflow)?;

    // ----- Commit vAMM reserves + OI -----
    market.base_asset_reserve = new_base;
    market.quote_asset_reserve = new_quote;
    if base_i > 0 {
        market.open_interest_long = market.open_interest_long.saturating_sub(base_to_close);
    } else {
        market.open_interest_short = market.open_interest_short.saturating_sub(base_to_close);
    }

    // ----- SOL flows -----
    // Partial close pays from collateral (for fees + losses) and receives from
    // insurance (for gains). Position stays open with reduced collateral
    // tracking — but we DON'T actually move collateral: the collateral field
    // stays the same for the remaining position (user can withdraw separately
    // if they want to reduce leverage).
    //
    // For partial close, the realized portion is paid out DIRECTLY to the user
    // as a lamport transfer from position/insurance → user.
    // Fees come out of position's collateral lamports.
    let position_info = ctx.accounts.position.to_account_info();
    let insurance_info = ctx.accounts.insurance_vault.to_account_info();
    let protocol_info = ctx.accounts.protocol_treasury.to_account_info();
    let user_info = ctx.accounts.user.to_account_info();

    // Fees: from position PDA to insurance + protocol
    if to_insurance_fee > 0 {
        **position_info.try_borrow_mut_lamports()? = position_info
            .lamports()
            .checked_sub(to_insurance_fee)
            .ok_or(TorchPerpError::MathOverflow)?;
        **insurance_info.try_borrow_mut_lamports()? = insurance_info
            .lamports()
            .checked_add(to_insurance_fee)
            .ok_or(TorchPerpError::MathOverflow)?;
        market.insurance_balance = market
            .insurance_balance
            .checked_add(to_insurance_fee)
            .ok_or(TorchPerpError::MathOverflow)?;
    }
    if to_protocol_fee > 0 {
        **position_info.try_borrow_mut_lamports()? = position_info
            .lamports()
            .checked_sub(to_protocol_fee)
            .ok_or(TorchPerpError::MathOverflow)?;
        **protocol_info.try_borrow_mut_lamports()? = protocol_info
            .lamports()
            .checked_add(to_protocol_fee)
            .ok_or(TorchPerpError::MathOverflow)?;
    }

    // Realized PnL settlement: from insurance to user if positive, from position to insurance if negative
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
            **user_info.try_borrow_mut_lamports()? = user_info
                .lamports()
                .checked_add(from_insurance)
                .ok_or(TorchPerpError::MathOverflow)?;
            market.insurance_balance -= from_insurance;
        }
    } else if total_realized < 0 {
        let loss: u64 = (-total_realized)
            .try_into()
            .map_err(|_| TorchPerpError::MathOverflow)?;
        // Draw from position collateral (cap at available lamports)
        let available = position_info
            .lamports()
            .saturating_sub(Rent::get()?.minimum_balance(8 + 165));
        let to_move = loss.min(available);
        if to_move > 0 {
            **position_info.try_borrow_mut_lamports()? = position_info
                .lamports()
                .checked_sub(to_move)
                .ok_or(TorchPerpError::MathOverflow)?;
            **insurance_info.try_borrow_mut_lamports()? = insurance_info
                .lamports()
                .checked_add(to_move)
                .ok_or(TorchPerpError::MathOverflow)?;
            market.insurance_balance = market
                .insurance_balance
                .checked_add(to_move)
                .ok_or(TorchPerpError::MathOverflow)?;
        }
    }

    // ----- Update position state -----
    let position = &mut ctx.accounts.position;
    let prop_entry = proportional_entry(entry_notional, base_to_close, abs_base)
        .ok_or(TorchPerpError::MathOverflow)?;
    if base_i > 0 {
        position.base_asset_amount = base_i - (base_to_close as i64);
    } else {
        position.base_asset_amount = base_i + (base_to_close as i64);
    }
    position.entry_notional = entry_notional.saturating_sub(prop_entry);
    // Update collateral ledger to reflect fees + losses drawn from lamports
    position.quote_asset_collateral = position.quote_asset_collateral.saturating_sub(fee);
    if total_realized < 0 {
        let loss: u64 = (-total_realized)
            .try_into()
            .map_err(|_| TorchPerpError::MathOverflow)?;
        position.quote_asset_collateral = position.quote_asset_collateral.saturating_sub(loss);
    }
    // Reset funding + K snapshots so remaining position accrues from current
    position.last_cumulative_funding = market.cumulative_funding_long;
    position.k_snapshot = market.k_index;

    // Record observation
    let current_slot = Clock::get()?.slot;
    record_observation(market, pool_sol, pool_tokens, current_slot)?;

    let _ = collateral; // silence unused warning (captured before mut borrow)
    Ok(())
}
