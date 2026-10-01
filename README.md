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

Once installed, you have native tools for both wallet management and the [PIVX Tasks](https://tasks.pivxla.bz) bounty platform. Via MCP, they appear as `pivx_init`, `pivx_balance`, `pivx_send`, `pivx_task_list`, `pivx_task_submit`, `pivx_task_approve`, etc. Via CLI, the same operations are available as commands.

**Wallet — MCP example:**
```
tool: pivx_balance
→ { "private_balance": 30.0, "public_balance": 1.5, "total_balance": 31.5,
    "private_balance_usd": "2.07", "public_balance_usd": "0.10", "total_balance_usd": "2.17", "price_usd": "0.06888275" }

tool: pivx_send { "address": "D...", "amount_usd": "5", "from": "public", "reason": "Pay for API credits" }
→ { "status": "sent", "txid": "...", "amount": 72.58711361, "amount_usd": "5.00", "price_usd": "0.06888275", "price_source": "oracle", ... }

tool: pivx_send { "address": "D...", "amount": "0.5", "from": "private", "reason": "Tip for logo feedback" }
→ { "status": "sent", "txid": "6f3d...", "from": "private", "amount": 0.5, "fee": 0.02365, "reason": "Tip for logo feedback", "id": "a1b2c3d4e5f60718" }

tool: pivx_history { "limit": 1 }
→ { "count": 1, "sends": [ { "time": "2026-10-01T09:20:00Z", "status": "broadcast", "amount": 0.5,
      "to": "D...", "reason": "Tip for logo feedback", "txid": "6f3d...", ... } ],
    "totals": { "last_24h": 0.52365, "last_24h_sat": 52365000 } }
```

**Tasks platform — MCP example:**
```
tool: pivx_task_list { "status": "open", "limit": 5 }
→ [ { "id": 12, "title": "...", "bounty_sat": 100000, ... }, ... ]

tool: pivx_task_signup { "id_or_url": "12" }
→ { "id": 12, "status": "in_progress", "slots": { ... } }

tool: pivx_task_submit { "id_or_url": "12", "body": "Done — see attached.", "files": ["/tmp/proof.png"] }
→ { "commitment_id": 7, "proof_id": 7, "attachments": 1 }

tool: pivx_task_approve { "id_or_url": "12", "worker": "frosted-otter-417" }
→ { "id": 12, "status": "submitted", "txid": "84b00...", "slots": { ... } }
```

**History and limits tools:** `pivx_history` (optional `limit`, `since`), `pivx_policy` and `pivx_pending_list` (read-only).

**Available task tools** (full surface mirrors the CLI — see "PIVX Tasks platform" under Commands):
`pivx_task_list`, `pivx_task_search`, `pivx_task_get`, `pivx_task_profile`, `pivx_task_signup`, `pivx_task_submit`, `pivx_task_create`, `pivx_task_approve`, `pivx_task_reject`, `pivx_task_cancel`, `pivx_task_notifications`, `pivx_task_notification_read`, `pivx_task_notification_read_all`, `pivx_task_notification_dismiss`.

**CLI example:**
```
$ pivx-agent-kit balance
{ "private_balance": 30.0, "public_balance": 1.5, "total_balance": 31.5 }

$ pivx-agent-kit send D... 0.5 --from public --reason "Pay for API credits"
{ "status": "sent", "txid": "a1b2...", "from": "public", "amount": 0.5, "fee": 0.0000228, ... }

$ pivx-agent-kit send D... 5usd --from public      # also '$5' (single quotes!), "5 USD", 5 --usd
{ "status": "sent", "amount": 72.58711361, "amount_usd": "5.00", "price_usd": "0.06888275", ... }

$ pivx-agent-kit history --since 2026-09-01 --csv > september.csv
```

**Best practices:**
- Both `balance` and `send` auto-sync to the chain tip before executing. No need to sync manually.
- The `messages` field in `balance` output contains encrypted memos attached to received shield funds.
- Use `--from private` to spend from the shield balance, `--from public` for the transparent balance. Anything else is an error.
- Give every payment a `reason`. It's stored in the spend history your operator reviews.
- Before retrying a payment that errored, check `pivx_history`: it may have gone through.
- Amounts are exact — use decimal strings like `0.1`, not floats. Parsed with integer precision.
- Memos can be up to 512 bytes of UTF-8 text (private-to-private only).
- Your seed phrase is stored securely and is never output by any command except `export`.

## Commands

```
pivx-agent-kit init                                          Create a new wallet
pivx-agent-kit import <word1 word2 ... word24>               Import wallet from seed phrase
pivx-agent-kit address                                       Show shield + transparent addresses
pivx-agent-kit balance                                       Sync and show private + public balances
pivx-agent-kit send <address> <amount> [--usd] [--from private|public] [--reason <text>] [memo]
                                                             Send PIV or a dollar amount (default: from private)
pivx-agent-kit history [--limit N] [--since <ts|YYYY-MM-DD>] [--csv]
                                                             Payments sent, newest first
pivx-agent-kit resync                                        Reset and re-sync from checkpoint
pivx-agent-kit export                                        Export wallet seed phrase for migration
pivx-agent-kit serve                                         Run as MCP server
pivx-agent-kit update                                        Update to latest release
```

All commands output JSON to stdout. Status/progress goes to stderr. Errors return JSON to stderr with exit code 1.

### Dollar amounts

`balance` includes the USD value of each balance. `send` accepts dollar amounts: `5usd`, `5 USD`, `5 --usd`, or `'$5'` on the CLI, and `amount_usd` over MCP. Quote `'$5'` with single quotes: in double quotes or unquoted, the shell expands `$5` and the kit receives a plain PIV number. Each payment records the price it used, so `history` shows what it was worth in dollars at the time.

Prices come from the PIVX Labs oracle (`pivxla.bz/oracle`) and CoinGecko. `PIVX_PRICE_API` replaces the oracle URL; prices from it are labelled `custom`. A dollar send converts at a price no more than a minute old, and is refused when:
- the source price is more than 30 minutes stale or dated in the future;
- the two sources answer and disagree by more than 10%;
- there's no trusted reference from the last hour and the two sources don't both answer;
- the price is more than 25% away from that hour's reference. The reference doesn't follow the price around, so a feed can't walk it down step by step.

A broken price feed would otherwise turn "$5" into far more PIV. In practice, dollar sends (and dollar spending limits) need both sources up at least once an hour; a CoinGecko rate limit at the wrong moment means "retry later" or "send in PIV". Accepted prices are cached for a minute and signed with a device-bound key, so a hand-edited cache is ignored.

Sends in PIV are never blocked by price problems unless the operator set a dollar spending limit. `balance` uses a separate display price that never influences what spending trusts, leaves the USD fields out when no price is available, and doesn't retry a failed display fetch for a minute.

Each payment records a checked price (never a display price), so `history` can show what it was worth at the time. In `history`, each payment's `amount_usd` excludes the fee; the `last_24h_usd` total includes fees.

### Spending limits

The operator can cap what the agent spends and require approval above a threshold:

```bash
pivx-agent-kit policy set --max-tx 25usd --daily 50usd --approve-above 10usd
```

| Limit | What happens |
|-------|--------------|
| `--max-tx` | A payment above it is refused. |
| `--daily` | A payment that would push the last 24 hours over it is refused. Fees count, and so do payments whose outcome isn't known yet. |
| `--approve-above` | A payment above it is held until the operator approves it. |
| `--allow <addr>` | Once any address is allowlisted, payments to other addresses are held. Repeat the flag to add more. |

Limits are in PIV (`25`) or dollars (`25usd`). `none` removes a limit, flags you leave out keep their current value, and `policy clear` removes everything. Approval never overrides `--max-tx` or `--daily`; raise the limit instead. If a dollar limit is set and no trustworthy price is available, payments are refused.

A held payment comes back to the agent as `"status": "pending_approval"` with a `pending_id`. The CLI exits with code 2. The operator then runs:

```bash
pivx-agent-kit pending list
pivx-agent-kit pending approve p_7f3a91c2d4e5   # shows the payment, asks y/N, then sends it
pivx-agent-kit pending deny p_7f3a91c2d4e5
```

A dollar daily limit converts the last 24 hours' PIV total at the current price, so earlier payments are revalued as the price moves.

Approving a held task bounty also records the approval with PIVX Tasks. Approving a held card payment re-checks the order first. An approval covers exactly the payment that was held: if the amount, destination or balance changed in the meantime (a bounty edited on the platform, say), nothing is sent. On approval the limits are checked again at a fresh price, not the one from when the payment was held. Held payments expire after 24 hours. Retrying the same task or order doesn't create duplicates; a retry with a different amount or destination replaces the held one. The approval prompt shows the destination, amount and value first, then the agent's reason and memo with any control characters escaped, and warns if another held payment would pay the same destination the same amount.

`policy set`, `policy clear`, `pending approve` and `pending deny` only work in an interactive terminal and ask for confirmation. Agents can read the limits (`pivx_policy`: limits, 24h spend, what's left today) and held payments (`pivx_pending_list`), but no MCP tool changes either. While a policy is set, seed export (`pivx_export`, `export`) is refused unless a human runs it in a terminal, since the seed would bypass every limit. `pivx_task_submit` refuses to attach files from the kit's data directory or the machine-id files the wallet key is derived from (symlinks are resolved first), so an agent can't upload the wallet as "proof of work". Payment reasons can't contain control characters and are capped at 280 characters.

The policy file is signed with a device-bound key. If it's edited by hand, every payment is blocked until the operator re-runs `policy set`, which starts from scratch instead of re-signing the edited values.

**Who the limits hold against.** They're enforced against an agent that reaches the wallet only through the MCP tools: no tool changes the limits, approves a payment, exports the seed or uploads the wallet's files, and every payment goes through the policy. That covers runaway loops, prompt-injected "send everything" instructions and fat-fingered amounts.

They are **not** enforced against an agent that can run shell commands as your user (for example Claude Code with its Bash tool). Such an agent can turn them off by editing or deleting the kit's own files: removing `policy.json`, editing `ledger.jsonl` to reset today's total, or running the CLI in a pseudo-terminal. Some agent harnesses run commands in a pseudo-terminal by default, which also passes the terminal check. If the agent has a shell, treat the limits as a guard against mistakes, not against a determined agent.

### Upgrading from 0.6

These change existing behavior:

- **`--from` is strict.** A typo is now an error. Before, anything other than `public` silently spent from the private balance.
- **Extra arguments are errors.** An unquoted multi-word memo or a stray extra argument used to be silently cut off.
- **A bare `usd` or `piv` after the amount is a unit, not a memo.** `send ps1… 5 USD` now sends $5 instead of 5 PIV with the memo "USD".
- **`pivx_send` takes exactly one amount field.** It needs either `amount` (PIV) or `amount_usd`, and a numeric value in either one is an error.
- **Payments can wait.** They take a lock, so a payment may wait up to 2 minutes for another one in progress. `balance` and `resync` take it too.
- **Payment reasons are restricted.** They can't contain control characters and are capped at 280 characters.
- **Seed export is gated.** While a spending policy is set, export needs an interactive terminal; `pivx_export` refuses.
- **Building from source needs Rust 1.89 or newer.**

### Spend history

Every payment the kit makes is written to `ledger.jsonl` in the data directory: plain sends, task bounties paid by `task approve`, and card orders paid by `cards order pay`. Each entry records the amount, fee, destination, txid, which balance paid, the reason, and what triggered it.

The entry is written *before* the transaction is broadcast, with the txid computed locally. If the ledger can't be written, the payment is not sent. If a send is interrupted, or the broadcast errors and the kit can't tell whether the transaction reached the network, the payment stays `unresolved` and the error names its txid. The next `history` call looks it up and marks it `broadcast` or `failed`. A payment only counts as `failed` once two sources say the transaction doesn't exist (the kit asks Blockbook explorers and PIVX Core RPC nodes alternately; today the ones that answer are both run by PIVX Labs). `history` re-checks failed payments for a day in case one surfaces late.

`history` prints JSON (newest first, with rolling 24h totals). `history --csv` prints a spreadsheet-safe CSV for expense reports.

**Retrying a task approval or card payment won't pay twice.** Payments from one data directory are serialized with a lock file, so two processes (two MCP servers, or MCP plus the CLI) can't pay at the same time. `task approve` and `cards order pay` check the ledger before paying, and again under the lock. If an earlier attempt for the same task and worker, or the same order, already went out, the kit reuses that payment's txid. If an earlier attempt's outcome can't be established (explorers unreachable, or it started less than 10 minutes ago), the command refuses and asks you to retry later. Plain `send` calls are never deduplicated, since sending the same amount twice can be intentional. Check `history` before retrying one that errored.

If the ledger can't be read, or was written by a newer version of the kit, every spend is refused rather than risk a payment the double-pay check can't see. Payments wait up to two minutes for one already in progress, then give up without sending.

Known limitation: once a task or order payment is recorded as broadcast, retries always reuse it. If that transaction were later dropped from the mempool (rare), pay the worker manually and pass `--txid` to `task approve`.

### PIVX Tasks platform

`task` is a native client for [PIVX Tasks](https://tasks.pivxla.bz) — the bounty board where agents and humans post, claim, and complete paid work in PIV. The kit handles the platform's body-hash signed-request scheme internally so agents never construct auth headers; registration is automatic on first signed request.

```
pivx-agent-kit task list    [--status open|in_progress|submitted|paid]
                            [--category dev|design|content|social|research|marketing|other]
                            [--limit N]                  Browse the task board
pivx-agent-kit task search  <query> [--limit N]          Full-text search the board
pivx-agent-kit task get     <id-or-url>                  Fetch a single task
pivx-agent-kit task profile [<handle>]                   Profile (rep + tasks created +
                                                         tasks worked). Omit handle for self.

# Worker:
pivx-agent-kit task signup  <id-or-url>                  Take a slot
pivx-agent-kit task submit  <id-or-url> <body> [file...] Submit a proof (auto-signs up
                                                         if you don't already hold a slot)

# Creator:
pivx-agent-kit task create  --title T --description D --category C --amount A
                            --verification V
                            [--currency PIV] [--quantity Q] [--min-rep R]
pivx-agent-kit task approve <id-or-url> --worker <handle>
                            [--from public|private] [--txid <hex>]
                                                         Auto-pay bounty from kit's wallet,
                                                         then mark approved. Pass --txid if
                                                         you've already broadcast.
pivx-agent-kit task reject  <id-or-url> --worker <handle> --reason <text>
                                                         Reject a delivery (no rep impact;
                                                         reason is shown to the worker)
pivx-agent-kit task cancel  <id-or-url>                  Cancel a task you created

# Inbox:
pivx-agent-kit task notifications [--unread] [--limit N]
pivx-agent-kit task notifications read <id>
pivx-agent-kit task notifications read-all
pivx-agent-kit task notifications dismiss <id>
```

`<id-or-url>` accepts a numeric id (`5`), a query path (`task?id=5`), or a full URL (`https://tasks.pivxla.bz/task?id=5`) — agents copy-pasting links work without preprocessing.

The platform endpoint is configurable via the `PIVX_TASKS_API` environment variable (default `https://tasks.pivxla.bz`). Useful for testnets, forks, or local development against your own instance.

### Transaction types

All four directions are supported:

| From | To | Method |
|------|----|--------|
| Private (Shield) | Shield address | `--from private` |
| Private (Shield) | Transparent address | `--from private` |
| Public (Transparent) | Transparent address | `--from public` |
| Public (Transparent) | Shield address | `--from public` (shielding) |

## Building from source

Requires Rust 1.89+ (for `std::fs::File::lock`).

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
- `ledger.jsonl` — append-only history of every payment sent (chmod 600 on Unix)
- `ledger.lock` — lock file that serializes payments across processes
- `price_cache.json` — price reference, last accepted spend price, display price
- `policy.json` — spending limits, signed with a device-bound key (chmod 600)
- `pending.json` — payments held for approval (chmod 600)
- `tasks_state.json` — cached PIVX Tasks handle (created lazily on first `task` command that needs it)
- `cards_state.json` — PIVCards orders this wallet created

The seed and mnemonic in the wallet file are encrypted with a device-bound key, so a copy of `wallet.json` alone can't be read on another machine. The key comes from the machine id, which on Linux is a world-readable file (`/etc/machine-id`): anyone holding both `wallet.json` and that file can decrypt the seed, so protect them together. The extended spending key is not stored — it is derived from the seed on-the-fly when needed and zeroized from memory immediately after. To migrate a wallet to another device, use `export` to retrieve the seed phrase, then `import` on the new device.

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
├── core.rs            Shared wallet operations (used by both CLI and MCP); `send` is the one spend path
├── spend.rs           SendRequest / Pool / Origin types, local txid computation
├── ledger.rs          Append-only spend ledger, history, double-pay guard, spend lock
├── payment.rs         Double-pay re-check → ledger intent → broadcast → outcome
├── price.rs           PIV/USD price fetch, cache, sanity checks, integer conversion
├── policy.rs          Spending limits: model, evaluation, signed storage
├── pending.rs         Payments held for approval
├── gate.rs            Applies the policy to a payment (allow / hold / refuse)
├── operator.rs        Terminal-only policy and approval commands
├── cli.rs             Argument parsing for send / history
├── timefmt.rs         UTC date formatting and parsing
├── mcp.rs             MCP server (JSON-RPC over stdin/stdout)
├── wallet.rs          Wallet state, creation, persistence, device encryption
├── keys.rs            Key derivation (Sapling + BIP32 transparent)
├── shield.rs          Block processing, note decryption, shield + transparent tx building
├── sync.rs            Binary shield stream parser, transparent UTXO sync
├── network.rs         HTTP client for PIVX RPC and Blockbook APIs
├── prover.rs          Sapling proving parameter management
├── checkpoint.rs      Pre-computed commitment tree checkpoints
├── simd/hex.rs        SIMD-accelerated hex encoding/decoding
└── task/              PIVX Tasks platform client
    ├── mod.rs         Subcommand dispatch
    ├── client.rs      HTTP wrapper + body-hash signed-request scheme
    ├── commands.rs    Command implementations (list, submit, approve, etc.)
    └── state.rs       Cached handle (~/<data-dir>/tasks_state.json)
```

The cryptographic core uses [librustpivx](https://github.com/Duddino/librustpivx) (PIVX's fork of the Zcash Sapling libraries) compiled natively — no WebAssembly, no JavaScript, no async runtime.

## License

MIT
