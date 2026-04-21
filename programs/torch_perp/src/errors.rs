use anchor_lang::prelude::*;

#[error_code]
pub enum TorchPerpError {
    #[msg("input amount must be greater than zero")]
    ZeroInput,
    #[msg("math overflow")]
    MathOverflow,
    #[msg("slippage tolerance exceeded")]
    SlippageExceeded,

    // Market state
    #[msg("market is in DrainOnly phase — no new open interest permitted")]
    MarketInDrainOnly,
    #[msg("market is in ResetPending phase — awaiting reset")]
    MarketInResetPending,
    #[msg("market is not yet initialized or spot pool reference is invalid")]
    InvalidMarketConfig,

    // Pool validation
    #[msg("spot pool account does not match market configuration")]
    PoolMismatch,
    #[msg("invalid pool account — expected a Raydium CPMM or DeepPool pool state")]
    InvalidPool,
    #[msg("token mint does not have a torch Treasury — not a torch token")]
    NotTorchToken,
    #[msg("token has not yet migrated — cannot initialize perp market")]
    TokenNotMigrated,

    // Position state
    #[msg("position already exists for this user in this market")]
    PositionAlreadyExists,
    #[msg("no active position for this user in this market")]
    NoActivePosition,
    #[msg("invalid position direction")]
    InvalidPositionDirection,

    // Leverage / margin
    #[msg("leverage exceeds max_leverage_bps for this market")]
    MaxLeverageExceeded,
    #[msg("position below maintenance margin")]
    MaintenanceMarginBreach,
    #[msg("withdrawal would breach maintenance margin")]
    WithdrawalBreachesMargin,
    #[msg("insufficient collateral for requested operation")]
    InsufficientCollateral,

    // Liquidation
    #[msg("position is not liquidatable (above maintenance margin)")]
    PositionNotLiquidatable,

    // Funding / observations
    #[msg("funding update not yet due (last_funding_slot + funding_period_slots > current_slot)")]
    FundingUpdateNotDue,
    #[msg("TWAP observation window is stale — no observations within the window")]
    ObservationStale,

    // Percolator
    #[msg("percolator a_index scaling would breach precision threshold")]
    PercolatorPrecisionBreach,
    #[msg("percolator k_index snapshot is invalid for this position's epoch")]
    EpochMismatch,

    // Misc
    #[msg("unauthorized")]
    Unauthorized,
}
