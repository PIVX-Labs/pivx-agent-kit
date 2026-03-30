# PIVX Agent Kit

A lightweight CLI and MCP server that gives AI agents their own cryptocurrency wallet on the [PIVX](https://pivx.org) blockchain.

Built in pure Rust. Supports both **transparent** (public) and **SHIELD** (private, zkSNARK) transactions from a single seed phrase.

## Why this exists

AI agents are becoming economic actors — they need to send, receive, and hold value. Existing PIVX wallet options don't fit:

- **PIVX Core** is a full node. It syncs the entire blockchain (~20 GB), requires hours of setup, and runs a persistent daemon. Agents need something they can call and get a JSON answer.
- **MyPIVXWallet** runs in a browser with a JavaScript UI. Agents can access browsers through MCPs, but driving a visual wallet through browser automation is slow, fragile, and wasteful for a text-based LLM.

PIVX Agent Kit is purpose-built for agents: a single binary, structured JSON output, no GUI, no daemon, no full chain sync. A new wallet syncs in seconds using checkpoint fast-path, and every command returns machine-readable output that agents can parse directly.

**Dual balance model:** One seed phrase derives both a transparent (public, `D...`) address and a shield (private, `ps1...`) address. You choose which balance to spend from.

## Install

**Claude Code:**

```bash
curl -sSf https://install.pivx.ai | sh
claude mcp add --scope user pivx pivx-agent-kit serve
```

The first line installs the binary. The second registers it as an MCP server available across all projects. Restart your session and the PIVX tools appear natively.

**Other MCP-compatible agents** (OpenCode, Cursor, Cline, etc.) — install the binary, then add this to your MCP configuration:

```json
{
  "mcpServers": {
    "pivx": {
      "command": "pivx-agent-kit",
      "args": ["serve"]
    }
  }
}
```

**CLI only** (no MCP) — just install the binary and use the commands directly.

Pre-built binaries for Linux (x86_64, aarch64), macOS (Intel, Apple Silicon), and Windows are available on the [releases page](https://github.com/PIVX-Labs/pivx-agent-kit/releases).

## For agents

Once installed, you have native tools for wallet management. Via MCP, they appear as `pivx_init`, `pivx_balance`, `pivx_send`, etc. Via CLI, the same operations are available as commands.

**MCP example:**
```
tool: pivx_balance
→ { "private_balance": 30.0, "public_balance": 1.5, "total_balance": 31.5 }

tool: pivx_send { "address": "D...", "amount": "0.5", "from": "private" }
→ { "status": "sent", "txid": "6f3d...", "from": "private", "amount": 0.5, "fee": 0.02365 }
```

**CLI example:**
```
$ pivx-agent-kit balance
{ "private_balance": 30.0, "public_balance": 1.5, "total_balance": 31.5 }

$ pivx-agent-kit send D... 0.5 --from public
{ "status": "sent", "txid": "a1b2...", "from": "public", "amount": 0.5, "fee": 0.0000228 }
```

**Best practices:**
- Both `balance` and `send` auto-sync to the chain tip before executing. No need to sync manually.
- The `messages` field in `balance` output contains encrypted memos attached to received shield funds.
- Use `--from private` to spend from the shield balance, `--from public` for the transparent balance.
- Amounts are exact — use decimal strings like `0.1`, not floats. Parsed with integer precision.
- Memos can be up to 512 bytes of UTF-8 text (private-to-private only).
- Your seed phrase is stored securely and is never output by any command except `export`.

## Commands

```
pivx-agent-kit init                                          Create a new wallet
pivx-agent-kit import <word1 word2 ... word24>               Import wallet from seed phrase
pivx-agent-kit address                                       Show shield + transparent addresses
pivx-agent-kit balance                                       Sync and show private + public balances
pivx-agent-kit send <address> <amount> --from <private|public> [memo]
                                                             Send PIV from specified balance
pivx-agent-kit resync                                        Reset and re-sync from checkpoint
pivx-agent-kit export                                        Export wallet seed phrase for migration
pivx-agent-kit serve                                         Run as MCP server
pivx-agent-kit update                                        Update to latest release
```

All commands output JSON to stdout. Status/progress goes to stderr. Errors return JSON to stderr with exit code 1.

### Transaction types

All four directions are supported:

| From | To | Method |
|------|----|--------|
| Private (Shield) | Shield address | `--from private` |
| Private (Shield) | Transparent address | `--from private` |
| Public (Transparent) | Transparent address | `--from public` |
| Public (Transparent) | Shield address | `--from public` (shielding) |

## Building from source

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
- `wallet.json` — sync state, viewing key, encrypted notes + UTXOs (chmod 600 on Unix)
- `params/` — cached Sapling proving parameters

The seed and mnemonic in the wallet file are encrypted with a device-bound key — they cannot be read on any other machine. The extended spending key is not stored — it is derived from the seed on-the-fly when needed and zeroized from memory immediately after. To migrate a wallet to another device, use `export` to retrieve the seed phrase, then `import` on the new device.

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

## Architecture

```
pivx-agent-kit
├── main.rs            CLI entry, command dispatch
├── core.rs            Shared wallet operations (used by both CLI and MCP)
├── mcp.rs             MCP server (JSON-RPC over stdin/stdout)
├── wallet.rs          Wallet state, creation, persistence, device encryption
├── keys.rs            Key derivation (Sapling + BIP32 transparent)
├── shield.rs          Block processing, note decryption, shield + transparent tx building
├── sync.rs            Binary shield stream parser, transparent UTXO sync
├── network.rs         HTTP client for PIVX RPC and Blockbook APIs
├── prover.rs          Sapling proving parameter management
├── checkpoint.rs      Pre-computed commitment tree checkpoints
└── simd/hex.rs        SIMD-accelerated hex encoding/decoding
```

The cryptographic core uses [librustpivx](https://github.com/Duddino/librustpivx) (PIVX's fork of the Zcash Sapling libraries) compiled natively — no WebAssembly, no JavaScript, no async runtime.

## License

MIT
