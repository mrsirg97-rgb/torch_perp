/**
 * Read-only queries — market state, position state, derived views.
 */

import { Connection, PublicKey } from '@solana/web3.js'

import {
  LAMPORTS_PER_SOL,
  POS_SCALE,
  RECOVERY_DRAIN_ONLY,
  RECOVERY_NORMAL,
  RECOVERY_RESET_PENDING,
  TORCH_PERP_PROGRAM_ID,
} from './constants'
import {
  is_above_maintenance,
  position_notional,
  required_margin,
  unrealized_pnl,
} from './math'
import {
  getGlobalConfigPda,
  getPerpMarketPda,
  getPerpPositionPda,
} from './pda'
import { getCoder, IDL } from './program'
import { decodeGlobalConfig, decodePerpMarket, decodePerpPosition, fetchRawAccount } from './state'
import {
  GlobalConfig,
  PerpMarket,
  PerpMarketSummary,
  PerpPosition,
  PerpPositionInfo,
  PositionHealth,
  RecoveryPhaseName,
} from './types'

// ============================================================================
// Raw account fetchers
// ============================================================================

export const getGlobalConfig = async (
  connection: Connection,
): Promise<GlobalConfig | null> => {
  const [pda] = getGlobalConfigPda()
  const data = await fetchRawAccount(connection, pda)
  if (!data) return null
  return decodeGlobalConfig(data)
}

export const getPerpMarket = async (
  connection: Connection,
  mint: PublicKey | string,
): Promise<PerpMarket | null> => {
  const mintPk = typeof mint === 'string' ? new PublicKey(mint) : mint
  const [pda] = getPerpMarketPda(mintPk)
  const data = await fetchRawAccount(connection, pda)
  if (!data) return null
  return decodePerpMarket(data)
}

export const getPerpPosition = async (
  connection: Connection,
  market: PublicKey | string,
  user: PublicKey | string,
): Promise<PerpPosition | null> => {
  const marketPk = typeof market === 'string' ? new PublicKey(market) : market
  const userPk = typeof user === 'string' ? new PublicKey(user) : user
  const [pda] = getPerpPositionPda(marketPk, userPk)
  const data = await fetchRawAccount(connection, pda)
  if (!data) return null
  return decodePerpPosition(data)
}

// ============================================================================
// Derived views — summaries + health
// ============================================================================

const phaseName = (n: number): RecoveryPhaseName => {
  if (n === RECOVERY_NORMAL) return 'Normal'
  if (n === RECOVERY_DRAIN_ONLY) return 'DrainOnly'
  if (n === RECOVERY_RESET_PENDING) return 'ResetPending'
  return 'Normal'
}

export const summarizeMarket = (market: PerpMarket): PerpMarketSummary => {
  const base = BigInt(market.base_asset_reserve.toString())
  const quote = BigInt(market.quote_asset_reserve.toString())
  const mark = base > 0n ? Number(quote) / Number(base) : 0

  const [pda] = getPerpMarketPda(market.mint)
  return {
    address: pda.toString(),
    mint: market.mint.toString(),
    mark_price_sol: mark,
    spot_pool: market.spot_pool.toString(),
    base_asset_reserve: base.toString(),
    quote_asset_reserve: quote.toString(),
    open_interest_long: Number(market.open_interest_long.toString()),
    open_interest_short: Number(market.open_interest_short.toString()),
    insurance_balance_sol: Number(market.insurance_balance.toString()) / LAMPORTS_PER_SOL,
    a_index_ratio: Number(BigInt(market.a_index.toString()) * 10_000n / POS_SCALE) / 10_000,
    recovery_phase: phaseName(market.recovery_phase),
    epoch: market.epoch,
    initial_margin_ratio_bps: market.initial_margin_ratio_bps,
    maintenance_margin_ratio_bps: market.maintenance_margin_ratio_bps,
  }
}

