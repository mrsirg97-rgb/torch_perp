use anchor_lang::prelude::*;
use anchor_lang::system_program;

use crate::contexts::DepositCollateral;
use crate::errors::TorchPerpError;

// Add SOL collateral to an existing position. The position PDA holds its
// collateral as native lamports (same pattern as torch's TorchVault); the
// `quote_asset_collateral` field is the ledger mirror of `position.lamports()`
// minus the rent-exempt reserve.
//
// Adding collateral is always safe — no margin or leverage check required.
// The only guard: position must exist + belong to the signer (enforced by
// Anchor constraints in DepositCollateral).
pub fn handler(ctx: Context<DepositCollateral>, amount: u64) -> Result<()> {
    require!(amount > 0, TorchPerpError::ZeroInput);

    // Transfer SOL from user → position PDA. System program credits lamports;
    // torch_perp remains the data-owner so position state is protected.
    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            system_program::Transfer {
                from: ctx.accounts.user.to_account_info(),
                to: ctx.accounts.position.to_account_info(),
            },
        ),
        amount,
    )?;

    // Update ledger. Overflow-safe.
    let position = &mut ctx.accounts.position;
    position.quote_asset_collateral = position
        .quote_asset_collateral
        .checked_add(amount)
        .ok_or(TorchPerpError::MathOverflow)?;

    Ok(())
}
