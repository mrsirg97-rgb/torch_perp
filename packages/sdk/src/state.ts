/**
 * Account decoders. Consumed by queries.ts.
 */

import { Connection, PublicKey } from '@solana/web3.js'

import { getCoder } from './program'
import { GlobalConfig, PerpMarket, PerpPosition } from './types'

export const decodeGlobalConfig = (data: Buffer): GlobalConfig => {
  return getCoder().accounts.decode('GlobalConfig', data) as unknown as GlobalConfig
}

export const decodePerpMarket = (data: Buffer): PerpMarket => {
  return getCoder().accounts.decode('PerpMarket', data) as unknown as PerpMarket
}

export const decodePerpPosition = (data: Buffer): PerpPosition => {
  return getCoder().accounts.decode('PerpPosition', data) as unknown as PerpPosition
}

export const fetchRawAccount = async (
  connection: Connection,
  pubkey: PublicKey,
): Promise<Buffer | null> => {
  const info = await connection.getAccountInfo(pubkey, 'confirmed')
  if (!info) return null
  return info.data
}
