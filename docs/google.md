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

| Area     | Scope                   | Tools today                                      | Waits for approvals (Paul)        |
| -------- | ----------------------- | ------------------------------------------------ | --------------------------------- |
| mail     | `gmail.modify`          | search, read, labels, **drafts**                 | send, delete, label changes       |
| calendar | `calendar`              | list, events, free/busy, create (primary, no guests) | invite, RSVP, edit, delete    |
| drive    | `drive`                 | search, read as text, create/update private files | share, delete, edit shared files |
| contacts | `contacts.readonly`     | search                                           | —                                 |

The scopes are broader than today's tools on purpose: Carl, not the token, is
what limits the agent, and new tools shouldn't make the user link again.

"Private" is enforced by Carl, not Google: `google_drive_create` only writes
to My Drive's root or a folder the user owns and hasn't shared, and
`google_drive_update` only touches owned, unshared Docs and text files.
Calendar events are created with `sendUpdates=none` and no attendees field.

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
