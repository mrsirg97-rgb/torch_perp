/**
 * Transaction builders. Each function returns a TransactionInstruction (and
 * any auxiliary info like PDAs used), letting the caller compose, sign, and
 * send. The SDK never signs anything.
 *
 * Pattern mirrors torchsdk: return an object with { instruction, ... } so
 * UIs can compose multiple ix into one tx (e.g., create ATA + open position).
 */

import {
  Connection,
  PublicKey,
  SystemProgram,
  TransactionInstruction,
} from '@solana/web3.js'
import BN from 'bn.js'

import {
  FEE_RATE_BPS,
  FUNDING_PERIOD_SLOTS,
  INITIAL_MARGIN_RATIO_BPS,
  INSURANCE_FUND_CUT_BPS,
  LIQUIDATION_PENALTY_BPS,
  MAINTENANCE_MARGIN_RATIO_BPS,
} from './constants'
import {
  getGlobalConfigPda,
  getInsuranceVaultPda,
  getPerpMarketPda,
  getPerpPositionPda,
} from './pda'
import { getProgram } from './program'

// ============================================================================
// Helpers
// ============================================================================

const toPk = (k: PublicKey | string): PublicKey =>
  typeof k === 'string' ? new PublicKey(k) : k

export interface BuildResult {
  instruction: TransactionInstruction
  accounts: Record<string, string>
}

// ============================================================================
// Initialize global config
// ============================================================================

export interface InitializeGlobalConfigParams {
  authority: PublicKey | string
  protocol_treasury: PublicKey | string
  fee_rate_bps?: number
  insurance_fund_cut_bps?: number
}

export const buildInitializeGlobalConfigInstruction = async (
  connection: Connection,
  params: InitializeGlobalConfigParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const authority = toPk(params.authority)
  const protocolTreasury = toPk(params.protocol_treasury)
  const [globalConfig] = getGlobalConfigPda()

  const ix = await program.methods
    .initializeGlobalConfig(
      params.fee_rate_bps ?? FEE_RATE_BPS,
      params.insurance_fund_cut_bps ?? INSURANCE_FUND_CUT_BPS,
    )
    .accounts({
      authority,
      protocolTreasury,
      globalConfig,
      systemProgram: SystemProgram.programId,
    } as any)
    .instruction()

  return {
    instruction: ix,
    accounts: { globalConfig: globalConfig.toString() },
  }
}

// ============================================================================
// Initialize market (permissionless after token migration)
// ============================================================================

export interface InitializeMarketParams {
  initializer: PublicKey | string
  mint: PublicKey | string
  spot_pool: PublicKey | string
  spot_vault_0: PublicKey | string
  spot_vault_1: PublicKey | string
  vamm_quote_reserve: bigint | BN // SOL depth the vAMM starts with
  initial_margin_ratio_bps?: number
  maintenance_margin_ratio_bps?: number
  liquidation_penalty_bps?: number
  funding_period_slots?: bigint | BN
}

export const buildInitializeMarketInstruction = async (
  connection: Connection,
  params: InitializeMarketParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const initializer = toPk(params.initializer)
  const mint = toPk(params.mint)
  const [market] = getPerpMarketPda(mint)
  const [insuranceVault] = getInsuranceVaultPda(mint)

  const vammQuote =
    params.vamm_quote_reserve instanceof BN
      ? params.vamm_quote_reserve
      : new BN(params.vamm_quote_reserve.toString())
  const fundingPeriod =
    params.funding_period_slots instanceof BN
      ? params.funding_period_slots
      : new BN((params.funding_period_slots ?? BigInt(FUNDING_PERIOD_SLOTS)).toString())

  const ix = await program.methods
    .initializeMarket(
      params.initial_margin_ratio_bps ?? INITIAL_MARGIN_RATIO_BPS,
      params.maintenance_margin_ratio_bps ?? MAINTENANCE_MARGIN_RATIO_BPS,
      params.liquidation_penalty_bps ?? LIQUIDATION_PENALTY_BPS,
      fundingPeriod,
      vammQuote,
    )
    .accounts({
      initializer,
      mint,
      spotPool: toPk(params.spot_pool),
      spotVault0: toPk(params.spot_vault_0),
      spotVault1: toPk(params.spot_vault_1),
      market,
      insuranceVault,
      systemProgram: SystemProgram.programId,
    } as any)
    .instruction()

  return {
    instruction: ix,
    accounts: {
      market: market.toString(),
      insuranceVault: insuranceVault.toString(),
    },
  }
}

// ============================================================================
// Open position
// ============================================================================

export interface OpenPositionParams {
  user: PublicKey | string
  mint: PublicKey | string // the torch token (used to derive market)
  base_amount: bigint | BN // signed i64 — sim inputs use u64 notional; handler signature takes i64
  collateral_lamports: bigint | BN
  max_price_impact_bps?: number
  spot_pool: PublicKey | string
  spot_vault_0: PublicKey | string
  spot_vault_1: PublicKey | string
}

