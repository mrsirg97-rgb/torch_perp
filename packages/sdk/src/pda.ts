/**
 * PDA derivers for torch_perp accounts.
 * Mirrors the seeds in programs/torch_perp/src/contexts.rs.
 */

import { PublicKey } from '@solana/web3.js'

import {
  GLOBAL_CONFIG_SEED,
  INSURANCE_VAULT_SEED,
  PERP_MARKET_SEED,
  PERP_POSITION_SEED,
  TORCH_PERP_PROGRAM_ID,
} from './constants'

export const getGlobalConfigPda = (): [PublicKey, number] =>
  PublicKey.findProgramAddressSync([GLOBAL_CONFIG_SEED], TORCH_PERP_PROGRAM_ID)

export const getPerpMarketPda = (mint: PublicKey): [PublicKey, number] =>
  PublicKey.findProgramAddressSync(
    [PERP_MARKET_SEED, mint.toBuffer()],
    TORCH_PERP_PROGRAM_ID,
  )

export const getInsuranceVaultPda = (mint: PublicKey): [PublicKey, number] =>
  PublicKey.findProgramAddressSync(
    [INSURANCE_VAULT_SEED, mint.toBuffer()],
    TORCH_PERP_PROGRAM_ID,
  )

export const getPerpPositionPda = (
  market: PublicKey,
  user: PublicKey,
): [PublicKey, number] =>
  PublicKey.findProgramAddressSync(
    [PERP_POSITION_SEED, market.toBuffer(), user.toBuffer()],
    TORCH_PERP_PROGRAM_ID,
  )
