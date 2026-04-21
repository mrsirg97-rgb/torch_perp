use anchor_lang::prelude::*;

use crate::constants::TWAP_RING_SIZE;

// ==============================================================================
// GlobalConfig — one per program
// ==============================================================================
// Holds protocol-level parameters set by an admin at init.
// Fee rate and insurance cut are immutable after init (torch-style immutability).
#[account]
pub struct GlobalConfig {
    pub authority: Pubkey,           // market-init authority (for v1; becomes permissionless later)
    pub protocol_treasury: Pubkey,   // SOL vault for non-insurance fee share
    pub fee_rate_bps: u16,           // taker fee on open/close
    pub insurance_fund_cut_bps: u16, // portion of fees routed to market's insurance
    pub bump: u8,
}

impl GlobalConfig {
    pub const LEN: usize = 8   // discriminator
        + 32  // authority
        + 32  // protocol_treasury
        + 2   // fee_rate_bps
        + 2   // insurance_fund_cut_bps
        + 1;  // bump
}

// ==============================================================================
// Observation — TWAP ring buffer entry (inline inside PerpMarket)
// ==============================================================================
// Stores cumulative price observations for computing TWAP.
// cumulative_sol and cumulative_token are running sums of (reserve × slot_delta).
// TWAP over [t0, t1] = (cumulative[t1] - cumulative[t0]) / (slot[t1] - slot[t0]).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default)]
pub struct Observation {
    pub slot: u64,
    pub cumulative_sol: u128,
    pub cumulative_token: u128,
}

impl Observation {
    pub const LEN: usize = 8 + 16 + 16; // 40 bytes
}

// ==============================================================================
// PerpMarket — one per torch token
// ==============================================================================
// Encapsulates the vAMM, funding state, TWAP ring, insurance fund, and
// percolator solvency indices for a single torch token's perp market.
#[account]
pub struct PerpMarket {
    // Token + spot pool references
    pub mint: Pubkey,               // torch token mint
    pub spot_pool: Pubkey,          // Raydium or DeepPool pool state
    pub spot_vault_0: Pubkey,       // pool's token_0 vault
    pub spot_vault_1: Pubkey,       // pool's token_1 vault
    pub is_wsol_token_0: bool,      // cached pool orientation

    // vAMM reserves (base = torch token, quote = SOL lamports)
    pub base_asset_reserve: u128,
    pub quote_asset_reserve: u128,
    pub vamm_k_invariant: u128,     // base × quote, immutable per market

    // Risk / margin parameters (immutable per market)
    // initial_margin_ratio_bps: min collateral/notional at open. 1000 bps = 10x max leverage.
    pub initial_margin_ratio_bps: u16,
    // maintenance_margin_ratio_bps: min equity/notional before liquidation.
    pub maintenance_margin_ratio_bps: u16,
    pub liquidation_penalty_bps: u16,

    // Funding rate state
    pub cumulative_funding_long: i128,  // paid by longs (negative) / received by longs (positive)
    pub cumulative_funding_short: i128,
    pub last_funding_slot: u64,
    pub funding_period_slots: u64,

    // Open interest (base asset amounts, always ≥ 0)
    pub open_interest_long: u64,
    pub open_interest_short: u64,

    // TWAP ring buffer
    pub twap_observations: [Observation; TWAP_RING_SIZE],
    pub twap_head: u16,             // index of next write position

    // Insurance fund balance (lamports) — first line of defense on bad debt
    pub insurance_balance: u64,

    // Percolator solvency layer
    pub a_index: u128,              // starts at POS_SCALE; decreases on A-scaling events
    pub k_index: i128,              // cumulative residual PnL accumulator
    pub recovery_phase: u8,         // Normal / DrainOnly / ResetPending (see constants)
    pub epoch: u32,                 // increments on ResetPending → Normal transition

    pub bump: u8,
}

impl PerpMarket {
    pub const LEN: usize = 8        // discriminator
        + 32                        // mint
        + 32                        // spot_pool
        + 32                        // spot_vault_0
        + 32                        // spot_vault_1
        + 1                         // is_wsol_token_0
        + 16                        // base_asset_reserve
        + 16                        // quote_asset_reserve
        + 16                        // vamm_k_invariant
        + 2                         // max_leverage_bps
        + 2                         // maintenance_margin_bps
        + 2                         // liquidation_penalty_bps
        + 16                        // cumulative_funding_long
        + 16                        // cumulative_funding_short
        + 8                         // last_funding_slot
        + 8                         // funding_period_slots
        + 8                         // open_interest_long
        + 8                         // open_interest_short
        + (Observation::LEN * TWAP_RING_SIZE) // twap_observations
        + 2                         // twap_head
        + 8                         // insurance_balance
        + 16                        // a_index
        + 16                        // k_index
        + 1                         // recovery_phase
        + 4                         // epoch
        + 1;                        // bump
}

// ==============================================================================
// PerpPosition — one per user per market
// ==============================================================================
// A user's isolated leveraged exposure. `base_asset_amount` is signed:
// positive = long, negative = short. Never zero for an active position.
#[account]
pub struct PerpPosition {
    pub user: Pubkey,
    pub market: Pubkey,

    // Position size (signed: + long / - short). In percolator terms: basis_i.
    pub base_asset_amount: i64,
    // SOL collateral held for this position (lamports). In percolator terms: C_i.
    pub quote_asset_collateral: u64,
    // |base_asset_amount| × entry_price, captured at open (for PnL calculation)
    pub entry_notional: u64,

    // Funding snapshot at last interaction (for delta calc)
    pub last_cumulative_funding: i128,

    // Percolator snapshots (captured at entry / any state-changing op)
    pub a_basis_snapshot: u128,     // market.a_index at the time basis was set
    pub k_snapshot: i128,           // market.k_index at the time basis was set

    // Realized PnL sitting as reserve (R_i in percolator). After MATURED_WARMUP_SLOTS
    // passes since open_slot, this becomes "matured" and is subject to H haircut
    // but senior to unrealized PnL.
    pub matured_pnl: i64,

    // Epoch this position was opened in. A position opened in an older epoch
    // is stale (market reset) and must be closed/reset separately.
    pub open_epoch: u32,
    pub open_slot: u64,

    pub bump: u8,
}

impl PerpPosition {
    pub const LEN: usize = 8   // discriminator
        + 32  // user
        + 32  // market
        + 8   // base_asset_amount (i64)
        + 8   // quote_asset_collateral
        + 8   // entry_notional
        + 16  // last_cumulative_funding
        + 16  // a_basis_snapshot
        + 16  // k_snapshot
        + 8   // matured_pnl (i64)
        + 4   // open_epoch
        + 8   // open_slot
        + 1;  // bump
}
