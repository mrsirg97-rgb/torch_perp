use anchor_lang::prelude::*;

use crate::constants::*;
use crate::contexts::ClosePosition;
use crate::errors::TorchPerpError;
use crate::handlers::write_observation::record_observation;
use crate::math::{compute_fee, split_fee, vamm_buy_base, vamm_sell_base};
use crate::pool::verify_and_read_reserves;

// Close a position fully.
// Flow:
//   1. Inverse vAMM swap (sell base for long, buy base for short)
//   2. Compute realized PnL + percolator K delta
//   3. Settle: user receives collateral + realized_pnl - fee, clamped ≥ 0
//   4. Delta flows to/from insurance_vault (zero-sum against vAMM)
//   5. Close position account (rent to user via Anchor constraint)
pub fn handler(ctx: Context<ClosePosition>, min_quote_out: u64) -> Result<()> {
    let market = &mut ctx.accounts.market;

    // Pool refs + reserves
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
    let entry_notional = position.entry_notional;
    let collateral = position.quote_asset_collateral;
    let k_snap = position.k_snapshot;
    let a_basis = position.a_basis_snapshot;

    let base_r = market.base_asset_reserve;
    let quote_r = market.quote_asset_reserve;

    // Inverse swap
    let (realized_pnl_i128, new_base, new_quote, close_notional) = if base_i > 0 {
        // Long close: sell abs_base
        let result = vamm_sell_base(abs_base, base_r, quote_r)
            .ok_or(TorchPerpError::MathOverflow)?;
        let quote_received = result.0;
        require!(
            quote_received >= min_quote_out,
            TorchPerpError::SlippageExceeded
        );
        // realized = quote_received - entry_notional
        let pnl = (quote_received as i128)
            .checked_sub(entry_notional as i128)
            .ok_or(TorchPerpError::MathOverflow)?;
        (pnl, result.1, result.2, quote_received)
    } else {
        // Short close: buy abs_base — solve for quote_in = ceil(abs_base × quote_r / (base_r - abs_base))
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
        // realized = entry_notional - quote_in (short profits when price falls → quote_in < entry_notional)
        let pnl = (entry_notional as i128)
            .checked_sub(quote_in as i128)
            .ok_or(TorchPerpError::MathOverflow)?;
        (pnl, result.1, result.2, quote_in)
    };

    // Percolator K delta: share of any accumulated bad debt assigned to this position.
    // pnl_delta_k = |base| × (k_index - k_snapshot) / (a_basis_snapshot × POS_SCALE)
    // Note: k_index DECREASES on bad-debt events, so this term is typically ≤ 0.
    let k_delta_i128 = if a_basis > 0 {
        let diff = (market.k_index)
            .checked_sub(k_snap)
            .ok_or(TorchPerpError::MathOverflow)?;
        // |base| × diff / (a_basis × POS_SCALE) — careful with signs
        let abs_base_i128 = abs_base as i128;
        let num = abs_base_i128
            .checked_mul(diff)
            .ok_or(TorchPerpError::MathOverflow)?;
        let denom_i128 = (a_basis as i128)
            .checked_mul(POS_SCALE as i128)
            .ok_or(TorchPerpError::MathOverflow)?;
        if denom_i128 > 0 {
            num / denom_i128
        } else {
            0
        }
    } else {
        0
    };

    let total_realized = realized_pnl_i128
        .checked_add(k_delta_i128)
        .ok_or(TorchPerpError::MathOverflow)?;

    // Fee on closing notional
    let fee = compute_fee(close_notional, ctx.accounts.global_config.fee_rate_bps)
        .ok_or(TorchPerpError::MathOverflow)?;
    let (to_insurance_fee, to_protocol_fee) =
        split_fee(fee, ctx.accounts.global_config.insurance_fund_cut_bps)
            .ok_or(TorchPerpError::MathOverflow)?;

    // Settle: payout = collateral + total_realized - fee, clamped ≥ 0
    let pre_fee_i128 = (collateral as i128)
        .checked_add(total_realized)
        .ok_or(TorchPerpError::MathOverflow)?;
    let payout_i128 = pre_fee_i128.saturating_sub(fee as i128);
    let user_payout: u64 = if payout_i128 > 0 {
        payout_i128
            .try_into()
            .map_err(|_| TorchPerpError::MathOverflow)?
    } else {
        0
    };

    // Commit vAMM reserves + OI
    market.base_asset_reserve = new_base;
    market.quote_asset_reserve = new_quote;
    if base_i > 0 {
        market.open_interest_long = market.open_interest_long.saturating_sub(abs_base);
    } else {
        market.open_interest_short = market.open_interest_short.saturating_sub(abs_base);
    }

    // ----- SOL transfers -----
    // The position PDA holds: collateral + rent_exempt. After close=user it
    // all returns to user by Anchor's close constraint. But we need the
    // insurance_vault to absorb losses / fund profits in between.

    let position_info = ctx.accounts.position.to_account_info();
    let insurance_info = ctx.accounts.insurance_vault.to_account_info();
    let protocol_info = ctx.accounts.protocol_treasury.to_account_info();

    // Move fee flows to insurance + protocol (from position collateral)
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

    // PnL flow between position PDA and insurance_vault:
    //   PnL > 0: insurance pays the delta to position PDA (which then goes to user on close)
    //   PnL < 0: position PDA pays |delta| to insurance_vault
    if total_realized > 0 {
        let pnl_amount: u64 = total_realized
            .try_into()
            .map_err(|_| TorchPerpError::MathOverflow)?;
        // Draw from insurance — if insufficient, cap to what's available
        let available = market.insurance_balance.min(pnl_amount);
        if available > 0 {
            **insurance_info.try_borrow_mut_lamports()? = insurance_info
                .lamports()
                .checked_sub(available)
                .ok_or(TorchPerpError::MathOverflow)?;
            **position_info.try_borrow_mut_lamports()? = position_info
                .lamports()
                .checked_add(available)
                .ok_or(TorchPerpError::MathOverflow)?;
            market.insurance_balance = market.insurance_balance - available;
        }
        // If insurance was insufficient, the user just doesn't collect the full PnL.
        // This is the v1 no-percolator-on-close behavior; percolator activates
        // primarily via liquidations (liquidate_position).
    } else if total_realized < 0 {
        let loss: u64 = (-total_realized)
            .try_into()
            .map_err(|_| TorchPerpError::MathOverflow)?;
        // Cap to what the position has after fees (if loss > remaining, clamp)
        let remaining = position_info.lamports();
        let to_move = loss.min(remaining);
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

    // Note: Anchor's `close = user` constraint on the PerpPosition account will
    // return all remaining lamports (including rent + any remaining collateral
    // or winnings) to the user on ix completion.
    let _ = user_payout; // retained for future logging; physical flow is via close = user

    // Record TWAP observation
    let current_slot = Clock::get()?.slot;
    record_observation(market, pool_sol, pool_tokens, current_slot)?;

    Ok(())
}
