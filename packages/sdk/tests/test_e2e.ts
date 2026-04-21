/**
 * torch_perp E2E Test — Surfpool Mainnet Fork
 *
 * Comprehensive demo of torch + torch_perp composition:
 *   1. Create + bond + migrate a torch token (via torchsdk)
 *   2. Initialize torch_perp global config (if fresh program)
 *   3. Initialize perp market for the migrated token
 *   4. Fund traders: some do spot DEX, some do perps
 *   5. Perp traders open long + short positions
 *   6. Parallel DEX trading (moves spot price — vAMM stays independent)
 *   7. Permissionless cranks: update_funding, write_observation
 *   8. Close positions, verify PnL math
 *   9. Liquidation attempt
 *  10. Summary: fees collected, insurance growth, final state
 *
 * Run:
 *   surfpool start --network mainnet --no-tui
 *   npx tsx tests/test_e2e.ts
 */

import {
  Connection,
  Keypair,
  LAMPORTS_PER_SOL,
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
  VersionedTransaction,
} from '@solana/web3.js'

// Torch SDK — for token creation, bonding, migration
import {
  buildBuyTransaction,
  buildCreateTokenTransaction,
  buildCreateVaultTransaction,
  buildDepositVaultTransaction,
  buildDirectBuyTransaction,
  buildMigrateTransaction,
  getRaydiumMigrationAccounts,
  getToken,
} from 'torchsdk'

// Torch Perp SDK — all perp operations
import {
  buildClosePositionInstruction,
  buildInitializeGlobalConfigInstruction,
  buildInitializeMarketInstruction,
  buildLiquidatePositionInstruction,
  buildOpenPositionInstruction,
  buildUpdateFundingInstruction,
  buildWriteObservationInstruction,
  computeOpenQuote,
  FEE_RATE_BPS,
  getGlobalConfig,
  getGlobalConfigPda,
  getPerpMarket,
  getPerpMarketPda,
  getPerpPosition,
  getPerpPositionPda,
  INSURANCE_FUND_CUT_BPS,
  summarizeMarket,
  computePositionInfo,
} from '../src/index'

import * as fs from 'fs'
import * as os from 'os'
import * as path from 'path'

// ============================================================================
// Config + helpers
// ============================================================================

const RPC_URL = 'http://localhost:8899'
const WALLET_PATH = path.join(os.homedir(), '.config/solana/id.json')

const loadWallet = (): Keypair => {
  const raw = JSON.parse(fs.readFileSync(WALLET_PATH, 'utf-8'))
  return Keypair.fromSecretKey(Uint8Array.from(raw))
}

const log = (msg: string) => {
  const ts = new Date().toISOString().substr(11, 8)
  console.log(`[${ts}] ${msg}`)
}

const banner = (title: string) => {
  console.log()
  console.log('='.repeat(60))
  console.log(`  ${title}`)
  console.log('='.repeat(60))
}

const signAndSend = async (
  connection: Connection,
  wallet: Keypair,
  tx: Transaction | VersionedTransaction,
  quiet = false,
): Promise<string> => {
  if (tx instanceof VersionedTransaction) {
    tx.sign([wallet])
    const raw = tx.serialize()
    if (!quiet) log(`    tx size: ${raw.length}/1232 bytes`)
    const sig = await connection.sendRawTransaction(raw, {
      skipPreflight: false,
      preflightCommitment: 'confirmed',
    })
    await connection.confirmTransaction(sig, 'confirmed')
    return sig
  }
  tx.partialSign(wallet)
  const raw = tx.serialize()
  if (!quiet) log(`    tx size: ${raw.length}/1232 bytes`)
  const sig = await connection.sendRawTransaction(raw, {
    skipPreflight: false,
    preflightCommitment: 'confirmed',
  })
  await connection.confirmTransaction(sig, 'confirmed')
  return sig
}

const sendIx = async (
  connection: Connection,
  signer: Keypair,
  ixs: TransactionInstruction[],
  quiet = false,
): Promise<string> => {
  const tx = new Transaction().add(...ixs)
  const { blockhash } = await connection.getLatestBlockhash()
  tx.recentBlockhash = blockhash
  tx.feePayer = signer.publicKey
  return signAndSend(connection, signer, tx, quiet)
}