export const buildOpenPositionInstruction = async (
  connection: Connection,
  params: OpenPositionParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const user = toPk(params.user)
  const mint = toPk(params.mint)
  const [market] = getPerpMarketPda(mint)
  const [position] = getPerpPositionPda(market, user)
  const [globalConfig] = getGlobalConfigPda()
  const [insuranceVault] = getInsuranceVaultPda(mint)

  const baseAmount =
    params.base_amount instanceof BN ? params.base_amount : new BN(params.base_amount.toString())
  const collateral =
    params.collateral_lamports instanceof BN
      ? params.collateral_lamports
      : new BN(params.collateral_lamports.toString())

  // Protocol treasury must be read from globalConfig. Caller passes nothing —
  // we fetch the config to get the treasury address.
  const globalConfigInfo = await connection.getAccountInfo(globalConfig, 'confirmed')
  if (!globalConfigInfo) {
    throw new Error('global_config not initialized — call initializeGlobalConfig first')
  }
  // We need the protocol_treasury pubkey from the config. Decode lazily via the SDK's state layer.
  const { decodeGlobalConfig } = await import('./state')
  const cfg = decodeGlobalConfig(globalConfigInfo.data)
  const protocolTreasury = cfg.protocol_treasury

  const ix = await program.methods
    .openPosition(baseAmount, collateral, params.max_price_impact_bps ?? 10_000)
    .accounts({
      user,
      market,
      spotPool: toPk(params.spot_pool),
      spotVault0: toPk(params.spot_vault_0),
      spotVault1: toPk(params.spot_vault_1),
      position,
      globalConfig,
      protocolTreasury,
      insuranceVault,
      systemProgram: SystemProgram.programId,
    } as any)
    .instruction()

  return {
    instruction: ix,
    accounts: {
      market: market.toString(),
      position: position.toString(),
      insuranceVault: insuranceVault.toString(),
    },
  }
}

// ============================================================================
// Close position
// ============================================================================

export interface ClosePositionParams {
  user: PublicKey | string
  mint: PublicKey | string
  min_quote_out?: bigint | BN
  spot_pool: PublicKey | string
  spot_vault_0: PublicKey | string
  spot_vault_1: PublicKey | string
}

// ============================================================================
// Partial close (v1.2)
// ============================================================================

export interface PartialClosePositionParams {
  user: PublicKey | string
  mint: PublicKey | string
  base_to_close: bigint | BN // must be 0 < base_to_close < |position.base_asset_amount|
  min_quote_out?: bigint | BN
  spot_pool: PublicKey | string
  spot_vault_0: PublicKey | string
  spot_vault_1: PublicKey | string
}

export const buildPartialClosePositionInstruction = async (
  connection: Connection,
  params: PartialClosePositionParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const user = toPk(params.user)
  const mint = toPk(params.mint)
  const [market] = getPerpMarketPda(mint)
  const [position] = getPerpPositionPda(market, user)
  const [globalConfig] = getGlobalConfigPda()
  const [insuranceVault] = getInsuranceVaultPda(mint)

  const globalConfigInfo = await connection.getAccountInfo(globalConfig, 'confirmed')
  if (!globalConfigInfo) throw new Error('global_config not initialized')
  const { decodeGlobalConfig } = await import('./state')
  const cfg = decodeGlobalConfig(globalConfigInfo.data)

  const baseToClose =
    params.base_to_close instanceof BN
      ? params.base_to_close
      : new BN(params.base_to_close.toString())
  const minQuote =
    params.min_quote_out instanceof BN
      ? params.min_quote_out
      : new BN((params.min_quote_out ?? 0n).toString())

  const ix = await program.methods
    .partialClosePosition(baseToClose, minQuote)
    .accounts({
      user,
      market,
      spotPool: toPk(params.spot_pool),
      spotVault0: toPk(params.spot_vault_0),
      spotVault1: toPk(params.spot_vault_1),
      position,
      globalConfig,
      protocolTreasury: cfg.protocol_treasury,
      insuranceVault,
      systemProgram: SystemProgram.programId,
    } as any)
    .instruction()

  return {
    instruction: ix,
    accounts: { market: market.toString(), position: position.toString() },
  }
}

export const buildClosePositionInstruction = async (
  connection: Connection,
  params: ClosePositionParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const user = toPk(params.user)
  const mint = toPk(params.mint)
  const [market] = getPerpMarketPda(mint)
  const [position] = getPerpPositionPda(market, user)
  const [globalConfig] = getGlobalConfigPda()
  const [insuranceVault] = getInsuranceVaultPda(mint)

  const globalConfigInfo = await connection.getAccountInfo(globalConfig, 'confirmed')
  if (!globalConfigInfo) throw new Error('global_config not initialized')
  const { decodeGlobalConfig } = await import('./state')
  const cfg = decodeGlobalConfig(globalConfigInfo.data)

  const minQuote =
    params.min_quote_out instanceof BN
      ? params.min_quote_out
      : new BN((params.min_quote_out ?? 0n).toString())

  const ix = await program.methods
    .closePosition(minQuote)
    .accounts({
      user,
      market,
      spotPool: toPk(params.spot_pool),
      spotVault0: toPk(params.spot_vault_0),
      spotVault1: toPk(params.spot_vault_1),
      position,
      globalConfig,
      protocolTreasury: cfg.protocol_treasury,
      insuranceVault,
      systemProgram: SystemProgram.programId,
    } as any)
    .instruction()

  return {
    instruction: ix,
    accounts: { market: market.toString(), position: position.toString() },
  }
}

