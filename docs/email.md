# Email feature — design notes

> Status: **scaffold.** Tool surface exists (`email_create`, `email_list`,
> `email_send`); handlers return "not implemented." This document captures intent
> and open questions, not final decisions.

## Goal

Let an agent create and manage email identities and send/receive mail, backed by
Karpelès Lab email APIs. Useful for sign-ups, receiving verification codes,
correspondence, and per-task disposable addresses.

## Tool surface (current scaffold)

| Tool           | Arguments                          | Purpose                            |
| -------------- | ---------------------------------- | ---------------------------------- |
| `email_create` | `name?`                            | Create a managed address.          |
| `email_list`   | —                                  | List managed addresses.            |
| `email_send`   | `from`, `to`, `subject`, `body`    | Send from a managed address.       |

Likely additions: `email_inbox` / `email_read` (receive and read messages),
`email_wait` (block for an incoming message, e.g. a verification code),
`email_delete`, and richer message content (HTML, attachments).

## Open design questions

- **API integration.** Which Karpelès Lab endpoints back create/list/send and
  inbox retrieval? How is Carl authenticated to them (API key in
  `CARL_DATA_DIR`, OAuth, …)? Receiving likely needs polling or a webhook/relay
  — stdio has no inbound channel, so incoming mail is surfaced via a `wait`/poll
  tool the agent calls.
- **Address lifecycle.** Naming/domains for created addresses, quotas, and
  cleanup of disposable addresses.
- **Sending constraints.** `from` must be an address Carl manages; rate limits
  and anti-abuse so the agent can't be used to send spam.
- **Content & safety.** Plain text first; HTML/attachments later. Validate
  recipients; consider a send-confirmation step mirroring the wallet's spend
  authorization.

## Notes

- Receiving is fundamentally pull-based here: the agent asks Carl to check or
  wait for mail; Carl does not push.
- Treat API credentials with the same care as wallet secrets — never in logs or
  tool output.
