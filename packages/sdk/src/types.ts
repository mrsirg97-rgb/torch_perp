/**
 * TypeScript types for torch_perp state + responses.
 */

import { PublicKey } from '@solana/web3.js'
import BN from 'bn.js'

// ============================================================================
// On-chain account shapes (decoded from Anchor IDL)
// ============================================================================

export interface GlobalConfig {
  authority: PublicKey
  protocol_treasury: PublicKey
  fee_rate_bps: number
  insurance_fund_cut_bps: number
  bump: number
}

export interface Observation {
  slot: BN
  cumulative_sol: BN
  cumulative_token: BN
}

export interface PerpMarket {
  mint: PublicKey
  spot_pool: PublicKey
  spot_vault_0: PublicKey
  spot_vault_1: PublicKey
  is_wsol_token_0: boolean
  base_asset_reserve: BN
  quote_asset_reserve: BN
  vamm_k_invariant: BN
  initial_margin_ratio_bps: number
  maintenance_margin_ratio_bps: number
  liquidation_penalty_bps: number
  cumulative_funding_long: BN
  cumulative_funding_short: BN
  last_funding_slot: BN
  funding_period_slots: BN
  open_interest_long: BN
  open_interest_short: BN
  twap_observations: Observation[]
  twap_head: number
  insurance_balance: BN
  a_index: BN
  k_index: BN
  recovery_phase: number
  epoch: number
  bump: number
}

export interface PerpPosition {
  user: PublicKey
  market: PublicKey
  base_asset_amount: BN
  quote_asset_collateral: BN
  entry_notional: BN
  last_cumulative_funding: BN
  a_basis_snapshot: BN
  k_snapshot: BN
  matured_pnl: BN
  open_epoch: number
  open_slot: BN
  bump: number
}

// ============================================================================
// SDK response shapes (processed / human-friendly)
// ============================================================================

export type PositionHealth = 'none' | 'healthy' | 'at_risk' | 'liquidatable'
export type RecoveryPhaseName = 'Normal' | 'DrainOnly' | 'ResetPending'
export type Direction = 'long' | 'short'

export interface PerpMarketSummary {
  address: string
  mint: string
  mark_price_sol: number
  spot_pool: string
  base_asset_reserve: string
  quote_asset_reserve: string
  open_interest_long: number
  open_interest_short: number
  insurance_balance_sol: number
  a_index_ratio: number // a_index / POS_SCALE, 0..1
  recovery_phase: RecoveryPhaseName
  epoch: number
  initial_margin_ratio_bps: number
  maintenance_margin_ratio_bps: number
}

export interface PerpPositionInfo {
  address: string
  user: string
  market: string
  direction: Direction | 'flat'
  base_asset_amount: number
  collateral_sol: number
  entry_notional_sol: number
  current_notional_sol: number
  unrealized_pnl_sol: number
  equity_sol: number
  current_ltv_bps: number | null
  health: PositionHealth
  open_slot: number
  open_epoch: number
}

// Quote for opening a position (preview)
export interface OpenQuote {
  direction: Direction
  collateral_lamports: number
  quote_exposure_lamports: number
  est_base_acquired: number // |base_asset_amount| the position would get
  est_entry_notional_lamports: number
  fee_lamports: number
  implied_leverage_x: number
  price_impact_bps: number
  passes_imr_check: boolean
}

// Quote for closing a position (preview)
export interface CloseQuote {
  direction: Direction
  est_payout_lamports: number
  est_realized_pnl_lamports: number
  fee_lamports: number
  price_impact_bps: number
}

// Liquidator view
export interface LiquidationCandidate {
  position: PerpPositionInfo
  expected_bonus_lamports: number
}
