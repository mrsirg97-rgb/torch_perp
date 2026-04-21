use anchor_lang::prelude::*;

use crate::contexts::WithdrawCollateral;
use crate::errors::TorchPerpError;
use crate::math::{
    check_initial_margin, position_notional, unrealized_pnl,
};
use crate::pool::verify_and_read_reserves;

// Remove SOL collateral from an existing position. Post-withdrawal equity must
// still satisfy the initial margin ratio — i.e., the position can't be left
// under-collateralized by the withdrawal. This is STRICTER than maintenance
// margin because we don't want withdraws to leave a position in an
// "already-liquidatable" state.
pub fn handler(ctx: Context<WithdrawCollateral>, amount: u64) -> Result<()> {
    require!(amount > 0, TorchPerpError::ZeroInput);

    // Pool matches market's stored refs (we'll use it for liveness sanity but
    // margin uses the vAMM mark for this handler).
    let market = &ctx.accounts.market;
    require_keys_eq!(
        ctx.accounts.spot_pool.key(),
        market.spot_pool,
        TorchPerpError::PoolMismatch
    );
    let _ = verify_and_read_reserves(
        &ctx.accounts.spot_pool,
        &ctx.accounts.spot_vault_0,
        &ctx.accounts.spot_vault_1,
        &market.spot_pool,
        &market.spot_vault_0,
        &market.spot_vault_1,
        market.is_wsol_token_0,
    )?;

    let position = &mut ctx.accounts.position;
    require!(
        amount <= position.quote_asset_collateral,
        TorchPerpError::InsufficientCollateral
    );

    // Compute equity after the hypothetical withdrawal.
    let abs_base = position.base_asset_amount.unsigned_abs();
    let current_notional = position_notional(
        abs_base,
        market.base_asset_reserve,
        market.quote_asset_reserve,
    )
    .ok_or(TorchPerpError::MathOverflow)?;
    let upnl = unrealized_pnl(
        position.base_asset_amount,
        position.entry_notional,
        current_notional,
    )
    .ok_or(TorchPerpError::MathOverflow)?;

    let new_collateral = position.quote_asset_collateral - amount;
    // equity = new_collateral + upnl (i128 to handle negative upnl)
    let equity_i128 = (new_collateral as i128).checked_add(upnl as i128)
        .ok_or(TorchPerpError::MathOverflow)?;
    require!(equity_i128 >= 0, TorchPerpError::WithdrawalBreachesMargin);
    let equity_u64: u64 = equity_i128
        .try_into()
        .map_err(|_| TorchPerpError::MathOverflow)?;

    // Enforce initial margin (stricter than maintenance — can't leave in
    // already-at-risk state via withdrawal).
    let passes = check_initial_margin(current_notional, equity_u64, market.initial_margin_ratio_bps)
        .ok_or(TorchPerpError::MathOverflow)?;
    require!(passes, TorchPerpError::WithdrawalBreachesMargin);

    // Move SOL from position PDA → user. Both accounts are program/system owned
    // in terms of lamports; we manipulate directly.
    let position_info = position.to_account_info();
    let user_info = ctx.accounts.user.to_account_info();
    **position_info.try_borrow_mut_lamports()? = position_info
        .lamports()
        .checked_sub(amount)
        .ok_or(TorchPerpError::MathOverflow)?;
    **user_info.try_borrow_mut_lamports()? = user_info
        .lamports()
        .checked_add(amount)
        .ok_or(TorchPerpError::MathOverflow)?;

    position.quote_asset_collateral = new_collateral;
    Ok(())
}
