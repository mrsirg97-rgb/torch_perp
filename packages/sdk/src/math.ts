/**
 * Client-side math for quotes + health checks.
 *
 * Direct ports of programs/torch_perp/src/math.rs. Used for:
 *   - previewing open/close outcomes before submitting tx
 *   - computing position health for liquidation bots
 *   - building UI without an RPC round-trip
 *
 * All functions return `null` on overflow (mirrors `Option<T>` in Rust).
 * BigInt throughout to avoid JS Number precision loss on u128 values.
 */

import { BPS_DENOMINATOR } from './constants'

// ============================================================================
// vAMM swap
// ============================================================================

export interface SwapResult {
  output: bigint
  new_base_reserve: bigint
  new_quote_reserve: bigint
}

export const vamm_buy_base = (
  quote_in: bigint,
  base_reserve: bigint,
  quote_reserve: bigint,
): SwapResult | null => {
  if (quote_in === 0n) {
    return { output: 0n, new_base_reserve: base_reserve, new_quote_reserve: quote_reserve }
  }
  if (base_reserve === 0n || quote_reserve === 0n) return null
  const new_quote = quote_reserve + quote_in
  const base_out = (quote_in * base_reserve) / new_quote
  const new_base = base_reserve - base_out
  return { output: base_out, new_base_reserve: new_base, new_quote_reserve: new_quote }
}

export const vamm_sell_base = (
  base_in: bigint,
  base_reserve: bigint,
  quote_reserve: bigint,
): SwapResult | null => {
  if (base_in === 0n) {
    return { output: 0n, new_base_reserve: base_reserve, new_quote_reserve: quote_reserve }
  }
  if (base_reserve === 0n || quote_reserve === 0n) return null
  const new_base = base_reserve + base_in
  const quote_out = (base_in * quote_reserve) / new_base
  const new_quote = quote_reserve - quote_out
  return { output: quote_out, new_base_reserve: new_base, new_quote_reserve: new_quote }
}

// ============================================================================
// Fees
// ============================================================================

export const compute_fee = (notional: bigint, fee_rate_bps: number): bigint =>
  (notional * BigInt(fee_rate_bps)) / BigInt(BPS_DENOMINATOR)

export const split_fee = (
  fee: bigint,
  insurance_cut_bps: number,
): { to_insurance: bigint; to_protocol: bigint } => {
  const to_insurance = (fee * BigInt(insurance_cut_bps)) / BigInt(BPS_DENOMINATOR)
  return { to_insurance, to_protocol: fee - to_insurance }
}

// ============================================================================
// Position valuation & margin
// ============================================================================

export const position_notional = (
  abs_base: bigint,
  base_reserve: bigint,
  quote_reserve: bigint,
): bigint | null => {
  if (base_reserve === 0n) return null
  return (abs_base * quote_reserve) / base_reserve
}

export const unrealized_pnl = (
  base_asset_amount: bigint,
  entry_notional: bigint,
  current_notional: bigint,
): bigint => {
  if (base_asset_amount > 0n) return current_notional - entry_notional
  if (base_asset_amount < 0n) return entry_notional - current_notional
  return 0n
}

export const required_margin = (notional: bigint, margin_ratio_bps: number): bigint =>
  (notional * BigInt(margin_ratio_bps)) / BigInt(BPS_DENOMINATOR)

export const check_initial_margin = (
  notional: bigint,
  collateral: bigint,
  initial_margin_ratio_bps: number,
): boolean => collateral >= required_margin(notional, initial_margin_ratio_bps)

export const is_above_maintenance = (
  notional: bigint,
  equity: bigint,
  maintenance_margin_ratio_bps: number,
): boolean => {
  if (equity <= 0n) return false
  return equity >= required_margin(notional, maintenance_margin_ratio_bps)
}

// ============================================================================
// Price impact (bps) for a given swap
// ============================================================================

// Signed price impact in bps: (post_price - pre_price) / pre_price × 10_000.
// Positive = mark went up (buying base); negative = went down (selling base).
export const compute_price_impact_bps = (
  pre_base: bigint,
  pre_quote: bigint,
  post_base: bigint,
  post_quote: bigint,
): number => {
  if (pre_base === 0n || post_base === 0n) return 0
  // price = quote / base; we use (quote × 10000) / base for precision in bps arithmetic
  const pre_ratio = (pre_quote * 10_000n) / pre_base
  const post_ratio = (post_quote * 10_000n) / post_base
  if (pre_ratio === 0n) return 0
  const delta_bps = ((post_ratio - pre_ratio) * 10_000n) / pre_ratio
  return Number(delta_bps)
}
