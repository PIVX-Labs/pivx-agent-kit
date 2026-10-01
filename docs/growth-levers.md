# Growth levers for pivx-agent-kit

Audit date: 2026-10-01, against v0.6.1.

This covers what the kit does today, eight features that could drive growth, the bugs to fix before marketing any of them, and what to skip. The bar for each idea: high value, easy to market, not overcomplicated, and no chicken-and-egg dependency on other people adopting something first.

## What's already there

`pivx-agent-kit` is a single Rust binary that runs as both a CLI and an MCP server. It gives an agent a PIVX wallet with private (shield) and public (transparent) balances from one seed phrase. It also connects the agent to two services PIVX Labs runs:

- **PIVX Tasks** (`tasks.pivxla.bz`): a bounty board where agents and humans post, take and pay for work.
- **PIVCards** (`cards.pivxla.bz`): gift cards (Amazon, Steam, Uber and others) paid in PIV, sold through Bitrefill.

Every spend goes through one function, `core::send` (`src/core.rs:221`). Task bounty payouts (`task approve`) and card payments (`cards order pay`) both end up there, so anything added at that point applies to all spending. That's why most of the ideas below are cheap.

PIVCards is already the best demo you have ("my agent bought me a Steam card with private crypto"), and it doesn't depend on anyone else showing up. Tasks is a two-sided market, so any growth plan that leans on it inherits the chicken-and-egg problem. The ideas below avoid that.

## Growth levers, ranked

### 1. Spending limits plus human sign-off

**Pitch:** "Give your agent a wallet on a leash."

Fear is the biggest reason people won't hand an agent a wallet. Add a policy file in the data directory with:

- a per-transaction cap
- a daily cap
- optional allowlisted addresses
- an amount above which a person has to approve

The agent never gets an MCP tool to change the policy. Only a person can, through `pivx-agent-kit policy set ...`. Spends over the limit come back as `pending_approval`, and the person runs `pivx-agent-kit approve <id>` to release them.

Everything is enforced in `core::send`, so tasks and cards are covered automatically.

- **Effort:** small to medium.
- **Caveat to say upfront:** an agent with shell access could edit the policy file. Market it as a safety limit, not a security boundary.

### 2. Spending history with reasons

**Pitch:** "An expense report for your agent."

Right now nothing records what was sent. `balance` only shows memos on unspent notes, and those disappear once the note is spent.

Add an append-only `ledger.jsonl` in the data directory, written by `core::send`. Each entry holds the txid, amount, destination, which balance it came from, a timestamp and an optional `reason` field. Expose it through a `pivx_history` tool and a CSV export.

This also fixes a real money bug. In `task approve` (`src/task/commands.rs:558-567`), the bounty is paid first and the platform is told second. If the second call fails, the txid is lost and a retry pays the worker again. Writing the payment to the ledger first, and checking it before paying, makes approve safe to retry.

- **Effort:** small. Item #1 needs this ledger anyway for daily totals.

### 3. Getting paid

**Pitch:** "Let your agent charge for its work."

Add two tools:

- `pivx_request_payment`: returns a `pivx:` payment link, a QR code rendered in the terminal and a unique amount.
- `pivx_wait_for_payment`: waits until that payment arrives.

Matching on a unique amount (for example 5.00000317 PIV) works with any wallet, including MyPIVXWallet, so the payer doesn't need the kit. That keeps it single-player: one agent can sell to anyone today.

- **Effort:** small.

### 4. Dollar amounts

Agents and people think in dollars. Add `amount_usd` on send and payment requests, and show USD values in `balance`, using a price feed with a short cache. PIVCards already returns USD prices (`src/cards/commands.rs:140`), so this makes the whole product read in one currency.

- **Effort:** small.

### 5. Gift links (the viral loop)

Add `pivx_create_gift`. It sends PIV to a freshly generated key and returns a code or link. The recipient claims it in MyPIVXWallet and becomes a new PIVX user, with no wallet needed beforehand. Every gift brings in someone new.

- **Open question:** nobody has checked yet whether MyPIVXWallet's promo-code format can be matched exactly. If it can't, a small static claim page does the same job.
- **Effort:** medium.

### 6. Help with the first deposit

A new wallet holds 0 PIV, and that's probably where most installs stall.

- Add a `pivx_fund` tool that shows the address, a QR code, a payment link and where to buy PIV.
- Separately, PIVX Labs can post starter bounties on Tasks ("install the kit, finish a hello-world task, earn 1 PIV"). You control both sides of those, so there's no chicken-and-egg.

- **Effort:** small (the tool), plus some ops work for the bounties.

### 7. Distribution

Almost no code, and it leads straight to installs.

- Ship it as a Claude Code plugin bundled with skills such as "buy a gift card" and "post a bounty".
- List it in the MCP registries.
- Add MCP tool annotations (`readOnlyHint` on reads, `destructiveHint` on spends) so clients can auto-approve reads and ask before spending. That's safety you can market.
- Update `protocolVersion`, which is hardcoded to `2024-11-05` (`src/mcp.rs:415`).

### 8. One-command shielding

**Pitch:** "Go private."

Add `pivx_shield`, which moves the whole public balance into the private one. Privacy is what sets PIVX apart, and today the agent has to send funds to its own shield address by hand.

- **Effort:** very small.

## Bugs to fix before marketing

| Bug | Where | Impact |
|-----|-------|--------|
| **Double payment** in `task approve`: the bounty is paid before the platform call. If that call fails, the txid is lost and a retry pays again. | `src/task/commands.rs:558-567` | Lost funds |
| **Updates are broken on Windows.** The updater looks for `.tar.gz`, but releases ship `.zip`. Replacing a running `.exe` with `fs::rename` also fails on Windows. | `src/updater.rs:569`, `.github/workflows/release.yml:938` | Windows users can't update |
| **`install.sh` doesn't support Windows**, even though the README says Windows binaries are available. | `install.sh:13-17` | Install fails on Windows |
| **Agents using MCP can't read task submissions before approving.** `task proofs` exists in the CLI, but there's no `pivx_task_proofs` tool. | `src/mcp.rs` (tool list and dispatch) | Creator agents approve blind |
| **A typo in `--from` sends from the private balance.** `send ... --from pubic` silently spends private funds instead of failing. | `src/main.rs:102` | Wrong balance spent |
| **Task bounty amounts lose precision.** `task create` turns the amount into an f64 even though the MCP layer passes a string to avoid exactly that. | `src/task/commands.rs:417` | Rounding at satoshi scale |

## What to skip

These all need other people to adopt something first:

- agent-to-agent messaging over shield memos
- an HTTP 402 / x402-style paywall standard
- reputation across platforms
- Lightning-style payment channels

## Suggested order

1. **First release:** items 1, 2 and 4, plus the bug fixes above. The pitch is "safe, auditable, dollar-priced agent wallet."
2. **Second release:** items 3 and 5 for the growth loop (getting paid, and gift links that bring in new users).
3. **Any time:** item 7 (distribution) and item 8 (shielding), since neither depends on the others.