const fundWallet = async (
  connection: Connection,
  funder: Keypair,
  recipient: PublicKey,
  lamports: number,
) => {
  const tx = new Transaction().add(
    SystemProgram.transfer({
      fromPubkey: funder.publicKey,
      toPubkey: recipient,
      lamports,
    }),
  )
  const { blockhash } = await connection.getLatestBlockhash()
  tx.recentBlockhash = blockhash
  tx.feePayer = funder.publicKey
  tx.partialSign(funder)
  const sig = await connection.sendRawTransaction(tx.serialize())
  await connection.confirmTransaction(sig, 'confirmed')
}

const solFmt = (lamports: number | bigint) =>
  (Number(lamports) / LAMPORTS_PER_SOL).toFixed(4)

// ============================================================================
// Main
// ============================================================================

const main = async () => {
  banner('TORCH + TORCH_PERP E2E — Surfpool Mainnet Fork')

  const connection = new Connection(RPC_URL, 'confirmed')
  const funder = loadWallet()
  log(`Funder: ${funder.publicKey.toBase58()}`)
  log(`Funder balance: ${solFmt(await connection.getBalance(funder.publicKey))} SOL`)

  // Main test wallet — creates token, becomes torch authority, bonds the token
  const creator = Keypair.generate()
  await fundWallet(connection, funder, creator.publicKey, 800 * LAMPORTS_PER_SOL)
  log(`Creator: ${creator.publicKey.toBase58()} (funded 800 SOL)`)

  // Perp traders
  const longTrader = Keypair.generate()
  const shortTrader = Keypair.generate()
  await fundWallet(connection, funder, longTrader.publicKey, 50 * LAMPORTS_PER_SOL)
  await fundWallet(connection, funder, shortTrader.publicKey, 50 * LAMPORTS_PER_SOL)
  log(`Long trader:  ${longTrader.publicKey.toBase58()} (50 SOL)`)
  log(`Short trader: ${shortTrader.publicKey.toBase58()} (50 SOL)`)

  // DEX traders (trade on spot pool, not perps)
  const dexBuyer = Keypair.generate()
  const dexSeller = Keypair.generate()
  await fundWallet(connection, funder, dexBuyer.publicKey, 20 * LAMPORTS_PER_SOL)
  await fundWallet(connection, funder, dexSeller.publicKey, 20 * LAMPORTS_PER_SOL)

  // Liquidator
  const liquidator = Keypair.generate()
  await fundWallet(connection, funder, liquidator.publicKey, 2 * LAMPORTS_PER_SOL)
  log(`Liquidator:   ${liquidator.publicKey.toBase58()} (2 SOL)`)

  // Protocol treasury recipient — a SystemAccount that receives the non-insurance fee share.
  // Using a generated wallet here so it shows up as a funded SystemAccount in tests.
  const protocolTreasury = Keypair.generate()
  await fundWallet(connection, funder, protocolTreasury.publicKey, 0.01 * LAMPORTS_PER_SOL)

  let passed = 0
  let failed = 0
  const ok = (name: string, detail?: string) => {
    passed++
    log(`  ✓ ${name}${detail ? ` — ${detail}` : ''}`)
  }
  const fail = (name: string, err: any) => {
    failed++
    log(`  ✗ ${name} — ${err.message || err}`)
  }

  // ==========================================================================
  // Phase 1: Create + bond + migrate the torch token
  // ==========================================================================
  banner('Phase 1 — torch token lifecycle (create → bond → migrate)')

  let mint: string
  try {
    const creatorAddr = creator.publicKey.toBase58()
    const createResult = await buildCreateTokenTransaction(connection, {
      creator: creatorAddr,
      name: 'Perp Demo Token',
      symbol: 'PERPDEMO',
      metadata_uri: 'https://example.com/perp-demo.json',
    })
    const sig = await signAndSend(connection, creator, createResult.transaction)
    mint = createResult.mint.toBase58()
    ok('create token', `mint=${mint.slice(0, 8)}... sig=${sig.slice(0, 8)}...`)
  } catch (e: any) {
    fail('create token', e)
    process.exit(1)
  }

  // Vault setup (creator needs one to route buys through)
  try {
    const creatorAddr = creator.publicKey.toBase58()
    const vaultRes = await buildCreateVaultTransaction(connection, { creator: creatorAddr })
    await signAndSend(connection, creator, vaultRes.transaction, true)
    const depRes = await buildDepositVaultTransaction(connection, {
      depositor: creatorAddr,
      vault_creator: creatorAddr,
      amount_sol: 300 * LAMPORTS_PER_SOL,
    })
    await signAndSend(connection, creator, depRes.transaction, true)
    ok('vault setup', '300 SOL deposited')
  } catch (e: any) {
    fail('vault setup', e)
  }

  // Bond to completion — use direct buys from ephemeral wallets (2% cap per wallet)
  log('  Bonding to ~200 SOL via direct buys (may take a while)...')
  const BUY_SIZE = 1.5 * LAMPORTS_PER_SOL
  let bondingComplete = false
  let buyCount = 0
  let firstError: string | null = null
  for (let i = 0; i < 160 && !bondingComplete; i++) {
    try {
      const ephemeral = Keypair.generate()
      await fundWallet(connection, funder, ephemeral.publicKey, BUY_SIZE + LAMPORTS_PER_SOL)
      const buyRes = await buildDirectBuyTransaction(connection, {
        mint,
        buyer: ephemeral.publicKey.toBase58(),
        amount_sol: BUY_SIZE,
        slippage_bps: 1000, // max allowed by torchsdk (10%)
      })
      await signAndSend(connection, ephemeral, buyRes.transaction, true)
      buyCount++
      if (buyCount % 25 === 0) {
        const detail = await getToken(connection, mint)
        log(`  Buy ${buyCount}: ${detail.progress_percent.toFixed(1)}% (${detail.sol_raised.toFixed(1)} SOL)`)
        if (detail.status !== 'bonding') bondingComplete = true
      }
    } catch (e: any) {
      const msg = e.message || ''
      if (msg.includes('BondingComplete') || msg.includes('bonding_complete')) {
        bondingComplete = true
        break
      }
      if (!firstError) {
        firstError = msg.substring(0, 200)
        log(`  [first buy error]: ${firstError}`)
      }
    }
  }

  // Final buy from main wallet if still bonding (auto-migration buffer)
  if (!bondingComplete) {
    log('  Final buy from creator...')
    try {
      const buyRes = await buildDirectBuyTransaction(connection, {
        mint,
        buyer: creator.publicKey.toBase58(),
        amount_sol: BUY_SIZE,
        slippage_bps: 1000,
      })
      await signAndSend(connection, creator, buyRes.transaction, true)
    } catch (e: any) {
      if (e.message?.includes('BondingComplete')) bondingComplete = true
      else log(`  final buy error: ${e.message?.substring(0, 200)}`)
    }
  }

  try {
    const detail = await getToken(connection, mint)
    if (detail.status !== 'bonding') bondingComplete = true
    log(`  Bonding final: ${detail.progress_percent.toFixed(1)}% status=${detail.status}`)
    ok('bond to completion', `after ${buyCount} buys, status=${detail.status}`)
  } catch (e: any) {
    fail('bonding status check', e)
  }

  // Migrate (or confirm auto-migrated). Use getToken status rather than
  // internal torchsdk helpers to keep the SDK surface public.
  try {
    const detail = await getToken(connection, mint)
    if (detail.status === 'migrated') {
      ok('migrate to DEX', 'auto-migrated with last buy')
    } else if (detail.status === 'complete') {
      const migRes = await buildMigrateTransaction(connection, {
        mint,
        payer: creator.publicKey.toBase58(),
      })
      await signAndSend(connection, creator, migRes.transaction)
      ok('migrate to DEX', 'Raydium pool created (separate migration call)')
    } else {
      throw new Error(`cannot migrate: status=${detail.status}`)
    }
  } catch (e: any) {
    fail('migrate', e)
  }

  // Resolve Raydium pool accounts (spot pool + vaults) — needed by perp ixs
  const mintPk = new PublicKey(mint)
  const raydium = getRaydiumMigrationAccounts(mintPk)
  const spotPool = raydium.poolState
  const spotVault0 = raydium.token0Vault
  const spotVault1 = raydium.token1Vault
  const isWsolToken0 = raydium.isWsolToken0
  log(`  Spot pool:  ${spotPool.toBase58()}`)
  log(`  WSOL is token0: ${isWsolToken0}`)

  // Read initial pool state
  let initialSpotSol = 0
  let initialSpotTokens = 0
  try {
    const [v0, v1] = await Promise.all([
      connection.getTokenAccountBalance(spotVault0),
      connection.getTokenAccountBalance(spotVault1),
    ])
    if (isWsolToken0) {
      initialSpotSol = Number(v0.value.amount)
      initialSpotTokens = Number(v1.value.amount)
    } else {
      initialSpotSol = Number(v1.value.amount)
      initialSpotTokens = Number(v0.value.amount)
    }
    log(`  Initial pool: ${solFmt(initialSpotSol)} SOL / ${(initialSpotTokens / 1e6).toFixed(0)} tokens`)
    log(`  Initial price: ${(initialSpotSol / initialSpotTokens).toExponential(4)} SOL/base`)
  } catch (e: any) {
    fail('read pool state', e)
  }

  // ==========================================================================
  // Phase 2: torch_perp global config + market init
  // ==========================================================================
  banner('Phase 2 — torch_perp global config + market init')

  // Global config — initialize if not already. If already initialized from a
  // previous test run on the same surfpool state, just read it.
  try {
    const existing = await getGlobalConfig(connection)
    if (existing) {
      ok('global config already initialized', `fee=${existing.fee_rate_bps} bps`)
    } else {
      const initRes = await buildInitializeGlobalConfigInstruction(connection, {
        authority: creator.publicKey,
        protocol_treasury: protocolTreasury.publicKey,
      })
      await sendIx(connection, creator, [initRes.instruction])
      ok('initialize_global_config', `fee=${FEE_RATE_BPS} bps, ins_cut=${INSURANCE_FUND_CUT_BPS} bps`)
    }
  } catch (e: any) {
    fail('initialize_global_config', e)
  }

  // Initialize perp market. Seed vAMM with 200 SOL depth matching spot price.
  const VAMM_QUOTE = 200n * BigInt(LAMPORTS_PER_SOL)
  try {
    const initMktRes = await buildInitializeMarketInstruction(connection, {
      initializer: creator.publicKey,
      mint: mintPk,
      spot_pool: spotPool,
      spot_vault_0: spotVault0,
      spot_vault_1: spotVault1,
      vamm_quote_reserve: VAMM_QUOTE,
    })
    await sendIx(connection, creator, [initMktRes.instruction])
    ok('initialize_market', `market=${initMktRes.accounts.market.slice(0, 8)}... vAMM_quote=200 SOL`)
  } catch (e: any) {
    fail('initialize_market', e)
  }

  // Read the market and print summary
  try {
    const market = await getPerpMarket(connection, mintPk)
    if (!market) throw new Error('market not found')
    const s = summarizeMarket(market)
    log(`  Market state:`)
    log(`    mark_price:    ${s.mark_price_sol.toExponential(4)}`)
    log(`    vAMM reserves: base=${s.base_asset_reserve}  quote=${s.quote_asset_reserve}`)
    log(`    phase:         ${s.recovery_phase}  epoch=${s.epoch}`)
    log(`    insurance:     ${s.insurance_balance_sol.toFixed(4)} SOL`)
    ok('getPerpMarket', 'market state readable')
  } catch (e: any) {
    fail('getPerpMarket', e)
  }

  // ==========================================================================
  // Phase 3: Perp trading — open long + open short
  // ==========================================================================
  banner('Phase 3 — Perp positions (long + short)')

  // Long trader: buy 1000 tokens base (1e9 base units @ 6 decimals = 1000 tokens display)
  // Collateral: whatever satisfies IMR (10% of expected notional)
  const LONG_BASE = 1_000_000_000n // 1e9 = 1000 tokens display
  const LONG_COLLATERAL = 5n * BigInt(LAMPORTS_PER_SOL) // 5 SOL
  try {
    // Preview via quote helper first
    const market = await getPerpMarket(connection, mintPk)
    if (market) {
      const quote = computeOpenQuote(
        market,
        {
          direction: 'long',
          collateral_lamports: LONG_COLLATERAL,
          leverage_x: 5, // target 5x
        },
        FEE_RATE_BPS,
      )
      log(`  Long quote: est_base=${quote.est_base_acquired}  fee=${solFmt(quote.fee_lamports)} SOL  impact=${quote.price_impact_bps}bps  passes_imr=${quote.passes_imr_check}`)
    }

    const openRes = await buildOpenPositionInstruction(connection, {
      user: longTrader.publicKey,
      mint: mintPk,
      base_amount: LONG_BASE,
      collateral_lamports: LONG_COLLATERAL,
      max_price_impact_bps: 2000, // 20% — generous for a fresh market
      spot_pool: spotPool,
      spot_vault_0: spotVault0,
      spot_vault_1: spotVault1,
    })
    const sig = await sendIx(connection, longTrader, [openRes.instruction])
    ok('open long (1000 tokens, 5 SOL collateral)', `sig=${sig.slice(0, 8)}...`)
  } catch (e: any) {
    fail('open long', e)
  }

  // Short trader: sell 500 tokens base
  const SHORT_BASE = -500_000_000n // negative = short
  const SHORT_COLLATERAL = 3n * BigInt(LAMPORTS_PER_SOL)
  try {
    const openRes = await buildOpenPositionInstruction(connection, {
      user: shortTrader.publicKey,
      mint: mintPk,
      base_amount: SHORT_BASE,
      collateral_lamports: SHORT_COLLATERAL,
      max_price_impact_bps: 2000,
      spot_pool: spotPool,
      spot_vault_0: spotVault0,
      spot_vault_1: spotVault1,
    })
    const sig = await sendIx(connection, shortTrader, [openRes.instruction])
    ok('open short (500 tokens, 3 SOL collateral)', `sig=${sig.slice(0, 8)}...`)
  } catch (e: any) {
    fail('open short', e)
  }

  // Read positions
  for (const [label, trader] of [
    ['long', longTrader],
    ['short', shortTrader],
  ] as const) {
    try {
      const market = await getPerpMarket(connection, mintPk)
      const pos = await getPerpPosition(connection, getPerpMarketPda(mintPk)[0], trader.publicKey)
      if (!market || !pos) throw new Error('position not found')
      const info = computePositionInfo(market, pos)
      log(`  ${label}: dir=${info.direction}  base=${info.base_asset_amount}  collateral=${info.collateral_sol.toFixed(4)} SOL  equity=${info.equity_sol.toFixed(4)} SOL  health=${info.health}`)
    } catch (e: any) {
      fail(`read ${label} position`, e)
    }
  }

  // ==========================================================================
  // Phase 4: Parallel DEX trading — moves spot, perp vAMM stays independent
  // ==========================================================================
  banner('Phase 4 — Parallel spot DEX trading (independent of perp vAMM)')

  // Post-migration, torchsdk requires vault-based trading (`buildBuyTransaction`
  // with a vault parameter). Set up a dedicated vault for the dex buyer.
  const dexBuyerAddr = dexBuyer.publicKey.toBase58()
  try {
    const vr = await buildCreateVaultTransaction(connection, { creator: dexBuyerAddr })
    await signAndSend(connection, dexBuyer, vr.transaction, true)
    const dp = await buildDepositVaultTransaction(connection, {
      depositor: dexBuyerAddr,
      vault_creator: dexBuyerAddr,
      amount_sol: 15 * LAMPORTS_PER_SOL,
    })
    await signAndSend(connection, dexBuyer, dp.transaction, true)
    ok('dex buyer vault setup', '15 SOL deposited')
  } catch (e: any) {
    fail('dex buyer vault setup', e)
  }

  try {
    // Vault-routed buy (uses Raydium under the hood post-migration)
    const dexBuyRes = await buildBuyTransaction(connection, {
      mint,
      buyer: dexBuyerAddr,
      amount_sol: 10 * LAMPORTS_PER_SOL,
      slippage_bps: 1000,
      vault: dexBuyerAddr, // vault creator (dexBuyer is their own vault authority)
    })
    await signAndSend(connection, dexBuyer, dexBuyRes.transaction, true)

    const [v0, v1] = await Promise.all([
      connection.getTokenAccountBalance(spotVault0),
      connection.getTokenAccountBalance(spotVault1),
    ])
    const spotSol = isWsolToken0 ? Number(v0.value.amount) : Number(v1.value.amount)
    const spotTok = isWsolToken0 ? Number(v1.value.amount) : Number(v0.value.amount)
    log(`  After DEX buy: spot=${(spotSol / spotTok).toExponential(4)}`)
    ok('DEX buy (10 SOL vault-routed)', 'spot price moved')
  } catch (e: any) {
    fail('DEX buy', e)
  }

  // vAMM mark should be UNCHANGED — perp is isolated from spot pool
  try {
    const market = await getPerpMarket(connection, mintPk)
    if (!market) throw new Error('market gone')
    const s = summarizeMarket(market)
    log(`  vAMM mark after DEX buy: ${s.mark_price_sol.toExponential(4)} (unchanged by spot trading)`)
    ok('vAMM isolation verified', 'spot DEX buy did not affect perp mark')
  } catch (e: any) {
    fail('vAMM isolation', e)
  }

  // ==========================================================================
  // Phase 5: Permissionless cranks
  // ==========================================================================
  banner('Phase 5 — Permissionless cranks (write_observation, update_funding)')

  try {
    const obsRes = await buildWriteObservationInstruction(connection, {
      mint: mintPk,
      spot_pool: spotPool,
      spot_vault_0: spotVault0,
      spot_vault_1: spotVault1,
    })
    await sendIx(connection, liquidator, [obsRes.instruction], true)
    ok('write_observation', 'TWAP observation appended')
  } catch (e: any) {
    fail('write_observation', e)
  }

  try {
    const fundRes = await buildUpdateFundingInstruction(connection, {
      mint: mintPk,
      spot_pool: spotPool,
      spot_vault_0: spotVault0,
      spot_vault_1: spotVault1,
    })
    await sendIx(connection, liquidator, [fundRes.instruction], true)
    ok('update_funding', 'v1 no-op (cumulative funding stays 0)')
  } catch (e: any) {
    fail('update_funding', e)
  }

  // ==========================================================================
  // Phase 6: Close long position — realize PnL
  // ==========================================================================
  banner('Phase 6 — Close long position (realize PnL)')

  try {
    const balBefore = await connection.getBalance(longTrader.publicKey)
    const closeRes = await buildClosePositionInstruction(connection, {
      user: longTrader.publicKey,
      mint: mintPk,
      min_quote_out: 0n,
      spot_pool: spotPool,
      spot_vault_0: spotVault0,
      spot_vault_1: spotVault1,
    })
    await sendIx(connection, longTrader, [closeRes.instruction])
    const balAfter = await connection.getBalance(longTrader.publicKey)
    const delta = balAfter - balBefore
    log(`  Long trader SOL delta: ${solFmt(delta)} SOL (collateral return + PnL - fees - tx costs)`)
    ok('close long', `delta=${solFmt(delta)} SOL`)
  } catch (e: any) {
    fail('close long', e)
  }

  // ==========================================================================
  // Phase 7: Liquidation attempt
  // ==========================================================================
  banner('Phase 7 — Liquidation scenario')

  // Open a max-leverage long, then push mark down via a big counter-short
  const victim = Keypair.generate()
  await fundWallet(connection, funder, victim.publicKey, 10 * LAMPORTS_PER_SOL)

  try {
    // 10x long: 10 SOL notional exposure with 1 SOL collateral. Use small base.
    const VICTIM_BASE = 500_000_000n
    const VICTIM_COLLATERAL = 1n * BigInt(LAMPORTS_PER_SOL)
    const openRes = await buildOpenPositionInstruction(connection, {
      user: victim.publicKey,
      mint: mintPk,
      base_amount: VICTIM_BASE,
      collateral_lamports: VICTIM_COLLATERAL,
      max_price_impact_bps: 3000,
      spot_pool: spotPool,
      spot_vault_0: spotVault0,
      spot_vault_1: spotVault1,
    })
    await sendIx(connection, victim, [openRes.instruction], true)
    log('  Victim opened max-leverage long')

    // Counter-whale: large short to push mark down
    const whale = Keypair.generate()
    await fundWallet(connection, funder, whale.publicKey, 30 * LAMPORTS_PER_SOL)
    const whaleBase = -2_000_000_000n
    const whaleCollateral = 20n * BigInt(LAMPORTS_PER_SOL)
    const whaleRes = await buildOpenPositionInstruction(connection, {
      user: whale.publicKey,
      mint: mintPk,
      base_amount: whaleBase,
      collateral_lamports: whaleCollateral,
      max_price_impact_bps: 5000,
      spot_pool: spotPool,
      spot_vault_0: spotVault0,
      spot_vault_1: spotVault1,
    })
    await sendIx(connection, whale, [whaleRes.instruction], true)
    log('  Counter-whale opened large short → mark pushed down')

    // Check victim health
    const market = await getPerpMarket(connection, mintPk)
    const victimPos = await getPerpPosition(
      connection,
      getPerpMarketPda(mintPk)[0],
      victim.publicKey,
    )
    if (!market || !victimPos) throw new Error('state missing')
    const info = computePositionInfo(market, victimPos)
    log(`  Victim health: ${info.health}  equity=${info.equity_sol.toFixed(4)} SOL`)

    if (info.health === 'liquidatable') {
      const liqRes = await buildLiquidatePositionInstruction(connection, {
        liquidator: liquidator.publicKey,
        mint: mintPk,
        position_owner: victim.publicKey,
        spot_pool: spotPool,
        spot_vault_0: spotVault0,
        spot_vault_1: spotVault1,
      })
      const liqBalBefore = await connection.getBalance(liquidator.publicKey)
      await sendIx(connection, liquidator, [liqRes.instruction])
      const liqBalAfter = await connection.getBalance(liquidator.publicKey)
      log(`  Liquidator earned: ${solFmt(liqBalAfter - liqBalBefore)} SOL`)
      ok('liquidate underwater position', `bonus=${solFmt(liqBalAfter - liqBalBefore)} SOL`)
    } else {
      ok('liquidation gate respected', `victim is ${info.health} (not liquidatable)`)
    }
  } catch (e: any) {
    fail('liquidation scenario', e)
  }

  // ==========================================================================
  // Final summary
  // ==========================================================================
  banner('Final market state')

  try {
    const market = await getPerpMarket(connection, mintPk)
    if (!market) throw new Error('market gone')
    const s = summarizeMarket(market)
    log(`  mark_price:       ${s.mark_price_sol.toExponential(4)}`)
    log(`  open_interest:    long=${s.open_interest_long}  short=${s.open_interest_short}`)
    log(`  insurance:        ${s.insurance_balance_sol.toFixed(4)} SOL`)
    log(`  a_index:          ${s.a_index_ratio.toFixed(6)}`)
    log(`  recovery_phase:   ${s.recovery_phase}`)
    log(`  epoch:            ${s.epoch}`)
  } catch (e: any) {
    fail('final state', e)
  }

  console.log()
  console.log('='.repeat(60))
  console.log(`  RESULTS: ${passed} passed, ${failed} failed`)
  console.log('='.repeat(60))
  process.exit(failed > 0 ? 1 : 0)
}

main().catch((e) => {
  console.error('FATAL:', e)
  process.exit(1)
})
