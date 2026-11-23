# Config files and the client/server split

Status: design. Nothing here is built. Where this conflicts with `rpc.md` or
`daemon-mode.md`, those describe the shipped behaviour and this note is what
replaces it.

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
  The entry is a Run-key value or a Startup shortcut; undecided, and cosmetic.
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

A login-time autostart also needs a hidden start: `crates/ui/src/main.rs` builds
the tray and then runs the app unconditionally, so a startup entry today would
pop the window at every login. `tray::show` is already wired to the
single-instance hand-over, so the flag is small.

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
    "remoteControl": ["recordOnce", "setRecord", "pipelineControl"]
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
  the UI can say so.
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
- `--auth-token` writes the file on startup and logs that it did, rather than
  being an invisible in-memory override that vanishes on restart.

## Connections

### Local

The pipe/socket stays the local transport. It *is* the access control (pipe
DACL, 0600 socket), there is no listener on a machine that does not need one,
and `probe`, `ensure_running` and restart are built on it. `Connection::Local`
means "this install's endpoint", not "a URL that says loopback": trusting
loopback would let any local process impersonate a daemon.

### Remote

`siphon://host:port#<fingerprint>` is the **enrollment string** the daemon
prints and the user pastes once. It is parsed into the fields above and never
re-parsed at connect time. The fingerprint rides in the fragment because a
fragment is never sent to a server; the credential never goes in the URL at all
— paths and userinfo leak into logs, shell history and, if the scheme is ever
registered, the URI-handler registry.

`rpc::AdminStream` is a concrete type and `spawn_listener(endpoint)` takes a
string, so a TCP admin transport is a third case, not a rewrite.

### Identity and pinning

- The daemon generates its key on first run and prints the fingerprint with the
  enrollment string, so **the first connection is pinned, not trusted on first
  use**. TOFU is the fallback for a hand-typed address. This matters more than
  in SSH, because the first connection carries the credential.
- Pin the key, not the certificate: the SPKI hash (TLS) or the static key
  (Noise). Pinning a self-signed certificate turns every renewal into a false
  alarm for no security gain.
- A mismatch offers "paste the enrollment string again", never a "connect
  anyway" button.

### Two transports, undecided

- **TLS**: `rustls` is already a dependency, and the hand-rolled HTTP could
  reuse it. Needs a `ServerCertVerifier` that ignores names and validity and
  compares the SPKI to the pin (the `dangerous` API), and keeps
  `openssl s_client` as a debugging tool.
- **Noise** (`snow`): the static key *is* the identity, and `XX` gives mutual
  authentication, so there is no bearer value on the wire at all and revoking a
  device is deleting a line from `clients.json`.

### What remote costs

- `ensure_running` must not spawn, and `restart` cannot be process-level: the
  tray Restart degrades to an RPC exit request, or to "go restart it there".
  The tuned "a frozen daemon is still restartable from the tray" property is
  local-only, and that should be said in the UI, not discovered.
- `probe` needs a connect plus handshake with a timeout, and "unreachable" must
  be distinguishable from "wrong fingerprint".
- Intents differ in blast radius. `applyUpdate` is local-only: running an
  installer on another machine is a different feature. `setRecordDir` is
  local-only: the path is on the daemon's filesystem, so a remote folder picker
  would be a lie. `network.remoteControl` gates the rest.
- No NAT traversal, no rendezvous. An address and a port, plus "use a tailnet
  or forward a port", is the honest scope.

### Version skew

One build today, two machines later. Because every install carries a daemon, a
mismatch is never a dead end — the UI can fall back to its own local daemon. So
the contract is exact-match `PROTOCOL_VERSION` with both versions named in the
refusal, rather than compatibility shims.

The protocol version now gates framing as well as messages, so a mismatch that
slipped through would be a corrupt picture rather than a clean error: gate on
the handshake, never open media before `Welcome`, and carry a capability set
(media framing, HDR, what the client can decode) because the two machines'
renderers genuinely differ.

## Video over the RPC

RTSP is being removed and media goes over an extended RPC. That deletes the
only unauthenticated surface and puts video behind the same credential. A media
subscriber is just another spool consumer with a `Reader`, which fits better
than the per-session packet queues did.

The control plane's assumptions invert:

- **Framing.** `RPC_MAX_FRAME` is 1 MiB and the codec is postcard. Access units
  need their own message class and cap, and a length-prefixed borrowed slice
  rather than a postcard-scanned `Vec<u8>`. Keep "reject before allocating".
- **Backpressure.** A control connection over `MAX_OUT` is dropped and resyncs
  from `Snapshot`. Media must never drop the connection; it skips forward to
  the next IDR and tells the client to flush. `Stream::send`'s "first IDR on
  every track" is that resync gate.
- **Head-of-line blocking.** A `Snapshot` must not queue behind a keyframe. The
  cheap answer is a second connection on the same handshake and credential,
  subscribing by `StreamKey` — RTSP's SETUP/PLAY split without RTSP. Per-stream
  flow control in one socket is the expensive answer.
- **Buffer ownership.** One packet is shared as `Rc<Vec<u8>>` today because
  per-session copies cost ~20k allocations/s. The RPC writer's per-connection
  `Vec<u8>` has to become a queue of shared buffers plus a per-connection
  header, with the write completion handing the buffer back.
- **Time.** PTS/DTS and codec config travel explicitly. Seek becomes an intent,
  and the SR-anchoring and seek-epoch rigmarole in `rtsp-output.md` goes with
  RTSP.

Keep a probe. RTSP is the only independent end-to-end check today
(`ffplay -nodisp rtsp://...`, `ffprobe`), and once the UI player is the only
consumer, "pipeline wrong" and "renderer wrong" look the same. Before RTSP
goes, add a `--dump` or `siphon-cli` subscriber that writes Annex-B or fMP4 so
`ffplay -f h264 -` still works.

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