// ============================================================================
// Deposit / withdraw collateral
// ============================================================================

export interface CollateralParams {
  user: PublicKey | string
  mint: PublicKey | string
  amount: bigint | BN
}

export const buildDepositCollateralInstruction = async (
  connection: Connection,
  params: CollateralParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const user = toPk(params.user)
  const mint = toPk(params.mint)
  const [market] = getPerpMarketPda(mint)
  const [position] = getPerpPositionPda(market, user)
  const amount = params.amount instanceof BN ? params.amount : new BN(params.amount.toString())

  const ix = await program.methods
    .depositCollateral(amount)
    .accounts({
      user,
      market,
      position,
      systemProgram: SystemProgram.programId,
    } as any)
    .instruction()

  return {
    instruction: ix,
    accounts: { market: market.toString(), position: position.toString() },
  }
}

export interface WithdrawCollateralParams extends CollateralParams {
  spot_pool: PublicKey | string
  spot_vault_0: PublicKey | string
  spot_vault_1: PublicKey | string
}

export const buildWithdrawCollateralInstruction = async (
  connection: Connection,
  params: WithdrawCollateralParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const user = toPk(params.user)
  const mint = toPk(params.mint)
  const [market] = getPerpMarketPda(mint)
  const [position] = getPerpPositionPda(market, user)
  const amount = params.amount instanceof BN ? params.amount : new BN(params.amount.toString())

  const ix = await program.methods
    .withdrawCollateral(amount)
    .accounts({
      user,
      market,
      position,
      spotPool: toPk(params.spot_pool),
      spotVault0: toPk(params.spot_vault_0),
      spotVault1: toPk(params.spot_vault_1),
    } as any)
    .instruction()

  return {
    instruction: ix,
    accounts: { market: market.toString(), position: position.toString() },
  }
}

// ============================================================================
// Liquidate position (permissionless)
// ============================================================================

export interface LiquidatePositionParams {
  liquidator: PublicKey | string
  mint: PublicKey | string
  position_owner: PublicKey | string
  spot_pool: PublicKey | string
  spot_vault_0: PublicKey | string
  spot_vault_1: PublicKey | string
}

export const buildLiquidatePositionInstruction = async (
  connection: Connection,
  params: LiquidatePositionParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const liquidator = toPk(params.liquidator)
  const mint = toPk(params.mint)
  const positionOwner = toPk(params.position_owner)
  const [market] = getPerpMarketPda(mint)
  const [position] = getPerpPositionPda(market, positionOwner)
  const [insuranceVault] = getInsuranceVaultPda(mint)

  const ix = await program.methods
    .liquidatePosition()
    .accounts({
      liquidator,
      market,
      spotPool: toPk(params.spot_pool),
      spotVault0: toPk(params.spot_vault_0),
      spotVault1: toPk(params.spot_vault_1),
      position,
      positionOwner,
      insuranceVault,
      systemProgram: SystemProgram.programId,
    } as any)
    .instruction()

  return {
    instruction: ix,
    accounts: { market: market.toString(), position: position.toString() },
  }
}

// ============================================================================
// Permissionless cranks — update_funding + write_observation
// ============================================================================

export interface MarketCrankParams {
  mint: PublicKey | string
  spot_pool: PublicKey | string
  spot_vault_0: PublicKey | string
  spot_vault_1: PublicKey | string
}

export const buildUpdateFundingInstruction = async (
  connection: Connection,
  params: MarketCrankParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const mint = toPk(params.mint)
  const [market] = getPerpMarketPda(mint)

  const ix = await program.methods
    .updateFunding()
    .accounts({
      market,
      spotPool: toPk(params.spot_pool),
      spotVault0: toPk(params.spot_vault_0),
      spotVault1: toPk(params.spot_vault_1),
    } as any)
    .instruction()

  return { instruction: ix, accounts: { market: market.toString() } }
}

export const buildWriteObservationInstruction = async (
  connection: Connection,
  params: MarketCrankParams,
): Promise<BuildResult> => {
  const program = getProgram(connection)
  const mint = toPk(params.mint)
  const [market] = getPerpMarketPda(mint)

  const ix = await program.methods
    .writeObservation()
    .accounts({
      market,
      spotPool: toPk(params.spot_pool),
      spotVault0: toPk(params.spot_vault_0),
      spotVault1: toPk(params.spot_vault_1),
    } as any)
    .instruction()

  return { instruction: ix, accounts: { market: market.toString() } }
}