export const computePositionInfo = (
  market: PerpMarket,
  position: PerpPosition,
): PerpPositionInfo => {
  const base_i = BigInt(position.base_asset_amount.toString())
  const abs_base = base_i < 0n ? -base_i : base_i
  const base_r = BigInt(market.base_asset_reserve.toString())
  const quote_r = BigInt(market.quote_asset_reserve.toString())
  const collateral = BigInt(position.quote_asset_collateral.toString())
  const entry = BigInt(position.entry_notional.toString())

  const direction: PerpPositionInfo['direction'] =
    base_i > 0n ? 'long' : base_i < 0n ? 'short' : 'flat'

  let current_notional = 0n
  const cur_res = position_notional(abs_base, base_r, quote_r)
  if (cur_res !== null) current_notional = cur_res

  const upnl = unrealized_pnl(base_i, entry, current_notional)
  const equity = collateral + upnl // i128 math in bigint
  const mmr_bps = market.maintenance_margin_ratio_bps

  let health: PositionHealth
  if (base_i === 0n) {
    health = 'none'
  } else if (!is_above_maintenance(current_notional, equity, mmr_bps)) {
    health = 'liquidatable'
  } else {
    // at_risk if equity is within 1.5x of the maintenance requirement
    const buffer = (required_margin(current_notional, mmr_bps) * 3n) / 2n
    health = equity < buffer ? 'at_risk' : 'healthy'
  }

  const ltv_bps =
    current_notional > 0n
      ? Number((equity * 10_000n) / current_notional)
      : null

  const [pda] = getPerpPositionPda(market.mint, position.user) // note: uses mint indirectly via market, see pda.ts

  return {
    address: pda.toString(),
    user: position.user.toString(),
    market: position.market.toString(),
    direction,
    base_asset_amount: Number(base_i),
    collateral_sol: Number(collateral) / LAMPORTS_PER_SOL,
    entry_notional_sol: Number(entry) / LAMPORTS_PER_SOL,
    current_notional_sol: Number(current_notional) / LAMPORTS_PER_SOL,
    unrealized_pnl_sol: Number(upnl) / LAMPORTS_PER_SOL,
    equity_sol: Number(equity) / LAMPORTS_PER_SOL,
    current_ltv_bps: ltv_bps,
    health,
    open_slot: Number(position.open_slot.toString()),
    open_epoch: position.open_epoch,
  }
}

// ============================================================================
// Bulk queries
// ============================================================================

// Fetch all PerpMarket accounts. Used by frontends building a market list.
export const listPerpMarkets = async (connection: Connection): Promise<PerpMarket[]> => {
  const disc = getCoder().accounts.accountDiscriminator('PerpMarket')
  // Lazy-load bs58 to avoid top-level dep surprises
  const bs58 = await import('bs58')
  const accounts = await connection.getProgramAccounts(TORCH_PERP_PROGRAM_ID, {
    filters: [{ memcmp: { offset: 0, bytes: bs58.default.encode(disc) } }],
  })
  const markets: PerpMarket[] = []
  for (const acc of accounts) {
    try {
      markets.push(decodePerpMarket(acc.account.data))
    } catch {
      // skip malformed
    }
  }
  return markets
}

// Fetch all PerpPosition accounts for a given market (for liquidation scanners).
export const listPositionsForMarket = async (
  connection: Connection,
  market: PublicKey | string,
): Promise<{ position: PerpPosition; pubkey: string }[]> => {
  const marketPk = typeof market === 'string' ? new PublicKey(market) : market
  const disc = getCoder().accounts.accountDiscriminator('PerpPosition')
  const bs58 = await import('bs58')
  const accounts = await connection.getProgramAccounts(TORCH_PERP_PROGRAM_ID, {
    filters: [
      { memcmp: { offset: 0, bytes: bs58.default.encode(disc) } },
      { memcmp: { offset: 8 + 32, bytes: marketPk.toBase58() } }, // market field at offset 40
    ],
  })
  const out: { position: PerpPosition; pubkey: string }[] = []
  for (const acc of accounts) {
    try {
      out.push({
        position: decodePerpPosition(acc.account.data),
        pubkey: acc.pubkey.toString(),
      })
    } catch {
      // skip
    }
  }
  return out
}

// Keep IDL import referenced for tree-shaken bundles
void IDL
