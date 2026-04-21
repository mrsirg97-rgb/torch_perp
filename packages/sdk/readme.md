# torchperpsdk

TypeScript SDK for [torch_perp](https://github.com/mrsirg97-rgb/torch_perp) — leveraged perpetual futures on torch tokens. Oracle-free mark via vAMM, TWAP-backed funding (v1.1), percolator-style solvency for bad debt, integer-math parity with on-chain Rust.

Composes with [torchsdk](https://www.npmjs.com/package/torchsdk) — torch_perp is a *consumer* of torch state, not an extension.

## Install

```bash
pnpm add torchperpsdk
```

Peer dependency: `@solana/web3.js ^1.98.0`
Runtime dep: `@coral-xyz/anchor ^0.32.1`

## How It Works

```
1. Read market state  →  getPerpMarket / getPerpPosition
2. Quote a trade      →  computeOpenQuote / computeCloseQuote (client-side, no RPC)
3. Build a tx         →  buildOpenPositionInstruction (+ 8 others)
4. Sign and send      →  your wallet / keypair
```

The SDK never signs. Every builder returns `{ instruction, accounts }`; you compose into a `Transaction`/`VersionedTransaction` and sign.

## Operations

### Read

| Function | Description |
|---|---|
| `getGlobalConfig(connection)` | Protocol config: fee rates, treasury, authority |
| `getPerpMarket(connection, mint)` | Market state: vAMM reserves, OI, funding, insurance, percolator indices |
| `getPerpPosition(connection, market, user)` | User's position: size, collateral, entry, funding snapshots |
| `listPerpMarkets(connection)` | All markets on the program (indexer-grade scan) |
| `listPositionsForMarket(connection, market)` | All positions for a market (used by liquidation bots) |
| `summarizeMarket(market)` | Human-friendly view: mark price, phase, a_index ratio |
| `computePositionInfo(market, position)` | Derived view: direction, uPnL, equity, LTV, health |

### Quotes (client-side, no RPC)

| Function | Description |
|---|---|
| `computeOpenQuote(market, { direction, collateral, leverage_x }, fee_rate_bps)` | Preview: base acquired, fee, price impact, IMR check |
| `getOpenQuote(connection, mint, input)` | Async variant — fetches market first |
| `computeCloseQuote(market, position, fee_rate_bps)` | Preview: payout, realized PnL, fee, price impact |
| `getCloseQuote(connection, mint, position, fee_rate_bps?)` | Async variant |
| `getLiquidationCandidates(connection, mint)` | Positions below maintenance margin, sorted by bonus |

### Transaction Builders

| Function | Signer | Description |
|---|---|---|
| `buildInitializeGlobalConfigInstruction` | authority | One-time protocol init |
| `buildInitializeMarketInstruction` | anyone | Permissionless per-token market init |
| `buildOpenPositionInstruction` | user | Open long or short (signed `base_amount`) |
| `buildClosePositionInstruction` | user | Close full position, settle PnL |
| `buildDepositCollateralInstruction` | user | Top up margin |
| `buildWithdrawCollateralInstruction` | user | Withdraw margin (IMR-gated) |
| `buildLiquidatePositionInstruction` | anyone | Permissionless liquidation w/ bonus |
| `buildUpdateFundingInstruction` | anyone | Crank: update funding (v1 no-op) + TWAP obs |
| `buildWriteObservationInstruction` | anyone | Crank: TWAP observation |

### Pure Math (port of `math.rs`)

Available for custom quote logic or simulators. All `Option<T>`-equivalent (`null` on overflow). BigInt throughout.

| Function | Property |
|---|---|
| `vamm_buy_base(quote_in, base_r, quote_r)` | Returns `{ output, new_base_reserve, new_quote_reserve }` |
| `vamm_sell_base(base_in, base_r, quote_r)` | Same shape |
| `compute_fee(notional, fee_rate_bps)` | Floor rounding in pool's favor |
| `split_fee(fee, insurance_cut_bps)` | Conservation: `to_insurance + to_protocol == fee` |
| `position_notional(abs_base, base_r, quote_r)` | Mark-to-market value |
| `unrealized_pnl(base_amount, entry, current)` | Signed i64 |
| `required_margin(notional, ratio_bps)` | IMR/MMR math |
| `check_initial_margin(notional, collateral, imr_bps)` | Boolean gate |
| `is_above_maintenance(notional, equity, mmr_bps)` | Boolean gate |
| `compute_price_impact_bps(...)` | Signed bps (+ for price-up) |

### PDA Derivation

| Function | Seeds |
|---|---|
| `getGlobalConfigPda()` | `["global_config"]` |
| `getPerpMarketPda(mint)` | `["perp_market", mint]` |
| `getPerpPositionPda(market, user)` | `["perp_position", market, user]` |
| `getInsuranceVaultPda(mint)` | `["insurance_vault", mint]` |

## Example — Open a 5x long with preview

```typescript
import { Connection, Keypair, Transaction } from '@solana/web3.js'
import {
  getPerpMarket,
  computeOpenQuote,
  buildOpenPositionInstruction,
  FEE_RATE_BPS,
} from 'torchperpsdk'
import { getRaydiumMigrationAccounts } from 'torchsdk'

const connection = new Connection('https://api.mainnet-beta.solana.com')
const mint = 'YOUR_TORCH_TOKEN_MINT'
const user = Keypair.generate()

// 1. Preview: what do I get for 1 SOL collateral at 5x leverage?
const market = await getPerpMarket(connection, mint)
const quote = computeOpenQuote(
  market!,
  { direction: 'long', collateral_lamports: 1_000_000_000n, leverage_x: 5 },
  FEE_RATE_BPS,
)
console.log(`Est base: ${quote.est_base_acquired}`)
console.log(`Fee: ${quote.fee_lamports}`)
console.log(`Price impact: ${quote.price_impact_bps} bps`)
console.log(`Passes IMR: ${quote.passes_imr_check}`)

// 2. Build the ix
const raydium = getRaydiumMigrationAccounts(mint)
const { instruction } = await buildOpenPositionInstruction(connection, {
  user: user.publicKey,
  mint,
  base_amount: BigInt(quote.est_base_acquired), // signed: + long, - short
  collateral_lamports: 1_000_000_000n,
  max_price_impact_bps: 1000,
  spot_pool: raydium.poolState,
  spot_vault_0: raydium.token0Vault,
  spot_vault_1: raydium.token1Vault,
})

// 3. Sign + send
const tx = new Transaction().add(instruction)
// ... set blockhash, signer, send
```

## Example — Liquidation bot

```typescript
import { getLiquidationCandidates, buildLiquidatePositionInstruction } from 'torchperpsdk'

const candidates = await getLiquidationCandidates(connection, mint)
for (const c of candidates) {
  console.log(`${c.position.user}: bonus=${c.expected_bonus_lamports}`)
  const { instruction } = await buildLiquidatePositionInstruction(connection, {
    liquidator: liquidator.publicKey,
    mint,
    position_owner: c.position.user,
    spot_pool, spot_vault_0, spot_vault_1,
  })
  // sign + submit
}
```

## Key Properties

- **Oracle-free mark** — vAMM with its own k invariant, independent of spot pool price movements
- **Permissionless market init** — anyone can open a perp market for any migrated torch token
- **Insurance fund from fees** — 50% of taker fees compound into per-market insurance; no external capital
- **Proportional bad-debt absorption** — uses the A/K scaling math from [@aeyakovenko's percolator research](https://github.com/aeyakovenko/percolator) (thank you) to distribute residual losses across active positions
- **Deterministic recovery** — market phases Normal → DrainOnly → ResetPending → Normal; no frozen-forever failure mode
- **No keepers required** — cranks are permissionless and best-effort; protocol works if nobody runs them (just with delayed observations)
- **Token-2022 native** — composes with torch's transfer-fee flow
- **Formally verified** — 41 Kani proofs over the math layer (see [verification.md](../../docs/verification.md))
- **Sim-validated** — 15 scenarios including Monte Carlo stress + percolator activation (see [sim.md](../../docs/sim.md))

## Constants

| Parameter | Value | Notes |
|---|---|---|
| Initial margin ratio | 1000 bps (10%) | Max 10x leverage |
| Maintenance margin ratio | 625 bps (6.25%) | Industry baseline (matches Drift) |
| Liquidation penalty | 500 bps (5%) | Liquidator bonus |
| Taker fee | 10 bps (0.10%) | Applied on open + close |
| Insurance fund cut | 5000 bps (50%) | Of each fee; remainder → protocol treasury |
| Funding period | 9000 slots (~1hr) | v1: disabled (funding is v1.1) |
| TWAP ring | 32 observations | ~10min window target |
| POS_SCALE | 1e18 | Percolator A/K precision |
| PRECISION_THRESHOLD | POS_SCALE / 1000 | DrainOnly trigger |

## Testing

Bundled e2e test composes torchsdk (token create + bond + migrate) with torchperpsdk (market init + perp trading + liquidation):

```bash
surfpool start --network mainnet --no-tui
cd packages/sdk && npx tsx tests/test_e2e.ts
```

16 checks across 7 phases: create → bond → migrate → market init → open long/short → parallel spot DEX trading → cranks → close → liquidation gate.

## Links

- [Design](../../docs/design.md) — architecture + parameters + empirical properties
- [Verification](../../docs/verification.md) — 41 Kani proof harnesses
- [Simulator](../../docs/sim.md) — 15 economic scenarios + Monte Carlo stress
- Program: `852yvbSWFCyVLRo8bWUPTiouM5amtw6JxctgS9P4ymdH`
- Built on [torch.market](https://torch.market)

## License

MIT
