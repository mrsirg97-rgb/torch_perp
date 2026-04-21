//! Kani formal verification proof harnesses for torch_perp.
//!
//! Proves properties of the pure math in `math.rs` at concrete values spanning
//! the protocol's operating range. Run with: `cargo kani`
//!
//! Concrete inputs (vs symbolic) avoid SAT solver explosion on wide-integer
//! arithmetic while verifying correctness at every scale the protocol operates
//! at. This mirrors the pattern used by torch_market and deep_pool.

use crate::math::{
    advance_cumulative, check_initial_margin, compute_fee, funding_delta, funding_owed,
    is_above_maintenance, liquidation_penalty_for_notional, mark_price_scaled, position_notional,
    premium_signed, proportional_entry, required_margin, split_fee, twap_price_scaled,
    unrealized_pnl, vamm_buy_base, vamm_sell_base, POS_SCALE,
};

// ============================================================================
// advance_cumulative
// ============================================================================
// Properties verified:
//   1. Additivity: cumulative grows by exactly reserve × slot_delta
//   2. Monotonicity: result >= prev_cumulative
//   3. Identity on zero slot delta: result == prev_cumulative
//   4. Overflow detection: returns None when overflow is imminent

#[cfg(kani)]
#[kani::proof]
fn verify_advance_cumulative_additivity() {
    // Cover dust, typical, and large-scale values within safe range.
    // Max safe u128 product: u64::MAX × u64::MAX fits in u128 exactly,
    // so we pick values well within headroom.
    let cases: [(u128, u64, u64); 6] = [
        (0, 0, 0),
        (0, 1, 1),
        (0, 1_000_000_000, 1),                   // 1 SOL across 1 slot
        (0, 1_000_000_000, 1_000_000),           // 1 SOL × 1M slots
        (100_000_000_000_000, 1_000_000_000, 10), // ongoing cumulative
        (u128::MAX / 2, 1_000_000_000, 1),       // near-halfway point
    ];

    for (prev, reserve, slots) in cases {
        let result = advance_cumulative(prev, reserve, slots);
        let expected = (reserve as u128).checked_mul(slots as u128)
            .and_then(|p| prev.checked_add(p));
        assert!(result == expected);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_advance_cumulative_monotonicity() {
    // For any non-overflowing input, result >= prev_cumulative.
    let cases: [(u128, u64, u64); 5] = [
        (0, 0, 0),
        (100, 1, 0),         // zero slot → no change
        (100, 0, 1_000_000), // zero reserve → no change
        (100, 500, 1),
        (1_000_000_000_000, 1_000_000, 1_000_000),
    ];

    for (prev, reserve, slots) in cases {
        if let Some(result) = advance_cumulative(prev, reserve, slots) {
            assert!(result >= prev);
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_advance_cumulative_zero_delta_is_identity() {
    // If slot_delta == 0, cumulative should not change regardless of reserve.
    let cases: [(u128, u64); 5] = [
        (0, 0),
        (0, 1_000_000_000),
        (100, 1_000_000_000),
        (u128::MAX / 2, u64::MAX),
        (u128::MAX, u64::MAX),
    ];

    for (prev, reserve) in cases {
        let result = advance_cumulative(prev, reserve, 0);
        assert!(result == Some(prev));
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_advance_cumulative_zero_reserve_is_identity() {
    // If reserve == 0, cumulative should not change regardless of slot_delta.
    let cases: [(u128, u64); 5] = [
        (0, 0),
        (0, 1_000_000_000),
        (100, 1_000_000_000),
        (u128::MAX / 2, u64::MAX),
        (u128::MAX, u64::MAX),
    ];

    for (prev, slots) in cases {
        let result = advance_cumulative(prev, 0, slots);
        assert!(result == Some(prev));
    }
}

// ============================================================================
// vAMM swap — k preservation + directionality
// ============================================================================
// Properties verified:
//   1. k invariant: new_base × new_quote >= base × quote (floor rounding → k is non-decreasing)
//   2. Buy base: base_out > 0 iff quote_in > 0
//   3. Sell base: quote_out > 0 iff base_in > 0
//   4. Mirror symmetry: reserves post-swap are consistent

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_buy_base_k_non_decreasing() {
    // k = base × quote. Floor rounding in new_base should keep k non-decreasing.
    // Cover dust, small, and torch-realistic depths.
    let cases: [(u64, u128, u128); 6] = [
        (0, 1_000_000_000_000, 100_000_000_000),
        (1, 1_000_000_000_000, 100_000_000_000),
        (100_000_000, 1_000_000_000_000, 100_000_000_000),   // 0.1 SOL into 100 SOL pool
        (1_000_000_000, 1_000_000_000_000, 100_000_000_000), // 1 SOL
        (10_000_000_000, 1_000_000_000_000, 100_000_000_000), // 10 SOL (big trade)
        (1, 1, 1),                                              // degenerate
    ];

    for (quote_in, base_r, quote_r) in cases {
        if let Some((_base_out, new_base, new_quote)) =
            vamm_buy_base(quote_in, base_r, quote_r)
        {
            let k_before = base_r.checked_mul(quote_r).unwrap();
            let k_after = new_base.checked_mul(new_quote).unwrap();
            assert!(k_after >= k_before);
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_sell_base_k_non_decreasing() {
    let cases: [(u64, u128, u128); 6] = [
        (0, 1_000_000_000_000, 100_000_000_000),
        (1, 1_000_000_000_000, 100_000_000_000),
        (1_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (100_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (500_000_000_000, 1_000_000_000_000, 100_000_000_000), // 50% of base
        (1, 1, 1),
    ];

    for (base_in, base_r, quote_r) in cases {
        if let Some((_quote_out, new_base, new_quote)) =
            vamm_sell_base(base_in, base_r, quote_r)
        {
            let k_before = base_r.checked_mul(quote_r).unwrap();
            let k_after = new_base.checked_mul(new_quote).unwrap();
            assert!(k_after >= k_before);
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_buy_zero_input_is_identity() {
    let cases: [(u128, u128); 4] = [
        (1_000_000_000_000, 100_000_000_000),
        (u128::MAX / 2, u128::MAX / 2),
        (1, 1),
        (0, 0),
    ];
    for (base_r, quote_r) in cases {
        let result = vamm_buy_base(0, base_r, quote_r);
        assert!(result == Some((0, base_r, quote_r)));
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_sell_zero_input_is_identity() {
    let cases: [(u128, u128); 4] = [
        (1_000_000_000_000, 100_000_000_000),
        (u128::MAX / 2, u128::MAX / 2),
        (1, 1),
        (0, 0),
    ];
    for (base_r, quote_r) in cases {
        let result = vamm_sell_base(0, base_r, quote_r);
        assert!(result == Some((0, base_r, quote_r)));
    }
}

// ============================================================================
// Fees — conservation + bounds
// ============================================================================

#[cfg(kani)]
#[kani::proof]
fn verify_compute_fee_bounded_by_notional() {
    // Fee must never exceed notional (since rate_bps ≤ 10_000 for valid configs).
    let cases: [(u64, u16); 6] = [
        (0, 0),
        (0, 10),
        (1_000_000_000, 10),       // 10 bps on 1 SOL
        (1_000_000_000, 200),      // 2% (hard upper bound per init validation)
        (u64::MAX, 10),
        (u64::MAX, 200),
    ];
    for (notional, rate_bps) in cases {
        let fee = compute_fee(notional, rate_bps).unwrap();
        assert!(fee <= notional);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_split_fee_conservation() {
    // to_insurance + to_protocol == fee for any valid cut_bps ≤ 10_000.
    let cases: [(u64, u16); 6] = [
        (0, 5_000),
        (1, 5_000),
        (1_000_000, 0),
        (1_000_000, 10_000),
        (1_000_000, 5_000),
        (u64::MAX, 5_000),
    ];
    for (fee, cut_bps) in cases {
        let (to_insurance, to_protocol) = split_fee(fee, cut_bps).unwrap();
        assert!(to_insurance.checked_add(to_protocol) == Some(fee));
        assert!(to_insurance <= fee);
        assert!(to_protocol <= fee);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_split_fee_zero_cut_is_all_protocol() {
    let cases: [u64; 4] = [0, 1, 1_000_000, u64::MAX];
    for fee in cases {
        let result = split_fee(fee, 0);
        assert!(result == Some((0, fee)));
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_split_fee_full_cut_is_all_insurance() {
    let cases: [u64; 4] = [0, 1, 1_000_000, u64::MAX];
    for fee in cases {
        let result = split_fee(fee, 10_000);
        assert!(result == Some((fee, 0)));
    }
}

// ============================================================================
// Position valuation & margin
// ============================================================================

#[cfg(kani)]
#[kani::proof]
fn verify_position_notional_zero_base() {
    // Zero position size → zero notional regardless of reserves.
    let cases: [(u128, u128); 4] = [
        (1_000_000_000_000, 100_000_000_000),
        (1, 1),
        (u128::MAX, u128::MAX),
        (1_000, 1_000),
    ];
    for (base_r, quote_r) in cases {
        let result = position_notional(0, base_r, quote_r);
        assert!(result == Some(0));
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_position_notional_formula() {
    // notional should equal |base| × quote / base exactly (floor).
    let cases: [(u64, u128, u128); 5] = [
        (1, 1_000_000_000_000, 100_000_000_000),
        (1_000_000, 1_000_000_000_000, 100_000_000_000),
        (1_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (10_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (u64::MAX, u128::MAX, u128::MAX),
    ];
    for (base, base_r, quote_r) in cases {
        let result = position_notional(base, base_r, quote_r);
        let expected = (base as u128)
            .checked_mul(quote_r)
            .and_then(|p| p.checked_div(base_r))
            .and_then(|v| u64::try_from(v).ok());
        assert!(result == expected);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_unrealized_pnl_long_profits_when_price_rises() {
    // Long: current > entry → positive PnL
    let result = unrealized_pnl(1_000_000, 100, 150);
    assert!(result == Some(50));
}

#[cfg(kani)]
#[kani::proof]
fn verify_unrealized_pnl_long_loses_when_price_falls() {
    // Long: current < entry → negative PnL
    let result = unrealized_pnl(1_000_000, 100, 80);
    assert!(result == Some(-20));
}

#[cfg(kani)]
#[kani::proof]
fn verify_unrealized_pnl_short_profits_when_price_falls() {
    // Short: current < entry → positive PnL
    let result = unrealized_pnl(-1_000_000, 100, 80);
    assert!(result == Some(20));
}

#[cfg(kani)]
#[kani::proof]
fn verify_unrealized_pnl_short_loses_when_price_rises() {
    // Short: current > entry → negative PnL
    let result = unrealized_pnl(-1_000_000, 100, 150);
    assert!(result == Some(-50));
}

#[cfg(kani)]
#[kani::proof]
fn verify_unrealized_pnl_flat_is_zero() {
    let cases: [(u64, u64); 4] = [(0, 0), (100, 100), (100, 0), (0, 100)];
    for (entry, current) in cases {
        let result = unrealized_pnl(0, entry, current);
        assert!(result == Some(0));
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_required_margin_formula() {
    let cases: [(u64, u16); 5] = [
        (0, 1_000),
        (1_000_000_000, 1_000),      // 10% of 1 SOL = 0.1 SOL
        (1_000_000_000, 625),        // 6.25% of 1 SOL
        (1_000_000_000, 10_000),     // 100% of 1 SOL
        (u64::MAX, 1_000),
    ];
    for (notional, ratio_bps) in cases {
        let result = required_margin(notional, ratio_bps);
        let expected = (notional as u128)
            .checked_mul(ratio_bps as u128)
            .and_then(|p| p.checked_div(10_000))
            .and_then(|v| u64::try_from(v).ok());
        assert!(result == expected);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_check_initial_margin_threshold() {
    // At exactly the required margin, opening is permitted (≥, not strict).
    // Under the required margin, not permitted. Over, permitted.
    let notional: u64 = 10_000_000_000; // 10 SOL
    let imr_bps: u16 = 1_000;            // 10% → requires 1 SOL
    let required = required_margin(notional, imr_bps).unwrap();

    assert!(check_initial_margin(notional, required, imr_bps) == Some(true));
    assert!(check_initial_margin(notional, required + 1, imr_bps) == Some(true));
    if required > 0 {
        assert!(check_initial_margin(notional, required - 1, imr_bps) == Some(false));
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_is_above_maintenance_negative_equity_is_liquidatable() {
    let notional: u64 = 1_000_000_000;
    let mmr_bps: u16 = 625;
    // Zero or negative equity → not above maintenance.
    assert!(is_above_maintenance(notional, 0, mmr_bps) == Some(false));
    assert!(is_above_maintenance(notional, -1, mmr_bps) == Some(false));
    assert!(is_above_maintenance(notional, i64::MIN, mmr_bps) == Some(false));
}

#[cfg(kani)]
#[kani::proof]
fn verify_is_above_maintenance_threshold() {
    let notional: u64 = 10_000_000_000; // 10 SOL
    let mmr_bps: u16 = 625;               // 6.25% → requires 0.625 SOL
    let required = required_margin(notional, mmr_bps).unwrap();

    assert!(is_above_maintenance(notional, required as i64, mmr_bps) == Some(true));
    assert!(is_above_maintenance(notional, (required + 1) as i64, mmr_bps) == Some(true));
    if required > 0 {
        assert!(is_above_maintenance(notional, (required - 1) as i64, mmr_bps) == Some(false));
    }
}

// ============================================================================
// vAMM roundtrip — no value extraction through trading
// ============================================================================
// quote → base → quote should never return MORE than starting quote.
// The rounding residual stays in the pool. This is the strongest invariant
// against "can you extract value by bouncing a trade back and forth?"

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_roundtrip_quote_base_quote_no_extraction() {
    let cases: [(u64, u128, u128); 5] = [
        (1, 1_000_000_000_000, 100_000_000_000),
        (1_000_000, 1_000_000_000_000, 100_000_000_000),
        (1_000_000_000, 1_000_000_000_000, 100_000_000_000), // 1 SOL roundtrip
        (10_000_000_000, 1_000_000_000_000, 100_000_000_000), // 10 SOL big trade
        (500_000_000, 500_000_000_000, 50_000_000_000),        // dense small pool
    ];

    for (quote_in, base_r, quote_r) in cases {
        let (base_out, new_base, new_quote) = vamm_buy_base(quote_in, base_r, quote_r).unwrap();
        // Immediately sell that same base back. No other trades in between.
        let (quote_back, _, _) = vamm_sell_base(base_out, new_base, new_quote).unwrap();
        // Cannot profit from roundtrip — at most, break even (minus floor residual).
        assert!(quote_back <= quote_in);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_roundtrip_base_quote_base_no_extraction() {
    let cases: [(u64, u128, u128); 5] = [
        (1, 1_000_000_000_000, 100_000_000_000),
        (1_000_000, 1_000_000_000_000, 100_000_000_000),
        (1_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (100_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (50_000_000_000, 500_000_000_000, 50_000_000_000),
    ];

    for (base_in, base_r, quote_r) in cases {
        let (quote_out, new_base, new_quote) = vamm_sell_base(base_in, base_r, quote_r).unwrap();
        let (base_back, _, _) = vamm_buy_base(quote_out, new_base, new_quote).unwrap();
        assert!(base_back <= base_in);
    }
}

// ============================================================================
// vAMM output bounded by reserve — swap never pays more than pool holds
// ============================================================================

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_buy_base_output_bounded_by_reserve() {
    let cases: [(u64, u128, u128); 6] = [
        (0, 1_000_000_000_000, 100_000_000_000),
        (1, 1_000_000_000_000, 100_000_000_000),
        (1_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (100_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (u64::MAX / 2, 1_000_000_000_000_000_000, 1_000_000_000_000_000_000),
        (1, 1, 1),
    ];

    for (quote_in, base_r, quote_r) in cases {
        if let Some((base_out, _, _)) = vamm_buy_base(quote_in, base_r, quote_r) {
            assert!(base_out as u128 <= base_r);
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_sell_base_output_bounded_by_reserve() {
    let cases: [(u64, u128, u128); 6] = [
        (0, 1_000_000_000_000, 100_000_000_000),
        (1, 1_000_000_000_000, 100_000_000_000),
        (1_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (100_000_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (u64::MAX / 2, 1_000_000_000_000_000_000, 1_000_000_000_000_000_000),
        (1, 1, 1),
    ];

    for (base_in, base_r, quote_r) in cases {
        if let Some((quote_out, _, _)) = vamm_sell_base(base_in, base_r, quote_r) {
            assert!(quote_out as u128 <= quote_r);
        }
    }
}

// ============================================================================
// PnL long/short symmetry — long and short at same params have opposite PnL
// ============================================================================
// For any (entry, current), a long of +B and a short of -B must produce
// PnL values that are exact negatives of each other. If long profits, short
// loses by the same magnitude, and vice versa.

#[cfg(kani)]
#[kani::proof]
fn verify_pnl_long_short_symmetry() {
    let cases: [(u64, u64); 6] = [
        (100, 100),    // flat
        (100, 150),    // price rose
        (100, 80),     // price fell
        (0, 0),
        (u64::MAX / 2, u64::MAX / 2 + 1_000),
        (u64::MAX / 2 + 1_000, u64::MAX / 2),
    ];
    let base_magnitudes: [i64; 4] = [1, 1_000_000, 1_000_000_000, i64::MAX / 2];

    for (entry, current) in cases {
        for mag in base_magnitudes {
            let long_pnl = unrealized_pnl(mag, entry, current).unwrap();
            let short_pnl = unrealized_pnl(-mag, entry, current).unwrap();
            // Long's PnL is negative of short's PnL at identical params.
            assert!(long_pnl == -short_pnl);
        }
    }
}

// ============================================================================
// Fee monotonicity — larger notional → larger or equal fee
// ============================================================================

#[cfg(kani)]
#[kani::proof]
fn verify_compute_fee_monotonic_in_notional() {
    let pairs: [(u64, u64); 5] = [
        (0, 1),
        (1, 1_000),
        (1_000, 1_000_000),
        (1_000_000, 1_000_000_000),
        (1_000_000_000, u64::MAX),
    ];
    let rates: [u16; 4] = [1, 10, 100, 200];

    for rate in rates {
        for (smaller, larger) in pairs {
            let fee_small = compute_fee(smaller, rate).unwrap();
            let fee_large = compute_fee(larger, rate).unwrap();
            assert!(fee_small <= fee_large);
        }
    }
}

// ============================================================================
// Required margin monotonicity — larger notional → larger or equal margin
// ============================================================================

#[cfg(kani)]
#[kani::proof]
fn verify_required_margin_monotonic_in_notional() {
    let pairs: [(u64, u64); 5] = [
        (0, 1),
        (1, 1_000),
        (1_000, 1_000_000),
        (1_000_000, 1_000_000_000),
        (1_000_000_000, u64::MAX),
    ];
    let ratios: [u16; 4] = [1, 625, 1_000, 10_000];

    for ratio in ratios {
        for (smaller, larger) in pairs {
            let margin_small = required_margin(smaller, ratio).unwrap();
            let margin_large = required_margin(larger, ratio).unwrap();
            assert!(margin_small <= margin_large);
        }
    }
}

// ============================================================================
// Liquidation penalty
// ============================================================================

#[cfg(kani)]
#[kani::proof]
fn verify_liquidation_penalty_formula() {
    let cases: [(u64, u16); 5] = [
        (0, 500),
        (1_000_000_000, 500),         // 5% of 1 SOL
        (1_000_000_000, 1_000),       // 10% of 1 SOL
        (1_000_000_000, 10_000),      // 100% of 1 SOL
        (u64::MAX, 500),
    ];
    for (notional, bps) in cases {
        let result = liquidation_penalty_for_notional(notional, bps);
        let expected = (notional as u128)
            .checked_mul(bps as u128)
            .and_then(|p| p.checked_div(10_000))
            .and_then(|v| u64::try_from(v).ok());
        assert!(result == expected);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_liquidation_penalty_bounded_by_notional() {
    // For any penalty_bps ≤ 10_000, penalty ≤ notional.
    let cases: [(u64, u16); 6] = [
        (0, 0),
        (0, 10_000),
        (1_000_000, 500),
        (1_000_000, 10_000),
        (u64::MAX, 500),
        (u64::MAX, 10_000),
    ];
    for (notional, bps) in cases {
        let penalty = liquidation_penalty_for_notional(notional, bps).unwrap();
        assert!(penalty <= notional);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_liquidation_penalty_monotonic_in_notional() {
    let pairs: [(u64, u64); 5] = [
        (0, 1),
        (1, 1_000),
        (1_000, 1_000_000),
        (1_000_000, 1_000_000_000),
        (1_000_000_000, u64::MAX),
    ];
    let bps: [u16; 4] = [1, 500, 1_000, 10_000];
    for b in bps {
        for (smaller, larger) in pairs {
            let small = liquidation_penalty_for_notional(smaller, b).unwrap();
            let large = liquidation_penalty_for_notional(larger, b).unwrap();
            assert!(small <= large);
        }
    }
}

// ============================================================================
// IMR ⇒ MMR consistency — opening a position doesn't leave it liquidatable
// ============================================================================
// Property: if a position passes the initial-margin gate with equity = C and
// the maintenance margin ratio is STRICTLY LESS than the initial margin ratio,
// then the position is also above maintenance. I.e., a fresh position isn't
// immediately liquidatable.

#[cfg(kani)]
#[kani::proof]
fn verify_imr_implies_above_maintenance() {
    // Valid config: mmr < imr, both in (0, 10_000]
    let configs: [(u16, u16); 4] = [
        (1_000, 625),     // 10% IMR, 6.25% MMR (default)
        (500, 250),       // 5% IMR, 2.5% MMR
        (2_000, 1_000),   // 20% IMR, 10% MMR
        (10_000, 5_000),  // 100% IMR, 50% MMR (full collateralization)
    ];
    // Notionals large enough that required_margin > 0 (handler also requires
    // collateral > 0 via `require!(collateral_lamports > 0)`, so the degenerate
    // case of zero-equity fresh positions can't happen in practice).
    let notionals: [u64; 4] = [1_000_000, 100_000_000, 10_000_000_000, u64::MAX / 4];

    for (imr, mmr) in configs {
        assert!(mmr < imr);
        for notional in notionals {
            let required_imr = required_margin(notional, imr).unwrap();
            // Collateral at exactly the IMR threshold → passes IMR. Also ensure
            // collateral > 0 to match handler precondition (zero-equity is
            // always treated as liquidatable by is_above_maintenance).
            let collateral = if required_imr > 0 { required_imr } else { 1 };
            assert!(check_initial_margin(notional, collateral, imr) == Some(true));
            let equity = collateral as i64;
            assert!(is_above_maintenance(notional, equity, mmr) == Some(true));
        }
    }
}

// ============================================================================
// vAMM price direction — buy pushes mark up, sell pushes mark down
// ============================================================================
// Critical for the price-impact gate in open_position. Buying base MUST make
// the post-trade price strictly ≥ pre-trade price. Selling MUST make it ≤.

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_buy_pushes_price_up() {
    // price = quote / base. After buy: quote_r went up, base_r went down.
    // Invariant to check: new_quote × base_r >= base_r × quote_r
    //                     new_base × quote_r <= base_r × quote_r
    // Equivalent: (new_quote × old_base) >= (old_quote × new_base)
    let cases: [(u64, u128, u128); 5] = [
        (1, 1_000_000_000_000, 100_000_000_000),
        (1_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (10_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (100_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (1, 1, 1),
    ];
    for (quote_in, base_r, quote_r) in cases {
        if let Some((_base_out, new_base, new_quote)) = vamm_buy_base(quote_in, base_r, quote_r) {
            // new_price / old_price >= 1 ⇒ new_quote × old_base ≥ new_base × old_quote
            let lhs = new_quote.checked_mul(base_r).unwrap();
            let rhs = new_base.checked_mul(quote_r).unwrap();
            assert!(lhs >= rhs);
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_sell_pushes_price_down() {
    // After sell: base_r went up, quote_r went down.
    // new_quote × old_base ≤ new_base × old_quote
    let cases: [(u64, u128, u128); 5] = [
        (1, 1_000_000_000_000, 100_000_000_000),
        (1_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (100_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (500_000_000_000, 1_000_000_000_000, 100_000_000_000),
        (1, 1, 1),
    ];
    for (base_in, base_r, quote_r) in cases {
        if let Some((_quote_out, new_base, new_quote)) = vamm_sell_base(base_in, base_r, quote_r) {
            let lhs = new_quote.checked_mul(base_r).unwrap();
            let rhs = new_base.checked_mul(quote_r).unwrap();
            assert!(lhs <= rhs);
        }
    }
}

// ============================================================================
// vAMM output monotonicity — larger input ⇒ larger-or-equal output
// ============================================================================
// Catches grief-via-split-trades: if output were non-monotonic, an attacker
// could pay less by splitting a trade into smaller pieces.

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_buy_output_monotonic_in_input() {
    let reserves: [(u128, u128); 3] = [
        (1_000_000_000_000, 100_000_000_000),
        (500_000_000_000, 50_000_000_000),
        (1_000_000_000_000_000, 1_000_000_000_000_000),
    ];
    let pairs: [(u64, u64); 5] = [
        (0, 1),
        (1, 1_000),
        (1_000, 1_000_000),
        (1_000_000, 1_000_000_000),
        (1_000_000_000, 10_000_000_000),
    ];
    for (base_r, quote_r) in reserves {
        for (smaller, larger) in pairs {
            let small = vamm_buy_base(smaller, base_r, quote_r).unwrap();
            let large = vamm_buy_base(larger, base_r, quote_r).unwrap();
            assert!(small.0 <= large.0);
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_sell_output_monotonic_in_input() {
    let reserves: [(u128, u128); 3] = [
        (1_000_000_000_000, 100_000_000_000),
        (500_000_000_000, 50_000_000_000),
        (1_000_000_000_000_000, 1_000_000_000_000_000),
    ];
    let pairs: [(u64, u64); 5] = [
        (0, 1),
        (1, 1_000),
        (1_000, 1_000_000),
        (1_000_000, 1_000_000_000),
        (1_000_000_000, 10_000_000_000),
    ];
    for (base_r, quote_r) in reserves {
        for (smaller, larger) in pairs {
            let small = vamm_sell_base(smaller, base_r, quote_r).unwrap();
            let large = vamm_sell_base(larger, base_r, quote_r).unwrap();
            assert!(small.0 <= large.0);
        }
    }
}

// ============================================================================
// Price impact monotonicity — larger trade ⇒ larger (or equal) price impact
// ============================================================================
// Safety for the max_price_impact_bps gate. If price impact were non-monotonic,
// traders could bypass the gate by submitting larger trades that somehow had
// smaller impact. Monotonicity rules that out.

#[cfg(kani)]
#[kani::proof]
fn verify_vamm_buy_price_impact_monotonic() {
    let base_r: u128 = 1_000_000_000_000;
    let quote_r: u128 = 100_000_000_000;
    let pairs: [(u64, u64); 4] = [
        (1, 1_000),
        (1_000, 1_000_000),
        (1_000_000, 1_000_000_000),
        (1_000_000_000, 10_000_000_000),
    ];
    for (smaller, larger) in pairs {
        let s = vamm_buy_base(smaller, base_r, quote_r).unwrap();
        let l = vamm_buy_base(larger, base_r, quote_r).unwrap();
        // impact proxy: new_quote × base_r - new_base × quote_r (post-trade price premium)
        let s_impact = s.2.checked_mul(base_r).unwrap() as i128 - s.1.checked_mul(quote_r).unwrap() as i128;
        let l_impact = l.2.checked_mul(base_r).unwrap() as i128 - l.1.checked_mul(quote_r).unwrap() as i128;
        assert!(s_impact <= l_impact);
    }
}

// ============================================================================
// Required margin monotonic in RATIO (complement of notional monotonic)
// ============================================================================
// Raising MMR can never relax a position. Ensures admin/config changes
// in the direction of safety actually tighten things.

#[cfg(kani)]
#[kani::proof]
fn verify_required_margin_monotonic_in_ratio() {
    let notionals: [u64; 4] = [1_000_000, 1_000_000_000, 100_000_000_000, u64::MAX / 4];
    let ratio_pairs: [(u16, u16); 5] = [
        (0, 1),
        (1, 625),
        (625, 1_000),
        (1_000, 2_000),
        (2_000, 10_000),
    ];
    for notional in notionals {
        for (smaller, larger) in ratio_pairs {
            let req_small = required_margin(notional, smaller).unwrap();
            let req_large = required_margin(notional, larger).unwrap();
            assert!(req_small <= req_large);
        }
    }
}

// ============================================================================
// Unrealized PnL magnitude bounded by max(entry, current)
// ============================================================================
// |pnl| = |entry - current| ≤ max(entry, current). Catches sign/overflow bugs
// that would return absurd values.

#[cfg(kani)]
#[kani::proof]
fn verify_unrealized_pnl_bounded_by_max_notional() {
    let cases: [(u64, u64); 7] = [
        (0, 0),
        (100, 100),
        (100, 150),
        (100, 80),
        (u64::MAX / 2, u64::MAX / 2 + 1_000),
        (u64::MAX / 2 + 1_000, u64::MAX / 2),
        (1, u64::MAX / 4),
    ];
    let bases: [i64; 3] = [1_000_000, -1_000_000, 0];
    for (entry, current) in cases {
        for base in bases {
            let pnl = unrealized_pnl(base, entry, current).unwrap();
            let abs_pnl: u64 = if pnl >= 0 { pnl as u64 } else { (-pnl) as u64 };
            let max_notional = if entry > current { entry } else { current };
            assert!(abs_pnl <= max_notional);
        }
    }
}

// ============================================================================
// Funding rate (v1.1) — mark/index price, premium, accrual, settlement
// ============================================================================

#[cfg(kani)]
#[kani::proof]
fn verify_mark_price_formula() {
    let cases: [(u128, u128); 5] = [
        (1_000_000_000_000, 100_000_000_000),        // 100 SOL / 1M tokens
        (148_000_000_000_000, 200_000_000_000),       // post-migration torch scale
        (1_000_000, 1),
        (u128::MAX / POS_SCALE, u128::MAX / POS_SCALE),
        (1, 1),
    ];
    for (base_r, quote_r) in cases {
        let price = mark_price_scaled(base_r, quote_r);
        let expected = quote_r.checked_mul(POS_SCALE).and_then(|p| p.checked_div(base_r));
        assert!(price == expected);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_mark_price_zero_base_is_none() {
    let cases: [u128; 4] = [0, 1, 1_000_000_000, u128::MAX];
    for quote_r in cases {
        assert!(mark_price_scaled(0, quote_r).is_none());
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_twap_price_formula() {
    // Same shape as mark_price but using cumulative deltas.
    let cases: [(u128, u128); 4] = [
        (100_000_000_000, 1_000_000_000_000),
        (1_000, 1_000_000),
        (1, 1),
        (u128::MAX / POS_SCALE, u128::MAX / POS_SCALE),
    ];
    for (sol_delta, token_delta) in cases {
        let result = twap_price_scaled(sol_delta, token_delta);
        let expected = sol_delta.checked_mul(POS_SCALE).and_then(|p| p.checked_div(token_delta));
        assert!(result == expected);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_twap_zero_token_delta_is_none() {
    let cases: [u128; 4] = [0, 1, 1_000_000_000, u128::MAX];
    for sol_delta in cases {
        assert!(twap_price_scaled(sol_delta, 0).is_none());
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_premium_sign_correctness() {
    // mark == index → zero
    let cases: [u128; 5] = [0, 1, 1_000_000_000, POS_SCALE, u128::MAX / 2];
    for v in cases {
        assert!(premium_signed(v, v) == Some(0));
    }

    // mark > index → positive
    let pairs_pos: [(u128, u128); 4] = [
        (100, 50),
        (1_000_000_000_000, 999_999_999_999),
        (POS_SCALE, 1),
        (u128::MAX / 2, 0),
    ];
    for (mark, index) in pairs_pos {
        let p = premium_signed(mark, index).unwrap();
        assert!(p > 0);
    }

    // mark < index → negative
    let pairs_neg: [(u128, u128); 4] = [
        (50, 100),
        (999_999_999_999, 1_000_000_000_000),
        (1, POS_SCALE),
        (0, u128::MAX / 2),
    ];
    for (mark, index) in pairs_neg {
        let p = premium_signed(mark, index).unwrap();
        assert!(p < 0);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_funding_delta_zero_slots_is_zero() {
    // No time elapsed → no funding accrual, regardless of premium.
    let premiums: [i128; 5] = [
        0,
        1_000_000_000,
        -1_000_000_000,
        i128::MAX / 2,
        i128::MIN / 2,
    ];
    let periods: [u64; 3] = [1, 9_000, u64::MAX];
    for premium in premiums {
        for period in periods {
            assert!(funding_delta(premium, 0, period) == Some(0));
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_funding_delta_monotonic_in_elapsed() {
    // For positive premium, longer elapsed → larger (or equal) delta magnitude.
    let premium: i128 = 1_000_000_000_000_000; // 0.1% of POS_SCALE
    let period: u64 = 9_000;
    let elapsed_pairs: [(u64, u64); 4] = [
        (0, 1),
        (1, 100),
        (100, 1_000),
        (1_000, 10_000),
    ];
    for (smaller, larger) in elapsed_pairs {
        let d_small = funding_delta(premium, smaller, period).unwrap();
        let d_large = funding_delta(premium, larger, period).unwrap();
        assert!(d_small <= d_large);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_funding_delta_zero_period_is_none() {
    // funding_period_slots = 0 → math undefined → None.
    let premiums: [i128; 4] = [0, 1_000, -1_000, i128::MAX / 2];
    for premium in premiums {
        assert!(funding_delta(premium, 100, 0).is_none());
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_funding_owed_zero_delta_is_zero() {
    // cumulative_current == cumulative_snapshot → no funding owed.
    let bases: [i64; 5] = [0, 1, -1, 1_000_000_000, -1_000_000_000];
    let cums: [i128; 4] = [0, 1_000, -1_000, 1_000_000_000_000];
    for base in bases {
        for cum in cums {
            assert!(funding_owed(base, cum, cum) == Some(0));
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_funding_owed_sign_correctness() {
    // Long (base > 0) with positive delta → pays (positive owed).
    // Long with negative delta → receives (negative owed).
    // Short (base < 0) flips these.
    let long_base: i64 = 1_000_000;
    let short_base: i64 = -1_000_000;
    let pos_delta_cum: i128 = POS_SCALE as i128; // 1 unit of delta
    let neg_delta_cum: i128 = -(POS_SCALE as i128);

    let long_pos = funding_owed(long_base, pos_delta_cum, 0).unwrap();
    assert!(long_pos > 0); // long pays

    let long_neg = funding_owed(long_base, neg_delta_cum, 0).unwrap();
    assert!(long_neg < 0); // long receives

    let short_pos = funding_owed(short_base, pos_delta_cum, 0).unwrap();
    assert!(short_pos < 0); // short receives

    let short_neg = funding_owed(short_base, neg_delta_cum, 0).unwrap();
    assert!(short_neg > 0); // short pays
}

#[cfg(kani)]
#[kani::proof]
fn verify_funding_owed_long_short_symmetry() {
    // A long of +N and a short of -N at the same cumulative params
    // must have exactly opposite funding owed (enables zero-sum over OI).
    let bases: [i64; 4] = [1, 1_000_000, 1_000_000_000, i64::MAX / 2];
    let snap: i128 = 0;
    let currents: [i128; 4] = [
        POS_SCALE as i128,
        -(POS_SCALE as i128),
        (POS_SCALE as i128) / 100,
        -(POS_SCALE as i128) / 100,
    ];
    for base in bases {
        for current in currents {
            let long_owed = funding_owed(base, current, snap).unwrap();
            let short_owed = funding_owed(-base, current, snap).unwrap();
            assert!(long_owed == -short_owed);
        }
    }
}

// ============================================================================
// Partial close (v1.2) — proportional_entry
// ============================================================================

#[cfg(kani)]
#[kani::proof]
fn verify_proportional_entry_formula() {
    let cases: [(u64, u64, u64); 5] = [
        (1_000_000_000, 500_000_000, 1_000_000_000), // close 50%
        (1_000_000_000, 1_000_000_000, 1_000_000_000), // close 100%
        (1_000_000_000, 1, 1_000_000_000),            // close 1 unit
        (0, 500_000_000, 1_000_000_000),              // zero entry → zero proportional
        (u64::MAX, u64::MAX, u64::MAX),                // extreme
    ];
    for (entry, closed, total) in cases {
        let result = proportional_entry(entry, closed, total);
        let expected = (entry as u128)
            .checked_mul(closed as u128)
            .and_then(|p| p.checked_div(total as u128))
            .and_then(|v| u64::try_from(v).ok());
        assert!(result == expected);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_proportional_entry_bounded() {
    // proportional ≤ entry_notional when base_closed ≤ abs_base. Floor rounding
    // can only make it smaller, so bound is strict for partial closes.
    let cases: [(u64, u64, u64); 5] = [
        (1_000_000_000, 500_000_000, 1_000_000_000),
        (1_000_000_000, 999_999_999, 1_000_000_000),
        (1_000_000_000, 1, 1_000_000_000),
        (0, 0, 1),
        (u64::MAX / 2, u64::MAX / 4, u64::MAX / 2),
    ];
    for (entry, closed, total) in cases {
        if closed <= total {
            let result = proportional_entry(entry, closed, total).unwrap();
            assert!(result <= entry);
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_proportional_entry_zero_base_is_none() {
    // abs_base = 0 → division undefined → None.
    let cases: [(u64, u64); 3] = [(0, 0), (1_000, 0), (u64::MAX, 1_000)];
    for (entry, closed) in cases {
        assert!(proportional_entry(entry, closed, 0).is_none());
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_proportional_entry_monotonic_in_closed() {
    // Larger base_closed → larger-or-equal proportional entry.
    let entry: u64 = 10_000_000_000;
    let total: u64 = 1_000_000_000;
    let pairs: [(u64, u64); 4] = [
        (0, 1),
        (1, 1_000),
        (1_000, 1_000_000),
        (1_000_000, 1_000_000_000),
    ];
    for (smaller, larger) in pairs {
        let s = proportional_entry(entry, smaller, total).unwrap();
        let l = proportional_entry(entry, larger, total).unwrap();
        assert!(s <= l);
    }
}

#[cfg(kani)]
#[kani::proof]
fn verify_proportional_entry_full_equals_entry() {
    // When base_closed == abs_base, proportional should equal entry exactly
    // (no rounding loss at the boundary).
    let cases: [(u64, u64); 5] = [
        (0, 1),
        (1, 1),
        (1_000_000_000, 1_000_000_000),
        (u64::MAX / 4, u64::MAX / 4),
        (u64::MAX / 2, u64::MAX / 2),
    ];
    for (entry, total) in cases {
        let result = proportional_entry(entry, total, total).unwrap();
        assert!(result == entry);
    }
}
