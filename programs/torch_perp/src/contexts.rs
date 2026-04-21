use anchor_lang::prelude::*;
use anchor_spl::token_interface::Mint;

use crate::constants::*;
use crate::state::{GlobalConfig, PerpMarket, PerpPosition};

// ==============================================================================
// InitializeGlobalConfig — one-time, admin
// ==============================================================================
#[derive(Accounts)]
pub struct InitializeGlobalConfig<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: SystemAccount address stored for future protocol-treasury routing.
    pub protocol_treasury: SystemAccount<'info>,
    #[account(
        init,
        payer = authority,
        space = GlobalConfig::LEN,
        seeds = [GLOBAL_CONFIG_SEED],
        bump,
    )]
    pub global_config: Box<Account<'info, GlobalConfig>>,
    pub system_program: Program<'info, System>,
}

// ==============================================================================
// InitializeMarket — permissionless init for any mint
// ==============================================================================
// v1: intentionally does NOT cross-validate torch_market's Treasury PDA, to keep
// torch_perp decoupled from torch_market's program ID. Rent + insurance-seed cost
// deters spam. Tightening to torch-token-only is a v2 option.
#[derive(Accounts)]
pub struct InitializeMarket<'info> {
    #[account(mut)]
    pub initializer: Signer<'info>,

    pub mint: Box<InterfaceAccount<'info, Mint>>,

    /// CHECK: spot pool state account (Raydium CPMM or DeepPool). Validated
    /// structurally in handler: owner must be a supported pool program + vault
    /// pubkeys must match pool's stored vaults.
    pub spot_pool: UncheckedAccount<'info>,
    /// CHECK: pool vault 0 (validated in handler against spot_pool's layout)
    pub spot_vault_0: UncheckedAccount<'info>,
    /// CHECK: pool vault 1 (validated in handler)
    pub spot_vault_1: UncheckedAccount<'info>,

    #[account(
        init,
        payer = initializer,
        space = PerpMarket::LEN,
        seeds = [PERP_MARKET_SEED, mint.key().as_ref()],
        bump,
    )]
    pub market: Box<Account<'info, PerpMarket>>,

    /// CHECK: PDA that holds the insurance fund's SOL balance.
    /// System-owned SystemAccount — seeded empty at init, funded by fee share on trades.
    #[account(
        mut,
        seeds = [INSURANCE_VAULT_SEED, mint.key().as_ref()],
        bump,
    )]
    pub insurance_vault: SystemAccount<'info>,

    pub system_program: Program<'info, System>,
}

// ==============================================================================
// OpenPosition — user opens a new leveraged long or short
// ==============================================================================
#[derive(Accounts)]
pub struct OpenPosition<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [PERP_MARKET_SEED, market.mint.as_ref()],
        bump = market.bump,
    )]
    pub market: Box<Account<'info, PerpMarket>>,

    /// CHECK: live-read for spot reference + observation write.
    /// Validated against market.spot_pool in handler.
    pub spot_pool: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_0
    pub spot_vault_0: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_1
    pub spot_vault_1: UncheckedAccount<'info>,

    // Position is (user, market)-keyed. init_if_needed allows reopening after close.
    #[account(
        init_if_needed,
        payer = user,
        space = PerpPosition::LEN,
        seeds = [PERP_POSITION_SEED, market.key().as_ref(), user.key().as_ref()],
        bump,
    )]
    pub position: Box<Account<'info, PerpPosition>>,

    pub global_config: Box<Account<'info, GlobalConfig>>,

    /// CHECK: validated against global_config.protocol_treasury in handler
    #[account(mut)]
    pub protocol_treasury: SystemAccount<'info>,

    /// CHECK: market's insurance fund vault (mint-scoped PDA)
    #[account(
        mut,
        seeds = [INSURANCE_VAULT_SEED, market.mint.as_ref()],
        bump,
    )]
    pub insurance_vault: SystemAccount<'info>,

    pub system_program: Program<'info, System>,
}

// ==============================================================================
// ClosePosition — user closes their position, realizes PnL
// ==============================================================================
#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [PERP_MARKET_SEED, market.mint.as_ref()],
        bump = market.bump,
    )]
    pub market: Box<Account<'info, PerpMarket>>,

    /// CHECK: validated against market.spot_pool
    pub spot_pool: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_0
    pub spot_vault_0: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_1
    pub spot_vault_1: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [PERP_POSITION_SEED, market.key().as_ref(), user.key().as_ref()],
        bump = position.bump,
        has_one = user,
        has_one = market,
        close = user,
    )]
    pub position: Box<Account<'info, PerpPosition>>,

    pub global_config: Box<Account<'info, GlobalConfig>>,

    /// CHECK: validated against global_config.protocol_treasury
    #[account(mut)]
    pub protocol_treasury: SystemAccount<'info>,

    /// CHECK: market's insurance fund vault
    #[account(
        mut,
        seeds = [INSURANCE_VAULT_SEED, market.mint.as_ref()],
        bump,
    )]
    pub insurance_vault: SystemAccount<'info>,

    pub system_program: Program<'info, System>,
}

