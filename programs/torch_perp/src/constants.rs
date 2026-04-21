// PDA seeds
pub const GLOBAL_CONFIG_SEED: &[u8] = b"global_config";
pub const PERP_MARKET_SEED: &[u8] = b"perp_market";
pub const PERP_POSITION_SEED: &[u8] = b"perp_position";
pub const INSURANCE_VAULT_SEED: &[u8] = b"insurance_vault";

// Basis points denominator (10_000 = 100%)
pub const BPS_DENOMINATOR: u64 = 10_000;

// ===== Trading parameters =====
// Initial margin ratio: collateral/notional floor to OPEN a position.
// 1000 bps = 10% → max 10x leverage.
pub const INITIAL_MARGIN_RATIO_BPS: u16 = 1_000;
// Maintenance margin ratio: liquidated when equity/notional falls below this.
// 625 bps = 6.25% → liquidation at ~16x implied leverage.
pub const MAINTENANCE_MARGIN_RATIO_BPS: u16 = 625;
// Liquidator bonus: 5% of notional
pub const LIQUIDATION_PENALTY_BPS: u16 = 500;

// ===== Fees =====
// Taker fee on open/close: 10 bps of notional
pub const FEE_RATE_BPS: u16 = 10;
// Fraction of fees directed to insurance fund (rest goes to protocol treasury)
pub const INSURANCE_FUND_CUT_BPS: u16 = 5_000; // 50%

// ===== Funding rate =====
// Funding period: ~1 hour at 400ms/slot
pub const FUNDING_PERIOD_SLOTS: u64 = 9_000;

// ===== TWAP =====
// Observation ring buffer size — 32 observations
pub const TWAP_RING_SIZE: usize = 32;
// Target window for TWAP (~10 minutes at 400ms/slot)
pub const TWAP_WINDOW_SLOTS: u64 = 1_500;

// ===== Percolator solvency layer =====
// Fixed-point precision for A/K indices (1e18)
pub const POS_SCALE: u128 = 1_000_000_000_000_000_000;
// Precision threshold: when a_index drops below this, market enters DrainOnly
// POS_SCALE / 1000 = 0.1% of full precision
pub const PRECISION_THRESHOLD: u128 = POS_SCALE / 1_000;
// Matured PnL warmup: realized PnL sits as "reserve" for this many slots
// before becoming matured and haircut-eligible. Prevents flash-attack on haircut math.
// 256 slots ≈ 100 seconds at 400ms/slot.
pub const MATURED_WARMUP_SLOTS: u64 = 256;

// ===== Recovery phases =====
pub const RECOVERY_NORMAL: u8 = 0;
pub const RECOVERY_DRAIN_ONLY: u8 = 1;
pub const RECOVERY_RESET_PENDING: u8 = 2;
