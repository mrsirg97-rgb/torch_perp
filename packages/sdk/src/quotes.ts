/**
 * Quote helpers — compute open/close previews + liquidation candidates.
 *
 * Every function comes in two flavors:
 *   - sync `computeXxx(market, ...)` — for callers that already hold market state
 *   - async `getXxx(connection, mint, ...)` — fetches market + computes
 *
 * All outputs are snapshots of the math at the current vAMM state. Actual
 * execution may differ slightly due to: (a) concurrent swaps changing reserves
 * between quote and execution, (b) percolator A-scaling if the position's epoch
 * is older than market.epoch.
 */

import { Connection, PublicKey } from '@solana/web3.js'

import { BPS_DENOMINATOR, LAMPORTS_PER_SOL } from './constants'
import {
  check_initial_margin,
  compute_fee,
  compute_price_impact_bps,
  position_notional,
  required_margin,
  unrealized_pnl,
  vamm_buy_base,
  vamm_sell_base,
} from './math'
import { computePositionInfo, getPerpMarket, listPositionsForMarket } from './queries'
import {
  CloseQuote,
  Direction,
  LiquidationCandidate,
  OpenQuote,
  PerpMarket,
  PerpPosition,
} from './types'

// ============================================================================
// Open quote
// ============================================================================

export interface ComputeOpenQuoteInput {
  direction: Direction
  collateral_lamports: bigint
  // Specify EITHER leverage_x OR quote_exposure_lamports. leverage_x is more
  // common (UIs show "5x long"). quote_exposure is used when caller has a
  // specific notional target (e.g., "open 10 SOL of exposure").
  leverage_x?: number
  quote_exposure_lamports?: bigint
  fee_rate_bps?: number // defaults to the market's IMR — pass if non-default
}

export const computeOpenQuote = (
  market: PerpMarket,
  input: ComputeOpenQuoteInput,
  fee_rate_bps: number,
): OpenQuote => {
  const collateral = input.collateral_lamports

  let quote_exposure: bigint
  if (input.quote_exposure_lamports !== undefined) {
    quote_exposure = input.quote_exposure_lamports
  } else if (input.leverage_x !== undefined) {
    quote_exposure = collateral * BigInt(Math.floor(input.leverage_x * 1_000)) / 1_000n
  } else {
    throw new Error('provide either leverage_x or quote_exposure_lamports')
  }

  const base_r = BigInt(market.base_asset_reserve.toString())
  const quote_r = BigInt(market.quote_asset_reserve.toString())

  const fee = compute_fee(quote_exposure, fee_rate_bps)
  const passes_imr = check_initial_margin(
    quote_exposure,
    collateral,
    market.initial_margin_ratio_bps,
  )

  // Simulate the vAMM swap
  let est_base_acquired = 0n
  let new_base = base_r
  let new_quote = quote_r

  if (input.direction === 'long') {
    const result = vamm_buy_base(quote_exposure, base_r, quote_r)
    if (result) {
      est_base_acquired = result.output
      new_base = result.new_base_reserve
      new_quote = result.new_quote_reserve
    }
  } else {
    // Short: compute the base amount to sell that produces `quote_exposure` as output
    // approx: base_to_sell ≈ quote_exposure × base_r / (quote_r - quote_exposure)
    if (quote_r > quote_exposure) {
      const base_to_sell = (quote_exposure * base_r) / (quote_r - quote_exposure) + 1n
      const result = vamm_sell_base(base_to_sell, base_r, quote_r)
      if (result) {
        est_base_acquired = base_to_sell
        new_base = result.new_base_reserve
        new_quote = result.new_quote_reserve
      }
    }
  }

  const price_impact_bps = compute_price_impact_bps(base_r, quote_r, new_base, new_quote)
  const implied_leverage_x = collateral > 0n ? Number(quote_exposure) / Number(collateral) : 0

  return {
    direction: input.direction,
    collateral_lamports: Number(collateral),
    quote_exposure_lamports: Number(quote_exposure),
    est_base_acquired: Number(est_base_acquired),
    est_entry_notional_lamports: Number(quote_exposure),
    fee_lamports: Number(fee),
    implied_leverage_x,
    price_impact_bps: input.direction === 'long' ? price_impact_bps : -price_impact_bps,
    passes_imr_check: passes_imr,
  }
}

