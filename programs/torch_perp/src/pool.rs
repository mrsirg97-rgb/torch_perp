// Spot pool validation + live-reserve reads.
//
// v1 supports Raydium CPMM only. DeepPool support is a pool_kind dispatch
// to be added in v1.1 (when v20 torch tokens start migrating to DeepPool).
// Raydium layout constants mirror torch_market for consistency.

use anchor_lang::prelude::*;

use crate::errors::TorchPerpError;

// Raydium CPMM program ID (mainnet)
#[cfg(not(feature = "devnet"))]
pub const RAYDIUM_CPMM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    169, 42, 90, 139, 79, 41, 89, 82, 132, 37, 80, 170, 147, 253, 91, 149,
    181, 172, 230, 168, 235, 146, 12, 147, 148, 46, 67, 105, 12, 32, 236, 115,
]);
#[cfg(feature = "devnet")]
pub const RAYDIUM_CPMM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    169, 42, 49, 26, 136, 152, 134, 77, 32, 99, 200, 252, 203, 83, 110, 30,
    138, 48, 77, 141, 83, 152, 76, 10, 78, 179, 193, 68, 7, 214, 116, 231,
]);

// Raydium AMM config — constrains fee tier / pool category
#[cfg(not(feature = "devnet"))]
pub const RAYDIUM_AMM_CONFIG: Pubkey = Pubkey::new_from_array([
    179, 33, 63, 186, 139, 249, 200, 127, 169, 30, 71, 129, 150, 40, 195, 131,
    224, 11, 234, 126, 152, 199, 160, 62, 3, 186, 16, 105, 207, 195, 246, 243,
]);
#[cfg(feature = "devnet")]
pub const RAYDIUM_AMM_CONFIG: Pubkey = Pubkey::new_from_array([
    133, 148, 254, 76, 78, 52, 206, 247, 143, 191, 153, 193, 196, 159, 191, 131,
    75, 191, 127, 200, 157, 54, 17, 92, 40, 71, 106, 78, 131, 72, 250, 241,
]);

// Wrapped SOL mint
pub const WSOL_MINT: Pubkey = Pubkey::new_from_array([
    6, 155, 136, 87, 254, 171, 129, 132, 251, 104, 127, 99, 70, 24, 192, 53,
    218, 196, 57, 220, 26, 235, 59, 85, 152, 160, 240, 0, 0, 0, 0, 1,
]);

// Read a Pubkey from raw account data at a given offset.
fn read_pubkey_at(data: &[u8], offset: usize) -> Result<Pubkey> {
    require!(
        data.len() >= offset + 32,
        TorchPerpError::InvalidPool
    );
    Ok(Pubkey::new_from_array(
        data[offset..offset + 32].try_into().unwrap(),
    ))
}

// Read a SPL TokenAccount's balance (raw). Layout: mint (32) + owner (32) + amount (8).
fn read_token_account_balance(account: &AccountInfo) -> Result<u64> {
    let data = account.try_borrow_data()?;
    require!(data.len() >= 72, TorchPerpError::InvalidPool);
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

// Validate pool + vaults belong to a Raydium CPMM pool for `expected_mint` paired
// with WSOL, and return (pool_sol, pool_tokens, wsol_is_token_0).
//
// Raydium CPMM PoolState layout (after 8-byte discriminator):
//   amm_config:   Pubkey @ 8
//   pool_creator: Pubkey @ 40
//   token_0_vault: Pubkey @ 72
//   token_1_vault: Pubkey @ 104
//   lp_mint:       Pubkey @ 136
//   token_0_mint:  Pubkey @ 168
//   token_1_mint:  Pubkey @ 200
pub fn read_raydium_pool_reserves(
    pool_state: &AccountInfo,
    vault_0: &AccountInfo,
    vault_1: &AccountInfo,
    expected_mint: &Pubkey,
) -> Result<(u64, u64, bool)> {
    require!(
        *pool_state.owner == RAYDIUM_CPMM_PROGRAM_ID,
        TorchPerpError::InvalidPool
    );

    let wsol_is_0 = {
        let data = pool_state.try_borrow_data()?;

        let stored_amm_config = read_pubkey_at(&data, 8)?;
        require!(
            stored_amm_config == RAYDIUM_AMM_CONFIG,
            TorchPerpError::InvalidPool
        );

        let stored_vault_0 = read_pubkey_at(&data, 72)?;
        let stored_vault_1 = read_pubkey_at(&data, 104)?;
        require!(
            vault_0.key() == stored_vault_0,
            TorchPerpError::PoolMismatch
        );
        require!(
            vault_1.key() == stored_vault_1,
            TorchPerpError::PoolMismatch
        );

        let mint_0 = read_pubkey_at(&data, 168)?;
        let mint_1 = read_pubkey_at(&data, 200)?;
        let has_token = mint_0 == *expected_mint || mint_1 == *expected_mint;
        let has_wsol = mint_0 == WSOL_MINT || mint_1 == WSOL_MINT;
        require!(has_token && has_wsol, TorchPerpError::InvalidPool);

        mint_0 == WSOL_MINT
    };

    let vault_0_bal = read_token_account_balance(vault_0)?;
    let vault_1_bal = read_token_account_balance(vault_1)?;
    let (pool_sol, pool_tokens) = if wsol_is_0 {
        (vault_0_bal, vault_1_bal)
    } else {
        (vault_1_bal, vault_0_bal)
    };
    require!(
        pool_sol > 0 && pool_tokens > 0,
        TorchPerpError::InvalidPool
    );
    Ok((pool_sol, pool_tokens, wsol_is_0))
}

// Same as above but for subsequent operations — assumes market already stores
// validated pool + vault pubkeys; verifies the accounts passed match.
pub fn verify_and_read_reserves(
    pool_state: &AccountInfo,
    vault_0: &AccountInfo,
    vault_1: &AccountInfo,
    expected_pool: &Pubkey,
    expected_vault_0: &Pubkey,
    expected_vault_1: &Pubkey,
    is_wsol_token_0: bool,
) -> Result<(u64, u64)> {
    require!(pool_state.key() == *expected_pool, TorchPerpError::PoolMismatch);
    require!(vault_0.key() == *expected_vault_0, TorchPerpError::PoolMismatch);
    require!(vault_1.key() == *expected_vault_1, TorchPerpError::PoolMismatch);

    let vault_0_bal = read_token_account_balance(vault_0)?;
    let vault_1_bal = read_token_account_balance(vault_1)?;
    let (pool_sol, pool_tokens) = if is_wsol_token_0 {
        (vault_0_bal, vault_1_bal)
    } else {
        (vault_1_bal, vault_0_bal)
    };
    require!(
        pool_sol > 0 && pool_tokens > 0,
        TorchPerpError::InvalidPool
    );
    Ok((pool_sol, pool_tokens))
}
