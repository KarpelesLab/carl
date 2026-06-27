# Wallet feature — design notes

> Status: **scaffold.** Tool surface exists (`wallet_balance`, `wallet_address`,
> `wallet_send`); handlers return "not implemented." No key material is touched
> yet. This document captures intent and open questions, not final decisions.

## Goal

Let an agent custody value and move it — check balances, derive receiving
addresses, and send funds — under explicit, auditable policy. The agent should
be able to *use* funds for tasks (pay an invoice, fund a service) without ever
seeing private keys.

## Tool surface (current scaffold)

| Tool             | Arguments                  | Purpose                          |
| ---------------- | -------------------------- | -------------------------------- |
| `wallet_balance` | `asset`                    | Spendable balance for an asset.  |
| `wallet_address` | `asset`                    | A receiving address.             |
| `wallet_send`    | `to`, `amount`, `asset`    | Send funds (authorized).         |

`amount` is a **decimal string** to avoid floating-point rounding of money.

Likely additions: `wallet_assets` (list supported/held assets), `wallet_history`
(transactions), `wallet_estimate_fee`, and a confirmation/quote step before
`wallet_send` commits.

## Open design questions

- **Chains/assets.** Which chains first? The tool surface is intentionally
  chain-agnostic (`asset` symbol); the backend needs a chain abstraction
  (address formats, fee models, finality, decimals).
- **Key management.** Where do keys live and how are they protected? Options:
  OS keychain, an encrypted keystore under `MANU_DATA_DIR`, a hardware signer,
  or a remote KMS/HSM. Keys must never appear in tool output, logs, or the
  conversation.
- **Spend authorization.** A `wallet_send` request from the model must not be
  sufficient on its own. Candidate controls: per-asset/period spend limits,
  allow-lists of destinations, an out-of-band human confirmation, and a signed
  audit log of every spend. This is the core security feature, not an add-on.
- **Balance source.** Self-hosted node, third-party RPC/indexer, or a custody
  API? Affects trust, latency, and privacy.

## Security requirements (non-negotiable)

- Private keys never leave Manu and never appear in logs/output.
- Every spend is authorized against policy and recorded in an audit log.
- Amounts use exact decimal arithmetic, never binary floats.
- Default-deny: an unconfigured wallet cannot send.

See `CLAUDE.md` → "Security posture."