export const getOpenQuote = async (
  connection: Connection,
  mint: PublicKey | string,
  input: ComputeOpenQuoteInput,
): Promise<OpenQuote> => {
  const market = await getPerpMarket(connection, mint)
  if (!market) throw new Error(`market not found for mint ${mint}`)
  // Fee rate: use input override or fall back to a default (callers should pass
  // the real fee_rate_bps loaded from GlobalConfig for accurate quotes).
  const fee_rate_bps = input.fee_rate_bps ?? 10 // 10 bps default
  return computeOpenQuote(market, input, fee_rate_bps)
}

// ============================================================================
// Close quote
// ============================================================================

export const computeCloseQuote = (
  market: PerpMarket,
  position: PerpPosition,
  fee_rate_bps: number,
): CloseQuote => {
  const base_i = BigInt(position.base_asset_amount.toString())
  const abs_base = base_i < 0n ? -base_i : base_i
  const base_r = BigInt(market.base_asset_reserve.toString())
  const quote_r = BigInt(market.quote_asset_reserve.toString())
  const entry = BigInt(position.entry_notional.toString())
  const collateral = BigInt(position.quote_asset_collateral.toString())

  const direction: Direction = base_i > 0n ? 'long' : 'short'

  let est_payout = 0n
  let realized_pnl = 0n
  let new_base = base_r
  let new_quote = quote_r

  if (base_i > 0n) {
    // Close long: sell base → receive quote
    const result = vamm_sell_base(abs_base, base_r, quote_r)
    if (result) {
      const quote_received = result.output
      realized_pnl = quote_received - entry
      new_base = result.new_base_reserve
      new_quote = result.new_quote_reserve
    }
  } else if (base_i < 0n) {
    // Close short: buy back base with quote
    if (base_r > abs_base) {
      const quote_cost = (abs_base * quote_r) / (base_r - abs_base) + 1n
      const result = vamm_buy_base(quote_cost, base_r, quote_r)
      if (result) {
        realized_pnl = entry - quote_cost
        new_base = result.new_base_reserve
        new_quote = result.new_quote_reserve
      }
    }
  }

  const fee = compute_fee(entry < 0n ? -entry : entry, fee_rate_bps)
  est_payout = collateral + realized_pnl - fee
  if (est_payout < 0n) est_payout = 0n

  const price_impact_bps = compute_price_impact_bps(base_r, quote_r, new_base, new_quote)

  return {
    direction,
    est_payout_lamports: Number(est_payout),
    est_realized_pnl_lamports: Number(realized_pnl),
    fee_lamports: Number(fee),
    price_impact_bps,
  }
}

export const getCloseQuote = async (
  connection: Connection,
  mint: PublicKey | string,
  position: PerpPosition,
  fee_rate_bps?: number,
): Promise<CloseQuote> => {
  const market = await getPerpMarket(connection, mint)
  if (!market) throw new Error(`market not found for mint ${mint}`)
  return computeCloseQuote(market, position, fee_rate_bps ?? 10)
}

// ============================================================================
// Liquidation candidate scanner
// ============================================================================

// Scans all positions in a market and returns those below maintenance margin,
// sorted by expected liquidator bonus (highest first). Used by liquidation bots.
export const getLiquidationCandidates = async (
  connection: Connection,
  mint: PublicKey | string,
): Promise<LiquidationCandidate[]> => {
  const market = await getPerpMarket(connection, mint)
  if (!market) return []
  const positions = await listPositionsForMarket(
    connection,
    typeof mint === 'string' ? new PublicKey(mint) : mint,
  )

  const candidates: LiquidationCandidate[] = []
  for (const { position } of positions) {
    const info = computePositionInfo(market, position)
    if (info.health !== 'liquidatable') continue

    // Expected bonus ≈ liquidation_penalty_bps × current_notional
    const current_notional_lamports = BigInt(
      Math.round(info.current_notional_sol * LAMPORTS_PER_SOL),
    )
    const bonus = (current_notional_lamports * BigInt(market.liquidation_penalty_bps)) / BigInt(BPS_DENOMINATOR)

    candidates.push({
      position: info,
      expected_bonus_lamports: Number(bonus),
    })
  }

  candidates.sort((a, b) => b.expected_bonus_lamports - a.expected_bonus_lamports)
  return candidates
}

// Keep required_margin/unrealized_pnl referenced for re-export sanity
void required_margin
void unrealized_pnl
void position_notional
