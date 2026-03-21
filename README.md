# PIVX Agent Kit

A lightweight CLI that gives AI agents their own shielded cryptocurrency wallet on the [PIVX](https://pivx.org) blockchain.

Built in pure Rust. Full zkSNARK privacy via the SHIELD (Sapling) protocol.

## Why this exists

AI agents are becoming economic actors — they need to send, receive, and hold value. Existing PIVX wallet options don't fit:

- **PIVX Core** is a full node. It syncs the entire blockchain (~20 GB), requires hours of setup, and runs a persistent daemon. Agents need something they can call and get a JSON answer.
- **MyPIVXWallet** runs in a browser with a JavaScript UI. Agents don't have browsers.

PIVX Agent Kit is purpose-built for agents: a single binary, structured JSON output, no GUI, no daemon, no full chain sync. A new wallet syncs in seconds using checkpoint fast-path, and every command returns machine-readable output that agents can parse directly.

All transactions use SHIELD — PIVX's zero-knowledge privacy protocol. Balances, amounts, and memo contents are encrypted on-chain and only visible to the wallet holder.

## For agents

You can create a wallet, check your balance, send PIV, and read encrypted memos — all through simple CLI commands that return JSON.

```
$ pivx-agent-kit init
{
  "status": "created",
  "address": "ps1...",
  "birthday_height": 5236346
}

$ pivx-agent-kit balance
{
  "address": "ps1...",
  "balance": 2.47635,
  "balance_sat": 247635000,
  "unspent_notes": 2,
  "messages": [
    { "memo": "Hello from a human!", "amount": 1.0 }
  ]
}

$ pivx-agent-kit send ps1... 0.5 "Thanks for the PIV!"
{
  "status": "sent",
  "txid": "6f3d...",
  "amount": 0.5,
  "fee": 0.02365
}
```

**Best practices:**
- Both `balance` and `send` auto-sync to the chain tip before executing. No need to sync manually.
- The `messages` field in `balance` output contains encrypted memos attached to received funds. Check it to read communications from humans or other agents.
- Amounts are exact — use decimal strings like `0.1`, not floats. The CLI parses them with integer precision (no floating-point rounding).
- Memos can be up to 512 bytes of UTF-8 text (emoji and unicode work). Use them for payment references, instructions, or communication.
- If the commitment tree becomes corrupted, it is detected and repaired automatically during sync. You can also run `resync` to manually force a full re-sync from checkpoint.
- Your seed phrase is stored securely in the data directory and is never output by any command. The spending key is derived on-the-fly when needed and zeroized from memory after use.

## Commands

```
pivx-agent-kit init                              Create a new shielded wallet
pivx-agent-kit import <word1 word2 ... word24>   Import wallet from seed phrase
pivx-agent-kit address                           Show the shield receiving address
pivx-agent-kit balance                           Sync and show wallet balance
pivx-agent-kit send <address> <amount> [memo]    Send PIV to an address
pivx-agent-kit resync                            Reset and re-sync shield data from checkpoint
pivx-agent-kit export                            Export wallet seed phrase for migration
```

All commands output JSON to stdout. Status/progress goes to stderr. Errors return JSON to stderr with exit code 1.

## Building

Requires Rust 1.70+.

```bash
git clone https://github.com/PIVX-Labs/pivx-agent-kit
cd pivx-agent-kit
cargo build --release
```

The binary is at `target/release/pivx-agent-kit`.

The first time you run `send`, the Sapling proving parameters (~50 MB) are downloaded and cached in the data directory. Subsequent sends load them from disk instantly.

## Data directory

| Platform | Location |
|----------|----------|
| macOS    | `~/Library/Application Support/pivx-agent-kit/` |
| Linux    | `~/.local/share/pivx-agent-kit/` |
| Windows  | `%APPDATA%/pivx-agent-kit/` |

**Files:**
- `wallet.json` — sync state, viewing key, and encrypted notes (chmod 600 on Unix)
- `params/` — cached Sapling proving parameters

The seed and mnemonic in the wallet file are encrypted with a device-bound key — they cannot be read on any other machine. The extended spending key is not stored — it is derived from the seed on-the-fly when needed and zeroized from memory immediately after. To migrate a wallet to another device, use `import` with the mnemonic phrase.

## Security model

**Key protection:**
- **Device-bound encryption** — the seed and mnemonic are encrypted on disk using a key derived from the machine's unique hardware ID and data directory path. The wallet file is useless if copied to another device, leaked via cloud backup, or extracted from a disk image.
- **Seed isolation** — the seed and mnemonic are never exposed through normal CLI output. The `export` command exists for wallet migration but requires explicit confirmation and presents a safety warning designed to resist prompt injection attacks.
- **No spending key at rest** — the extended spending key (`extsk`) is not stored. It is derived from the seed in memory only during `send`, then zeroized.
- **Memory zeroization** — all sensitive key material is overwritten with zeroes when dropped, preventing extraction from core dumps or memory scanners.

**Data integrity:**
- **Atomic saves** — wallet state is written to a temp file then renamed, preventing corruption from crashes mid-write.
- **Sapling root validation** — after every sync, the local commitment tree root is compared against the network to detect corruption.
- **Auto-healing** — if corruption is detected, the wallet automatically resets to checkpoint and re-syncs without manual intervention.
- **Checkpoint recovery** — `resync` resets to the wallet's birthday checkpoint and rebuilds all state from the blockchain.

**Threat model — what this protects against:**
- Cloud backup leaks (file is encrypted with a device-specific key)
- Disk image extraction or VM snapshot cloning
- Agent accidentally reading or outputting the wallet file
- Process crashes leaking keys from memory
- Corrupted sync state from network issues or disk errors

**What this does NOT protect against:**
- An attacker with shell access on the same device (they can run the binary directly)
- Physical access to an unlocked machine

## Architecture

```
pivx-agent-kit
├── main.rs            CLI entry, command dispatch, amount parsing
├── wallet.rs          Wallet state, creation, persistence, zeroization
├── keys.rs            Sapling key derivation and encoding
├── shield.rs          Block processing, note decryption, transaction building
├── sync.rs            Binary shield stream parser, sync orchestration
├── network.rs         HTTP client for PIVX RPC and Blockbook APIs
├── prover.rs          Sapling proving parameter management
├── checkpoint.rs      Pre-computed commitment tree checkpoints
└── simd/hex.rs        SIMD-accelerated hex encoding/decoding
```

The cryptographic core uses [librustpivx](https://github.com/Duddino/librustpivx) (PIVX's fork of the Zcash Sapling libraries) compiled natively — no WebAssembly, no JavaScript, no async runtime.

## License

MIT
