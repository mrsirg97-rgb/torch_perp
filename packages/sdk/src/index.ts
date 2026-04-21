/**
 * torchperpsdk — TypeScript SDK for torch_perp.
 *
 * Leveraged perpetual futures on torch tokens. Oracle-free mark via vAMM,
 * percolator solvency layer, integer-math parity with on-chain Rust.
 */

// Constants
export {
  TORCH_PERP_PROGRAM_ID,
  LAMPORTS_PER_SOL,
  BPS_DENOMINATOR,
  INITIAL_MARGIN_RATIO_BPS,
  MAINTENANCE_MARGIN_RATIO_BPS,
  LIQUIDATION_PENALTY_BPS,
  FEE_RATE_BPS,
  INSURANCE_FUND_CUT_BPS,
  FUNDING_PERIOD_SLOTS,
  TWAP_RING_SIZE,
  TWAP_WINDOW_SLOTS,
  POS_SCALE,
  PRECISION_THRESHOLD,
  RECOVERY_NORMAL,
  RECOVERY_DRAIN_ONLY,
  RECOVERY_RESET_PENDING,
  RAYDIUM_CPMM_PROGRAM_ID,
  WSOL_MINT,
} from './constants'

// PDAs
export {
  getGlobalConfigPda,
  getPerpMarketPda,
  getInsuranceVaultPda,
  getPerpPositionPda,
} from './pda'

// State decoders
export { decodeGlobalConfig, decodePerpMarket, decodePerpPosition } from './state'

// Queries
export {
  getGlobalConfig,
  getPerpMarket,
  getPerpPosition,
  summarizeMarket,
  computePositionInfo,
  listPerpMarkets,
  listPositionsForMarket,
} from './queries'

// Math (pure client-side)
export {
  vamm_buy_base,
  vamm_sell_base,
  compute_fee,
  split_fee,
  position_notional,
  unrealized_pnl,
  required_margin,
  check_initial_margin,
  is_above_maintenance,
  compute_price_impact_bps,
} from './math'

// Quote helpers (previews + liquidation candidates)
export {
  computeOpenQuote,
  getOpenQuote,
  computeCloseQuote,
  getCloseQuote,
  getLiquidationCandidates,
} from './quotes'
export type { ComputeOpenQuoteInput } from './quotes'

// Transaction builders
export {
  buildInitializeGlobalConfigInstruction,
  buildInitializeMarketInstruction,
  buildOpenPositionInstruction,
  buildClosePositionInstruction,
  buildDepositCollateralInstruction,
  buildWithdrawCollateralInstruction,
  buildLiquidatePositionInstruction,
  buildUpdateFundingInstruction,
  buildWriteObservationInstruction,
} from './transactions'

// Types
export type {
  GlobalConfig,
  PerpMarket,
  PerpPosition,
  Observation,
  PositionHealth,
  RecoveryPhaseName,
  Direction,
  PerpMarketSummary,
  PerpPositionInfo,
  OpenQuote,
  CloseQuote,
  LiquidationCandidate,
} from './types'

export type {
  InitializeGlobalConfigParams,
  InitializeMarketParams,
  OpenPositionParams,
  ClosePositionParams,
  CollateralParams,
  WithdrawCollateralParams,
  LiquidatePositionParams,
  MarketCrankParams,
  BuildResult,
} from './transactions'

export type { SwapResult } from './math'

// Program helpers
export { IDL, getProgram, getCoder } from './program'
