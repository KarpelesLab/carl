# Google

The `google` feature gives agents access to the user's Google accounts:
Gmail, Calendar, Drive (Docs, Sheets, Slides) and Contacts.

## Linking

OAuth 2.0 for installed apps: authorization code with PKCE (S256) and a
loopback redirect, per Google's guidance for desktop clients. The device-code
flow isn't usable because it can't grant Gmail or Drive scopes.

1. `google_link` opens a random port on `127.0.0.1` in the daemon and returns
   the consent URL. The agent shows it to the user.
2. After approval, Google redirects the browser to that port. The daemon
   exchanges the code, reads the account's email from the `id_token`, and
   stores the refresh token.
3. If the loopback page can't load (Carl runs on another machine), the user
   pastes the final address into `google_link_complete`.

Links expire after 10 minutes. Linking the same account again adds areas
(`include_granted_scopes`). Several accounts can be linked; tools take an
optional `account` and require it when more than one is linked.

Accounts live in the daemon, so every agent on the machine shares them.

### OAuth client: both models

- **Bring your own** (today): the user creates a *Desktop app* client in their
  Google Cloud project and passes its JSON to `google_set_client`. Full scopes
  work without Google's app verification. The consent screen should be
  *In production* so refresh tokens don't expire after 7 days.
- **Built-in** (later): `BUILTIN_CLIENT` in `src/google/mod.rs` is empty.
  Gmail and Drive are *restricted* scopes: a shared client needs Google's
  verification plus a yearly security assessment (CASA) before it can serve
  more than 100 users without warnings. A configured client always wins.

Each account remembers which client issued its token; only that client can
refresh it.

## Whose account: `owner`

Each linked account records whose it is:

- `user` (the default): the user's own account. Carl acts **on their behalf**,
  so anything sent, shared or accepted with it speaks for the user. Paul
  (the approvals layer) will hold these to the stricter policy.
- `carl`: an account dedicated to Carl, e.g. its own mailbox
  (`carl@klb.jp`). Acting with it speaks for Carl, so it can get more
  latitude, like sending as itself.

Only a human changes it, with the CLI:

```sh
carl google accounts                      # list accounts and owners
carl google owner carl@klb.jp carl        # dedicate an account to Carl
```

There is deliberately no MCP tool for it: otherwise an agent could relabel
the user's account as Carl's to get more latitude. Linking an account again
keeps its owner. `google_accounts` shows it to agents.

Caveat: this guards against agents acting **through Carl**. An agent that can
also run shell commands as the same user could run that CLI or edit Carl's
files directly. Real isolation needs agents sandboxed from Carl's data dir
(e.g. Claude Code's sandbox denying writes to `~/.local/share/carl`).

## Scopes and what Carl does with them

The rule until the approvals layer (Paul) exists:

- **Affects only the user's own mailbox, calendar or drive**: allowed on any
  account (read, draft, label, private events and files).
- **Reaches other people** (sending, inviting, RSVPing, sharing, changing
  anything others see): only from accounts dedicated to Carl (`owner` =
  `carl`). From the user's account, tools refuse and point to drafts.
- **Deleting**: to the trash only, never permanently.
- **Local files**: Carl never reads or writes paths an agent picks. Uploads
  and attachments are passed as content; downloads land in
  `~/Downloads/carl/` (0600, never overwriting).

| Area     | Scope               | Any account | Carl-owned accounts only |
| -------- | ------------------- | ----------- | ------------------------ |
| mail     | `gmail.modify`      | search (paged), read, attachments, labels (archive, read, not spam…), drafts | send, send drafts, trash |
| calendar | `calendar`          | list, events, free/busy, get; create/update/delete own guest-less events on the primary calendar | guests & invitations, other calendars, RSVP, events others see |
| drive    | `drive`             | search (paged), read as text, download/export, Sheets ranges read; create/update/move/trash own unshared files, Sheets write on them | share/unshare, changing files others can see |
| contacts | `contacts.readonly` | search | — |

Sheets tools use the Sheets API, which must be enabled in the OAuth
client's Cloud project along with the Gmail, Calendar, Drive and People APIs.

The scopes are broader than some tools on purpose: Carl, not the token, is
what limits the agent, and new tools shouldn't make the user link again.

## New-mail subscriptions

A session can ask to hear about new mail in a linked account's inbox, e.g.
Carl's own `carl@klb.jp`: `google_mail_subscribe` (optionally only `from`
certain senders), `google_mail_unsubscribe`. Subscriptions belong to the
session that asked, and are saved with its state, so they survive daemon
restarts and updates; they end when the session unsubscribes or disconnects
cleanly.

The daemon (`mailwatch.rs`) polls each subscribed account every 30s with
Gmail's history API, remembering its position in `google/watch.json` so mail
arriving during a restart is still delivered. Each new inbox message goes:

- into each subscriber's `agent_inbox` (summary: from, subject, snippet…),
  wrapped as untrusted. Works with every client.
- as a **channel event** to Claude Code sessions that loaded Carl as a
  channel: `<channel source="carl" kind="email" account="…" message_id="…">`.
  It wakes the session without polling. It names only the account, the
  sender's address and the message id, never the subject or body: anyone can
  send mail, and channel content goes straight into the model's context.

Channels are a Claude Code research preview. Start the session with:

```sh
claude --dangerously-load-development-channels server:carl
```

(Carl isn't on the preview's allowlist, hence the development flag; Team and
Enterprise orgs must also enable channels.) Without it, events are dropped
silently and the agent relies on `agent_inbox`, e.g. with `wait_seconds`.

## Untrusted content

Anyone can email the user, invite them to an event, or share a document with
them. Tool results carrying such content wrap it as
`{"notice": "...untrusted...", "data": ...}` so the agent treats it as data,
not instructions. This matters more once Paul lets agents send and share.

## Storage

`<data dir>/google/` (0700): `client.json` and `accounts.json` (0600,
written atomically). Refresh tokens are as sensitive as wallet keys; they
move into the encrypted keystore when the wallet adds one. `google_unlink`
revokes the grant at Google before forgetting it.

## Open questions

- Attachments: download to a file, or return small text attachments inline?
- Sheets beyond CSV export of the first sheet (Sheets API for ranges, writes).
- Pagination for large result sets.
