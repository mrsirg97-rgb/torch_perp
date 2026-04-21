/**
 * Anchor program wiring — IDL loading + helpers.
 *
 * We instantiate Programs with a read-only provider (no signer) for queries
 * and quote math. Transaction builders construct raw TransactionInstructions
 * and let the caller sign + send, so the SDK never touches private keys.
 */

import { AnchorProvider, BorshCoder, Idl, Program } from '@coral-xyz/anchor'
import { Connection, Keypair, PublicKey } from '@solana/web3.js'

import idlJson from './torch_perp.json'
import { TORCH_PERP_PROGRAM_ID } from './constants'

export const IDL = idlJson as unknown as Idl

// Read-only wallet stub — never signs. Used for construction only.
const READ_ONLY_WALLET = {
  publicKey: new PublicKey('11111111111111111111111111111111'),
  signTransaction: async (tx: any) => tx,
  signAllTransactions: async (txs: any[]) => txs,
  payer: Keypair.generate(),
}

export const getProgram = (connection: Connection): Program => {
  const provider = new AnchorProvider(connection, READ_ONLY_WALLET as any, {
    commitment: 'confirmed',
  })
  return new Program(IDL, provider)
}

export const getCoder = (): BorshCoder => new BorshCoder(IDL)

export { TORCH_PERP_PROGRAM_ID }
