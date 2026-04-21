/**
 * Constants mirroring programs/torch_perp/src/constants.rs.
 * Any deviation here breaks on-chain compatibility.
 */

import { PublicKey } from '@solana/web3.js'

// Program
export const TORCH_PERP_PROGRAM_ID = new PublicKey(
  '852yvbSWFCyVLRo8bWUPTiouM5amtw6JxctgS9P4ymdH',
)

// Solana base units
export const LAMPORTS_PER_SOL = 1_000_000_000
export const BPS_DENOMINATOR = 10_000

// PDA seeds
export const GLOBAL_CONFIG_SEED = Buffer.from('global_config')
export const PERP_MARKET_SEED = Buffer.from('perp_market')
export const PERP_POSITION_SEED = Buffer.from('perp_position')
export const INSURANCE_VAULT_SEED = Buffer.from('insurance_vault')

// ===== Trading =====
export const INITIAL_MARGIN_RATIO_BPS = 1_000 // 10% → max 10x leverage
export const MAINTENANCE_MARGIN_RATIO_BPS = 625 // 6.25%
export const LIQUIDATION_PENALTY_BPS = 500 // 5%

// ===== Fees =====
export const FEE_RATE_BPS = 10 // 0.10% taker
export const INSURANCE_FUND_CUT_BPS = 5_000 // 50% of fees → insurance

// ===== Funding =====
export const FUNDING_PERIOD_SLOTS = 9_000 // ~1hr at 400ms

// ===== TWAP =====
export const TWAP_RING_SIZE = 32
export const TWAP_WINDOW_SLOTS = 1_500 // ~10min

// ===== Percolator =====
export const POS_SCALE = 1_000_000_000_000_000_000n // 1e18
export const PRECISION_THRESHOLD = POS_SCALE / 1_000n
export const MATURED_WARMUP_SLOTS = 256

// Recovery phases
export const RECOVERY_NORMAL = 0
export const RECOVERY_DRAIN_ONLY = 1
export const RECOVERY_RESET_PENDING = 2

// Raydium CPMM
export const RAYDIUM_CPMM_PROGRAM_ID = new PublicKey(
  'CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C',
)
export const WSOL_MINT = new PublicKey('So11111111111111111111111111111111111111112')
