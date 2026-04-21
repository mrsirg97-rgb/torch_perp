// torch_perp — leveraged perpetual futures on torch tokens.
// Oracle-free mark via vAMM, TWAP from spot pool for funding, percolator-style
// solvency layer for bad debt beyond the insurance fund.
//
// See docs/design.md for architecture + rationale.

use anchor_lang::prelude::*;

pub mod constants;
pub mod contexts;
pub mod errors;
pub mod handlers;
pub mod math;
pub mod pool;
pub mod state;

#[cfg(kani)]
mod kani_proofs;

use contexts::*;

declare_id!("852yvbSWFCyVLRo8bWUPTiouM5amtw6JxctgS9P4ymdH");

#[program]
pub mod torch_perp {
    use super::*;

    // One-time admin init. Sets protocol fee routing.
    pub fn initialize_global_config(
        ctx: Context<InitializeGlobalConfig>,
        fee_rate_bps: u16,
        insurance_fund_cut_bps: u16,
    ) -> Result<()> {
        handlers::initialize_global_config::handler(ctx, fee_rate_bps, insurance_fund_cut_bps)
    }

    // Permissionless market init for any migrated token. Seeds vAMM from spot pool.
    // Caller picks SOL-side depth (vamm_quote_reserve); base-side is derived
    // deterministically to match spot price at init.
    pub fn initialize_market(
        ctx: Context<InitializeMarket>,
        initial_margin_ratio_bps: u16,
        maintenance_margin_ratio_bps: u16,
        liquidation_penalty_bps: u16,
        funding_period_slots: u64,
        vamm_quote_reserve: u128,
    ) -> Result<()> {
        handlers::initialize_market::handler(
            ctx,
            initial_margin_ratio_bps,
            maintenance_margin_ratio_bps,
            liquidation_penalty_bps,
            funding_period_slots,
            vamm_quote_reserve,
        )
    }

    // Open a new leveraged long or short. base_amount sign determines direction.
    pub fn open_position(
        ctx: Context<OpenPosition>,
        base_amount: i64,
        collateral_lamports: u64,
        max_price_impact_bps: u16,
    ) -> Result<()> {
        handlers::open_position::handler(ctx, base_amount, collateral_lamports, max_price_impact_bps)
    }

    // Close full position, settle funding and PnL, return collateral ± PnL.
    pub fn close_position(
        ctx: Context<ClosePosition>,
        min_quote_out: u64,
    ) -> Result<()> {
        handlers::close_position::handler(ctx, min_quote_out)
    }

    // Add SOL collateral to an existing position.
    pub fn deposit_collateral(
        ctx: Context<DepositCollateral>,
        amount: u64,
    ) -> Result<()> {
        handlers::deposit_collateral::handler(ctx, amount)
    }

    // Remove collateral. Position must remain above initial margin post-op.
    pub fn withdraw_collateral(
        ctx: Context<WithdrawCollateral>,
        amount: u64,
    ) -> Result<()> {
        handlers::withdraw_collateral::handler(ctx, amount)
    }

    // Permissionless liquidation when position falls below maintenance margin.
    pub fn liquidate_position(ctx: Context<LiquidatePosition>) -> Result<()> {
        handlers::liquidate_position::handler(ctx)
    }

    // Permissionless funding update — v1: TWAP observation write only.
    // v1.1 will compute premium from mark vs TWAP and roll into cumulative_funding_*.
    pub fn update_funding(ctx: Context<UpdateFunding>) -> Result<()> {
        handlers::update_funding::handler(ctx)
    }

    // Permissionless TWAP observation write. Also called internally on any
    // pool-touching ix; exposed externally for low-activity markets.
    pub fn write_observation(ctx: Context<WriteObservation>) -> Result<()> {
        handlers::write_observation::handler(ctx)
    }
}
