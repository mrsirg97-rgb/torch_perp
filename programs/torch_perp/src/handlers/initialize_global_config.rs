use anchor_lang::prelude::*;

use crate::constants::BPS_DENOMINATOR;
use crate::contexts::InitializeGlobalConfig;
use crate::errors::TorchPerpError;

// One-time admin init. Records fee rate + insurance cut + admin + treasury
// recipient. These are immutable after init (torch-style immutability).
//
// fee_rate_bps: taker fee on open/close, bounded to [0, 200] (0% to 2%).
// insurance_fund_cut_bps: portion of fees → insurance, bounded to [0, 10000].
pub fn handler(
    ctx: Context<InitializeGlobalConfig>,
    fee_rate_bps: u16,
    insurance_fund_cut_bps: u16,
) -> Result<()> {
    // Fee rate sanity bounds. Hard cap at 2% — anything higher is almost
    // certainly a misconfiguration. Protects against fat-finger at init.
    require!(
        fee_rate_bps <= 200,
        TorchPerpError::InvalidMarketConfig
    );
    require!(
        (insurance_fund_cut_bps as u64) <= BPS_DENOMINATOR,
        TorchPerpError::InvalidMarketConfig
    );

    let config = &mut ctx.accounts.global_config;
    config.authority = ctx.accounts.authority.key();
    config.protocol_treasury = ctx.accounts.protocol_treasury.key();
    config.fee_rate_bps = fee_rate_bps;
    config.insurance_fund_cut_bps = insurance_fund_cut_bps;
    config.bump = ctx.bumps.global_config;

    Ok(())
}