// ==============================================================================
// DepositCollateral — top up margin on an existing position
// ==============================================================================
#[derive(Accounts)]
pub struct DepositCollateral<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        seeds = [PERP_MARKET_SEED, market.mint.as_ref()],
        bump = market.bump,
    )]
    pub market: Box<Account<'info, PerpMarket>>,

    #[account(
        mut,
        seeds = [PERP_POSITION_SEED, market.key().as_ref(), user.key().as_ref()],
        bump = position.bump,
        has_one = user,
        has_one = market,
    )]
    pub position: Box<Account<'info, PerpPosition>>,

    pub system_program: Program<'info, System>,
}

// ==============================================================================
// WithdrawCollateral — reduce margin; must remain above maintenance
// ==============================================================================
#[derive(Accounts)]
pub struct WithdrawCollateral<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        seeds = [PERP_MARKET_SEED, market.mint.as_ref()],
        bump = market.bump,
    )]
    pub market: Box<Account<'info, PerpMarket>>,

    #[account(
        mut,
        seeds = [PERP_POSITION_SEED, market.key().as_ref(), user.key().as_ref()],
        bump = position.bump,
        has_one = user,
        has_one = market,
    )]
    pub position: Box<Account<'info, PerpPosition>>,

    /// CHECK: validated against market.spot_pool (need live read for margin check)
    pub spot_pool: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_0
    pub spot_vault_0: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_1
    pub spot_vault_1: UncheckedAccount<'info>,
}

// ==============================================================================
// LiquidatePosition — permissionless when position below maintenance margin
// ==============================================================================
#[derive(Accounts)]
pub struct LiquidatePosition<'info> {
    #[account(mut)]
    pub liquidator: Signer<'info>,

    #[account(
        mut,
        seeds = [PERP_MARKET_SEED, market.mint.as_ref()],
        bump = market.bump,
    )]
    pub market: Box<Account<'info, PerpMarket>>,

    /// CHECK: validated against market.spot_pool
    pub spot_pool: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_0
    pub spot_vault_0: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_1
    pub spot_vault_1: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [PERP_POSITION_SEED, market.key().as_ref(), position_owner.key().as_ref()],
        bump = position.bump,
        has_one = market,
        constraint = position.user == position_owner.key(),
        close = position_owner,
    )]
    pub position: Box<Account<'info, PerpPosition>>,

    /// CHECK: the position's owner, receives any residual collateral
    #[account(mut)]
    pub position_owner: SystemAccount<'info>,

    /// CHECK: market's insurance vault — draws first on shortfalls
    #[account(
        mut,
        seeds = [INSURANCE_VAULT_SEED, market.mint.as_ref()],
        bump,
    )]
    pub insurance_vault: SystemAccount<'info>,

    pub system_program: Program<'info, System>,
}

// ==============================================================================
// UpdateFunding — permissionless crank, updates cumulative funding
// ==============================================================================
#[derive(Accounts)]
pub struct UpdateFunding<'info> {
    #[account(
        mut,
        seeds = [PERP_MARKET_SEED, market.mint.as_ref()],
        bump = market.bump,
    )]
    pub market: Box<Account<'info, PerpMarket>>,

    /// CHECK: validated against market.spot_pool for TWAP read
    pub spot_pool: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_0
    pub spot_vault_0: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_1
    pub spot_vault_1: UncheckedAccount<'info>,
}

// ==============================================================================
// WriteObservation — permissionless crank, appends TWAP observation
// ==============================================================================
// Also called internally by any ix that touches the pool. Exposed as a
// permissionless external ix to ensure observations stay fresh during
// low-trade-activity periods.
#[derive(Accounts)]
pub struct WriteObservation<'info> {
    #[account(
        mut,
        seeds = [PERP_MARKET_SEED, market.mint.as_ref()],
        bump = market.bump,
    )]
    pub market: Box<Account<'info, PerpMarket>>,

    /// CHECK: validated against market.spot_pool
    pub spot_pool: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_0
    pub spot_vault_0: UncheckedAccount<'info>,
    /// CHECK: validated against market.spot_vault_1
    pub spot_vault_1: UncheckedAccount<'info>,
}
