# Config files and the client/server split

Status: the three files, the auth token and the launch model are implemented.
Remote connections and video over the RPC are still design (`rpc.md` owns
those). Where this conflicts with `rpc.md` or `daemon-mode.md`, those describe
the shipped behaviour and this note is what replaces it.

## Scope

Three files, one process model, and one transport decision that hang together:
`config.json` (daemon), `ui.json` (client), `auth.json` (daemon secret). The
split is what lets the same UI point at a daemon on another machine without a
second install, because both binaries already ship in one payload.

## Process model

- The UI is the only thing that starts anything. It autostarts at login and, on
  launch, starts `siphon-daemon` from its own directory when the admin endpoint
  is idle (`crates/ui/src/service.rs::ensure_running`). The tray owns restart,
  at the process level — pid file, image-name check, respawn — because a frozen
  daemon cannot answer an RPC.
- The autostart entry launches `siphon.exe`, never `siphon-daemon.exe`: the
  tray has to exist for the session, and notifications now come from the UI.
  The installer writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Siphon`
  as `"<installdir>\siphon.exe" --minimized`, and the uninstaller deletes it;
  a portable copy therefore does not autostart.
- **Nothing runs while the user is logged out, by decision.** No service, no
  scheduled task, no S4U, no `ProgramData`, no elevation. The daemon dies with
  the session.

### What that costs

A logoff is an abrupt kill. Nothing handles a signal (`compio`'s `signal`
feature is enabled and unused), so a recording in flight loses its last
segment; `disk/io.rs` and `disk/mod.rs` `sync_data` the segments and the index
at every boundary, so that is at most the in-flight ~2 s segment, and an
incomplete run is already the case the v4 index and `resume.rs` handle. Do not
build anything that assumes a clean shutdown.

`--minimized` is the hidden start: the surface is shown by its first presented
frame (`zgui-runtime`, `show_first_frame`), so the window cannot be hidden
before it exists. The app hides it 50 ms after the first frame and trims the
working set, which means one short flash at login and nothing after it. The
tray's Open is the way back.

## The three files

| file | scope | where | secret |
| --- | --- | --- | --- |
| `config.json` | daemon | `%APPDATA%` / `$XDG_CONFIG_HOME` | no |
| `ui.json` | client | `%APPDATA%` / `$XDG_CONFIG_HOME` | no |
| `auth.json` | daemon | `%LOCALAPPDATA%` / `$XDG_STATE_HOME` | **yes** |

`APP_ID` is `com.iken.siphon`, or `com.iken.siphon.debug` in dev builds. The
two directory rules already exist: `store::config_dir` is `%APPDATA%` /
`$XDG_CONFIG_HOME`, `rpc::state_dir` is `%LOCALAPPDATA%` / `$XDG_STATE_HOME`.

Two rules cover all of it:

1. **Secrets live where the roaming profile does not.** A DPAPI blob in a
   roaming profile is bound to the wrong machine the day it syncs.
2. **A missing or unreadable secret degrades, never aborts.** The daemon starts
   and reports the token absent; the UI starts and shows the local daemon. The
   config and pins loaders already behave this way, and the secret files need
   it most.

### `config.json` — daemon, v6

```json
{
  "version": 6,
  "channels": [
    { "login": "sunnys", "id": 123456789, "displayName": "sunnys", "record": true }
  ],
  "recordDir": "D:\\Recordings",
  "recordDualFormat": false,
  "account": { "login": "ozeniken", "userId": 12345678 },
  "network": {
    "listen": [],
    "remoteControl": []
  }
}
```

- `channels[]` keeps the follow list and `record` and nothing else. `Channel`
  loses `notifyLive`/`notifyTitles`, and with them `ChannelSwitches` and
  `AddLogin { switches }` collapse — the switches are client state now, and the
  "re-adding a channel restores its switches" story moves to the UI.
- `account` is a **cache of what the token says about itself**, not a
  credential and not an authority. The token wins when they disagree; the cache
  is refreshed when it is empty or after a 401. A changed `userId` invalidates
  the fetched channel metadata, because that was resolved for the old user.
- `network.listen` is empty by default: pipe/socket only, no listener on a
  machine that does not need one, and `--admin` becomes an override rather than
  the only way in. If the daemon is reachable, `Config` is in the snapshot, so
  the UI can say so. `remoteControl` starts empty too — a capability has to be
  granted, never inherited — and lists only intents whose effect stays inside
  the daemon; `applyUpdate` and `setRecordDir` are not in the enum at all.
- No token field, ever. That is what keeps `Config` safe to put in `Snapshot`.

### `ui.json` — client, v2

```json
{
  "version": 2,
  "hdr": "auto",
  "tearing": false,
  "notifications": {
    "sound": true,
    "titleChanges": true,
    "filteredWords": ["offline"],
    "muteAfterStreamDownSecs": 300
  },
  "channels": {
    "123456789": { "notifyLive": true, "notifyTitles": false, "pinned": true }
  },
  "connections": [
    { "id": "local", "kind": "local", "label": "this machine" },
    {
      "id": "c41f0b2e",
      "kind": "remote",
      "endpoint": "home.example.net:7418",
      "fingerprint": "sha256:4f9c1e8a...",
      "label": "media box"
    }
  ],
  "activeConnection": "local"
}
```

- `channels` is a sparse map keyed by **Twitch channel id**, so it survives a
  rename and means the same thing on every connection; only a deviation from
  the defaults (`notifyLive`, `notifyTitles` on, `pinned` off) is written. Use
  `BTreeMap`, not `HashMap`: the file is pretty-printed and a hash map reshuffles
  it on every save.
- `pinned` is a boolean, not an ordered list: the pin is a bucket, and the
  order inside a bucket stays the fixed sort. An ordered list of ids only earns
  its keep if rows become draggable, and the conversion then is a one-way move
  from `pinned: true` to an id list.
- `notifications` holds only the globals, so the block has room to grow (a
  webhook URL belongs there). The old `pins.json` is gone; `Pins` and its
  load/save go with it.
- `UiSettings` stops being `Copy`. The renderer takes a `Copy` projection of
  `{ hdr, tearing }`, which is the only reason it was `Copy` at all; the
  module comment in `settings.rs` needs rewriting with it.
- Two clients on one daemon now differ in switches and pins, which is the same
  justification that put HDR and tearing here. Per-connection keying is not
  needed anywhere: channel ids are global and the pin is per channel.

### `auth.json` — daemon secret, v1

```json
{
  "version": 1,
  "protection": "dpapi",
  "token": "AQAAANCMnd8BFdERjHoAwE/Cl+sBAAAA..."
}
```

`protection` is `"dpapi"` (the token is the base64 of a `CryptProtectData`
blob) or `"none"` (plain, 0600). One format on every platform, and the marker
is the difference between "no token, paste a new one" and a decrypt error
nobody can explain. A corrupt or undecryptable file is `present: false`.

The token is the Twitch website's session bearer (`OAuth <token>`), not an
OAuth grant: no refresh, no scopes, and a life measured in months. So there is
no expiry field to store, and 401 is the whole failure story — a cookie that
rotates behind your back reads as a 401 from one that worked yesterday, and the
answer is one field to paste, not a retry loop.

### Files these imply

- `daemon.key` (daemon, state dir) — the daemon's static key or self-signed
  cert. Separate from `auth.json` on purpose: rotating the Twitch token must
  not change the server identity, and regenerating the identity must not cost
  the Twitch token.
- `clients.json` (daemon, state dir, **not** secret) and `client.key` (client,
  state dir, secret) — only if the remote credential is a mutual key rather
  than a bearer value. `clients.json` is `authorized_keys`-shaped and safe to
  edit by hand.
- `pins.json` — deleted. Pins are client state and now live in `ui.json`; a
  second client document with its own writer was drift, not a design.

## Who owns which field

The rule: **the daemon owns what makes it act, the client owns what it shows
and says.** `record` is the only per-channel switch that makes the daemon do
something, so it is the only one left in `config.json`.

### Notification policy moves to the UI

The switches, the word list, the mute window and the notification centre are
per-client, so `matcher.rs` and `notifier.rs` move to the UI with them. In the
protocol that removes `SetNotifyLive`, `SetNotifyTitles`, `AddFilteredWord`,
`RemoveFilteredWord`, plus `Snapshot::notifications`,
`Update::Notification` and `DismissNotification`. The centre then does not
survive a UI restart, and the webhook dies with the window — acceptable
*because* the UI autostarts at login and owns the daemon, so "the UI is not
running" stops being a state the product supports.

### Events, not coalesced state

`Update::ChannelUpsert` carries whole entities and is coalesced per channel
because Hermes viewcount traffic is frequent. A toast is an edge, so it cannot
come from coalesced state: title, game, live, stopped and offline transitions
need their own append-only update (`Update::StreamEvent { channel, kind, .. }`,
never coalesced), while viewcount stays coalescable because it never raises a
notification. The UI applies the word list, the switches and the mute window to
those events.

### The Twitch token, wherever it is handled

- Never in `Config`: it appears in every `Snapshot` and `Update::Config`.
- Never in argv: the command line is readable by every local user, and a
  service's `ImagePath` would put it in the registry.
- Never in a URL that gets logged. `variant.rs` puts it in the usher query
  string, so redaction has to cover queries and not just the `OAuth` header —
  otherwise `protection: "dpapi"` is undone by `daemon.log`.
- Only `play` and `record` have `--auth-token`. Giving one writes the file, so
  the flag is how a token is enrolled; the mode then uses it for that run.
  `daemon` has no such flag at all — its command line is the autostart entry,
  and an entry must not carry a secret — so it only ever reads `auth.json`.

## Transport

Local versus remote connections, the enrollment string, key pinning, version
skew and video over the RPC are protocol questions: `rpc.md` owns them. This
note owns only what the client stores about a connection (`connections`,
`activeConnection`) and why the local case stays the pipe.

## The channel table

- Order is fixed: pinned → live (newest first) → offline (most recently live
  first, which puts a channel dark for weeks at the bottom). No header sorting
  and no drag, for now; the pin is the only ordering control, so `order_key`
  does not change.
- The pin is a bucket, not a position.
- `last_live_ms` is an ordering key, not a start: roughly the broadcast's start
  a few seconds late while live, the end of it while offline.
  `stream_start_ms` (`stream.createdAt`) is the real start and feeds any uptime
  or duration.
- A filter bar is the retrieval tool. The offline tail scrolls and is not
  collapsed: grey already says "not live", so collapsing adds a second way to
  hide the same rows.

## Rejected

- A Windows service in every form — LocalSystem, named user, per-user service —
  plus a scheduled task with S4U, `ProgramData`, a machine-scope install and
  elevation. They exist to survive a logoff, and the logoff cost is bounded
  (above) while the credential is app-scoped, so none of the machinery buys
  anything.
- An ordered list of channels in `ui.json`: the follow list is the daemon's,
  and a total order in the client is a second copy of it.

## Open questions

- TLS with an SPKI pin, or Noise with client keys. This decides whether
  `clients.json` and `client.key` exist, or a bearer value sits in the client
  config.
- `protection`: DPAPI on Windows and plain 0600 on Unix, or DPAPI wherever it
  exists.
- Autostart mechanism, and the hidden-start flag.
- Media on a second connection or multiplexed in one.
- The webhook: stays with the UI (and dies with it), or moves back to the
  daemon.
- Capability names in `network.remoteControl`.
