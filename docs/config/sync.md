# Peer sync (`[sync]`)

Sync replicates one account's signed memory op-log between that account's devices over an iroh QUIC
transport. Every device holds the whole log and there is no server that owns the data: the relay
forwards encrypted traffic it cannot read, and the discovery service holds only addressing hints
under a pseudonym it cannot tie to you. Neither is trusted — see
[What each service learns](#what-each-service-learns).

This page is about **how to arrange your devices** — which of them listens, which dial, and how they
find each other — plus the `[sync]` table that expresses it. For minting an account and enrolling
devices, see `rag-rat sync --help` (`enable`, `init`, `join`).

## How devices reach each other

**A device can receive a sync connection only while a process on it holds the database's sync
endpoint.** Two processes do — an active MCP session and `rag-rat sync serve` — and the per-database
session lock lets only one of them hold it at a time. Anything else dials outward only:

| Role | Started by | Dials out | Accepts in |
|---|---|:-:|:-:|
| **Active MCP session** | automatic — `rag-rat mcp` serving an indexed repo with sync enabled | yes | yes |
| **Headless host** | `rag-rat sync serve` (long-running) | no | yes |
| **Hook pass** | the git hook, when no MCP session is hosting | yes | no |

- **Active MCP session.** The first `rag-rat mcp` process on a database starts a resident sync host
  for it. The host accepts inbound sessions, reconciles its peers every `push_interval_secs`, and
  advertises itself when `discoverable = true`. It needs a write-capable roster role (Member or
  Owner) — a read-only device dials but does not host — and it lives exactly as long as that MCP
  process.
- **Hook pass.** After a git action the hook signals a live resident host (one that has heartbeated
  in the last 30 seconds) and returns; the host reconciles on its next tick. With no live host the
  hook falls back to a short dial-only pass: it opens connections, reconciles, and exits. It never
  listens.
- **Headless host.** `rag-rat sync serve` accepts connections and advertises with no agent session
  open — for a machine that should be reachable at any hour.

So **two devices sync directly whenever at least one of them has an agent session open** in an
indexed repo; a laptop and a desktop need no dedicated host. What they cannot do is reach each other
while both are idle, because a device without an MCP session only dials — and that is a question of
process lifetime, not NAT: a device behind NAT is reachable through the relay whenever something on
it is listening.

When sync has to work at any hour, keep one always-on host running `rag-rat sync serve` — a desktop
that is usually on is enough. Devices reconcile *through* it: each pushes what the host lacks and
pulls what it lacks, so changes reach the other devices on their next pass.

```toml
# On a machine others should find without hardcoding its node id: a `sync serve` host, or a device
# that usually has an agent session open
[sync]
discoverable = true
```

```bash
# Optional: an always-on host, reachable with no agent session open
rag-rat sync serve
```

```toml
# On a device that only needs to reach others: nothing to set.
# Fetching is not gated on `discoverable`.
[sync]
```

Devices find advertised peers because **fetching is never gated on `discoverable`** — a device
queries the discovery service and dials what it finds without advertising anything itself. That
asymmetry is deliberate: a laptop behind NAT reaches a host without becoming reachable, or
discoverable, in turn.

### When to set `discoverable`

Set it on a `sync serve` host, and on any device that usually has an agent session open. The flag is
read by whichever process holds the endpoint — `sync serve` or the resident MCP host — and never by
the hook pass, which only ever fetches.

Leave it off on a device whose agent sessions are short or rare. An announcement lives for its whole
TTL, so once the MCP session that published it exits, every device that discovers it spends a dial
that can only time out, and it occupies one of the per-tag slots reachable hosts need (see
[The limits on discovery](#the-limits-on-discovery)).

### Scale

Each device dials the peers it discovers plus its pinned `server_peers`, so dials grow with devices
times advertisers — and only machines with `discoverable = true` advertise, so the per-tag *slot*
limit counts **advertisers**, not devices. Keeping advertising to one or two usually-reachable
machines keeps both small no matter how many devices sync through them. Device count reaches
discovery by a different route: it sets the size of each announcement, which is the device ceiling
below.

Adding hosts is how you spread load or place one nearer a group of devices; devices that dial more
than one host also propagate changes between those hosts.

## The limits on discovery

Two separate ceilings, with different symptoms. Neither binds for an ordinary account, and both
degrade rather than breaking: discovery is routing advice, so anything it fails to find is still
reachable through `server_peers`.

**How many hosts can advertise.** The service holds at most 32 live announcements per account and
evicts the oldest to make room rather than refusing a newcomer. A host renews at half the TTL, so it
keeps about two live announcements of itself — **roughly sixteen hosts advertising at once**. A
machine without `discoverable` costs nothing here; it only fetches. Past that, hosts evict each
other and discoverability **flaps**: a host findable this hour may not be next hour. Pin those hosts
in `server_peers` instead.

**How many devices an account can have.** Announcements use **25 wrap slots (2001 bytes)**, padding
unused slots with indistinguishable seals to discarded recipients. More than 25 devices exceeds
the service's 2048-byte publish limit. `MAX_PUBLISHABLE_RECIPIENTS` derives the ceiling from the
byte limit and wrap size. Past it a host logs `roster is too large to seal into one announcement`
and does not advertise; it serves normally, and every device that
reaches it through `server_peers` is unaffected. The announcement is never truncated to fit,
because which recipients got dropped would silently decide who can find that host.

A fetch is bounded the same way: the service answers with as much as fits one response frame, chosen
at random, so a busy tag returns a **sample** rather than everything. A device therefore learns some
of its peers per pass and the rest on later passes.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `relay_url` | the shipped relay | The iroh relay peers pin. Discovery is pinned to a single relay with no third-party directory, so **two devices can only reach each other if they share this value**. `RAG_RAT_SYNC_RELAY` overrides per invocation. |
| `server_peers` | empty | Node ids dialed unconditionally, without consulting the discovery service. Tried before discovered peers. |
| `push_interval_secs` | `300` | Minimum seconds between sync attempts — the resident MCP host's reconcile cadence, and the hook pass's rate limit. `0` attempts on every trigger. Also sets the TTL an advertising node publishes its announcement under. |
| `discovery` | `true` | Use the peer-discovery service at all. `false` means peers come from `server_peers` and nowhere else — no queries, no announcements. |
| `discoverable` | `false` | Advertise this node so devices can find it. Requires `discovery`. Read by whichever process holds the endpoint (`rag-rat sync serve` or an active MCP session), never by the hook pass — see [When to set `discoverable`](#when-to-set-discoverable). Fetching is **not** gated on it. |
| `discovery_node_id` | the shipped service | The discovery service's node id — a node id, not a URL; it is a separate peer reached through `relay_url`. `RAG_RAT_SYNC_DISCOVERY_NODE` overrides per invocation. |

### `server_peers` versus discovery

They are additive, and a peer listed in both is dialed once. Prefer discovery; reach for
`server_peers` when you want a peer dialed **regardless of whether the discovery service is
reachable** — it is the escape hatch that makes sync independent of that service.

An entry that is not a valid node id is logged, skipped, and **counted as a failed peer**, so a typo
shows up as an error rather than silently shrinking the peer list to a healthy-looking zero. Node
ids may be written as lowercase hex (the form the tools print) or as base32; the same node written
two ways is recognised as one peer and dialed once.

A device with an enrolled account queries the discovery service once per cadence even when nothing
is configured and its own roster shows no other device. That is deliberate, and it is the one place
where the cheaper-looking behaviour is wrong: the roster is replicated state, so "I am the only
device" is only as current as the last sync. A machine restored from a backup taken before your
other devices were enrolled believes it is alone — and if that belief stopped it looking, it would
never receive the entries that would correct it. It would stay stuck forever while a perfectly
reachable host advertised. The cost of looking anyway is one small request per cadence for an
account that really is alone.

A device with **no** account does nothing at all: there is nothing to sync and nothing to look for.

Pinning a host in `server_peers` is the answer when you would rather not *depend* on the discovery
service: paste the node id `sync serve` prints at startup, and the device dials it whether or not
discovery is reachable.

On its own that makes the device independent of discovery, not silent towards it — a pass still
queries the service on its cadence, in case the account advertises a host you have not pinned. Set
`discovery = false` as well to stop that: peers then come from `server_peers` and nowhere else, and
the device neither queries the service nor advertises to it.

```toml
[sync]
discovery = false
server_peers = ["<the node id `sync serve` printed>"]
```

The trade is that a host you have not pinned becomes unreachable, so pin every host you intend to
use — including any you add later.

`server_peers` is also where cross-account hosts go: a contribution owner's host, or a
contributor's. Discovery cannot find them — a foreign account's discovery tag derives from a
secret only its own devices hold — so automatic cross-account sync dials only pinned peers. Once
a pinned host answers for a foreign account, the device remembers which one and skips it during
its own device sync (a host serves only its own account, so dialing it there could never
succeed). Removing a host from `server_peers` stops all dialing to it, including remembered
cross-account pulls.

## Sharing a repo's memories across accounts

Everything above replicates ONE account between its own devices. Sharing a repo's memories with a
**teammate** — a separate identity, on a machine you do not control — is a different arrangement:
the repo has one **owner** account whose stream holds the shared set, and any number of granted
**contributors** whose memory writes target that stream.

The short version, one ticket end to end:

```bash
# Owner, once, on a dedicated public index: publish it, then stay online with a one-time invite
rag-rat sync publish
rag-rat sync invite-writer          # prints ragratinvite… and keeps serving

# Teammate, in a checkout of the same repo:
rag-rat sync contribute <ticket>
```

**`sync publish` is one-way and account-wide.** Every later memory the account authors goes onto a
public stream that anonymous readers can pull, and the command refuses an account that already holds
private memories or has authored sealed content. Publish a fresh index dedicated to the shared set,
not your working one: `sync publish --seed <index path>` first imports this repo's locally-authored
memories from an existing index (peer-synced memories are left out), and re-running it re-mirrors
from that source.

Redeeming the ticket does the whole exchange the old two-paste flow left half-finished: the owner
authors the Writer grant naming the teammate's account at redemption (no account ids change
hands by chat), the teammate's store pulls the owner's log over the same route so the grant takes
effect locally, and contribution is configured — subsequent `memory_create`/`memory_update` in
that checkout author onto the owner's stream. The ticket is single-use and expires (15 minutes by
default); pasting it into the wrong command says so by name, exactly like a pairing ticket.

For ongoing automatic sync in both directions, each side pins the other's serving host in
`[sync] server_peers` (memories travel by AUTHOR: the owner collects a contributor's entries by
syncing the CONTRIBUTOR's account, and vice versa — see the cross-account note under
`server_peers`). The pieces are also available separately when a ticket exchange is impractical:
`sync whoami` (the id to grant), `sync grant <id>` (owner side), `sync contribute <owner-id>` +
`sync pull <owner-id>` (teammate side). `sync uncontribute` stops contributing: authoring returns to
this store's own stream, the contributions already authored stay on the owner's stream, and the
grant stays open until the owner revokes it.

The owner stays in charge afterwards: `sync grants` lists who holds access, open and revoked, and
`sync revoke <account> --reason …` closes it — `departed`/`rotated`/`superseded` keep the work
this store has already accepted, `compromised` quarantines everything the grantee authored
(`--keep-until <seq>@<device>` can carve one vouched prefix back in).

### Subscribing read-only

A reader who wants a published repo's memories without writing back subscribes instead of
contributing. No grant is involved:

```bash
rag-rat sync subscribe              # owner named by the repo's checked-in .rag-rat-stream
rag-rat sync subscribe <owner-id>   # or name the owner explicitly
rag-rat sync unsubscribe
```

A publisher can check a `.rag-rat-stream` into the repo root so a clone needs no hand-carried id:

```toml
owner = "<64-hex account id, from the owner's `rag-rat sync whoami`>"
peers = ["<node id of the owner's serving host>"]
relay = "<relay URL, when the owner's host uses a non-default relay>"
name = "<display name; never used for identity>"
```

Only `owner` carries authority. The first subscribe pins it, and a later commit naming a different
owner is refused rather than followed — confirm the new id with the stream's owner and pass it
explicitly to move the pin. `peers` and `relay` are routing hints followed as given, because every
entry pulled is verified against the pinned account. In practice they are needed: discovery cannot
find a foreign account's host, so a locator naming only the owner reaches nothing unless that host
is already in `server_peers`. Nothing materializes until the owner's log reaches this store —
automatic sync pulls it once the owner's host is reachable, or run `rag-rat sync pull <owner-id>`.

**Subscribing replaces what the repo mirrors, and deletes.** Exactly one stream materializes a repo,
so while subscribed it mirrors the owner's stream instead of this account's: memories this
account's *other devices* synced here are removed, not marked stale. Memories authored in this store
keep going to its own stream and stay. `sync unsubscribe` restores the removed set, except local
binding work — a `memory rebind` made on a synced memory, and local edges onto it — because a
re-drain seeds only the anchors each memory's author published.

## What each service learns

- **The relay** forwards opaque encrypted QUIC traffic between peers.
- **The discovery service** is a key-value store keyed on a tag it cannot link to an account. The
  tag comes from account-scoped key material only enrolled devices hold — not from the account id,
  which every host you have ever dialed knows — so an outsider cannot compute it or find your hosts.
  Announcements are sealed to the account's current devices, so the service reads no node id out of
  a payload.

  It learns your node ids anyway, by a different route: publishes and fetches arrive over
  authenticated connections, so the service sees the node id at the other end of each one. Under
  that tag it can therefore see which nodes advertise, which nodes ask, how many there are, and when
  they stop renewing — that is, your active device set and its liveness. Sealing is aimed at whoever
  can compute the tag *without* being the service, which is the case that actually arises: a removed
  device, or a leaked tag. Unlinkability of tag to account is the guarantee; hiding your devices from
  the service is not.
- **Neither is trusted.** A discovered address is routing advice only: every peer, discovered or
  configured, passes full mutual roster authorization before a single log entry is exchanged. A
  forged announcement costs a failed dial and nothing else.

Discovery failing — unreachable, slow, rate-limited, or answering with nonsense — never fails a
sync. The configured peers are dialed exactly as they would have been.

### Managing devices

`rag-rat sync devices` lists the account's enrolled devices: fingerprint, role, label, whether each
holds owner authority, and which one is this store. The other two commands name a device by its
fingerprint or its first 8+ hex digits, and must run on an owner device:

- `rag-rat sync remove-device <device> [--reason <text>]` removes a lost device, or one whose store
  forked (`sync whoami` lists forked chains: the store was restored from an older copy or copied to
  another machine). A device cannot remove itself.
- `rag-rat sync promote <device>` gives an enrolled member device owner authority, so a sole owner
  can hand over before its own device is removed. A read-only device cannot be promoted.

### What a removed device keeps

Removing a device revokes it, but per host and not instantly: a serving host authorizes every peer
against its own local roster projection, so it stops syncing with the removed device only once it has
learned and folded the removal — the same per-host propagation that governs sealing below. A removal
authored on another device reaches a host only when an authorized peer syncs it there; until then
that host still treats the device as enrolled and syncs with it. The device also stops appearing as a
recipient of anything sealed afterwards. Two further things survive removal even at a host that has
folded it, because they do not depend on anything the account can take back:

- **The discovery tag**, which is derived from immutable account material already in that device's
  database. It can therefore keep watching the tag: how many hosts advertise and when they renew or
  stop. Newly published announcements have 25 wrap slots regardless of the actual recipient count,
  so their length hides enrollments and removals. Older publishers still expose their exact
  recipient count; cached unpadded envelopes are replaced on upgrade, but published copies remain
  visible until expiry. It can also publish junk under the tag,
  costing whoever fetches it a wasted slot. What it can no longer do is read a host's node id out
  of any announcement sealed after its removal. Rotating the tag itself is
  [#1081](https://github.com/cq27-dev/rag-rat/issues/1081).
- **Announcements sealed before it stops being a recipient**, which stay openable by it until they
  expire — at most one TTL, so 15 minutes at the default cadence. The window is measured from when a
  host **learns** of the removal, not from when it was authored: a removal authored on another
  device does not reach a host until an authorized peer syncs it there, and until then the host
  keeps the removed device in its own effective roster and re-seals every renewal to include it. A
  host that no remaining device ever reaches never learns, and keeps a removed device openable
  indefinitely — one more reason a host is only as current as its last inbound sync.

Neither grants access to data. Every peer, discovered or configured, still passes full mutual roster
authorization before a single log entry moves, and a removed device fails it at any host that has
folded the removal. Pin your hosts in `server_peers` and set `discovery = false` if you would rather
the tag not be in the path at all.

## Unresolved table rows

`rag-rat sync diagnostics --limit 100` lists the active repository's last observed local table-row
failures, including a missing clock or winner entry, an undecodable winner, a wrong operation,
table or key, an unprojectable winner, an unreadable local row, or a superseded self-apply.
The limit is 1–1000 rows across current table streams. `limit_reached` means the bounded report may
have more rows; the library's `table_sync_row_diagnostics` API also supports keyset pagination.

These observations are local diagnostics, not replicated authority or a reason to delete a row.
They survive restart and failed authoring rollback, including re-adoption failures. The
`self_apply_failed` flag keeps a failed authoring attempt visible even after its winner can be
resolved: a readable row alone does not prove a blocking clock or tombstone was repaired.
Successful publication or deletion clears that flag and the diagnostic;
a raw local repair becomes visible on the next producer scan or replay. A row with an unreadable
primary key has no addressable row identity and is not included in this per-row report. The command
reads existing observations; it does not trigger a scan or author entries. An empty report therefore
means no observations were found for the current streams, not proof that every row was scanned.

### Interrupted table recovery

An adopted retention floor may still be missing a suffix carrying updates or deletes. New sessions retain that promised suffix tip across restarts and report continuation until it arrives. Table authoring and compaction on the affected stream wait; local edits remain unsent. Reconnect to a peer retaining the missing history. Older intermediaries and floor adoptions predating this tracking do not carry the same guarantee. See [suffix delivery and recovery limits](../design/table-suffix-coverage.md).
