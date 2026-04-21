# torch perp

## an on chain perpetuals market utilizing torch market

**NOT LIVE ON DEVNET/MAINNET** tested on surfpool solana mainnet fork.

ProgramID: 852yvbSWFCyVLRo8bWUPTiouM5amtw6JxctgS9P4ymdH

- read the [design](./docs/design.md).
- 41/41 passing kani proofs in [verification](./docs/verification.md).
- internal [audit](./docs/audit.md).
- develop on torch_perp and use the test suite with the [sdk](./packages/sdk/readme.md).

```bash
anchor build
cargo kani
```

## run the sim

```bash
python3 sim/torch_perp_sim.py
```

Brightside Solutions, 2026
