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

**Only a device running `rag-rat sync serve` can receive a sync connection.** Everything else dials
outward: the maintenance-hook pass that runs after a git action opens connections, reconciles, and
exits. It never listens. A device is therefore in exactly one of two roles at a time — the
per-database session lock enforces that:

| Role | Command | Dials out | Accepts in |
|---|---|:-:|:-:|
| **Host** | `rag-rat sync serve` (long-running) | no | yes |
| **Device** | the git-hook pass (automatic) | yes | no |

Two consequences follow, and the first is a limitation rather than a design goal:

- **Two devices cannot currently sync directly with each other.** Neither is listening. This is not
  a NAT problem — a device behind NAT is perfectly reachable through the relay *if something on it
  is listening* — it is that the maintenance pass is a short batch job with nothing alive to accept.
  Discovery does not help: it can say where a node is, not make it answer. Lifting this is
  [#1079](https://github.com/cq27-dev/rag-rat/issues/1079); until then, arrange things as below.
- **So you need at least one host today.** Every device reconciles *through* it: a device pushes
  what the host lacks and pulls what it lacks, so changes reach the other devices on their next
  pass. The host does not have to be dedicated or hosted anywhere — a desktop that is usually on is
  a host.

Until #1079 lands the arrangement is therefore: **one or more always-on hosts, and any number of
devices reconciling through them.**

```toml
# On the host — advertise it so devices can find it without hardcoding its node id
[sync]
discoverable = true
```

```bash
# On the host
rag-rat sync serve
```

```toml
# On every device: nothing to set. `discoverable` is already false, and fetching is not gated on it.
[sync]
```

Devices find the host because **fetching is never gated on `discoverable`** — a device queries the
discovery service and dials what it finds without advertising anything itself. That asymmetry is
deliberate: a laptop behind NAT reaches the host without becoming reachable, or discoverable, in
turn.

### Why `discoverable` belongs only on hosts, for now

Setting it on a device would announce an address that cannot accept a connection and that stops
existing seconds later when the pass ends, while the announcement itself lives on for its whole TTL.
Every device that discovered it would spend a dial that can only time out, and it would occupy one
of the few per-tag slots a reachable host needs. The device-side pass therefore ignores the flag and
only ever fetches; `discoverable` is read by `sync serve`.

This restriction is a consequence of the limitation above, not of the discovery design. Once
devices can accept connections ([#1079](https://github.com/cq27-dev/rag-rat/issues/1079)) the flag
becomes meaningful on any device, because a device that advertises itself will be reachable.

### Scale

Dials grow linearly with the number of devices — each device dials the host, not every other device
— and only the hosts advertise, so the per-tag *slot* limit counts **hosts**, not devices. One or
two hosts stay well inside it no matter how many devices sync through them. Device count reaches
discovery by a different route: it sets the size of each announcement, which is the 25-device
ceiling above.

Adding hosts is how you spread load or place one nearer a group of devices; devices that dial more
than one host also propagate changes between those hosts.

## The limits on discovery

Two separate ceilings, with different symptoms. Neither binds for an ordinary account, and both
degrade rather than breaking: discovery is routing advice, so anything it fails to find is still
reachable through `server_peers`.

**How many hosts can advertise.** The service holds at most 32 live announcements per account and
evicts the oldest to make room rather than refusing a newcomer. A host renews at half the TTL, so it
keeps about two live announcements of itself — **roughly sixteen hosts advertising at once**.
Devices cost nothing here; they only fetch. Past that, hosts evict each other and discoverability
**flaps**: a host findable this hour may not be next hour. Pin those hosts in `server_peers` instead.

**How many devices an account can have.** An announcement is sealed once per recipient, so it grows
by 80 bytes per roster-effective device against a publish limit of 2048 bytes — a ceiling of about
**25 devices**. Past it a host logs `roster is too large to seal into one announcement` and does not
advertise; it serves normally, and every device that reaches it through `server_peers` is
unaffected. The announcement is never truncated to fit, because which recipients got dropped would
silently decide who can find that host.

A fetch is bounded the same way: the service answers with as much as fits one response frame, chosen
at random, so a busy tag returns a **sample** rather than everything. A device therefore learns some
of its peers per pass and the rest on later passes.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `relay_url` | the shipped relay | The iroh relay peers pin. Discovery is pinned to a single relay with no third-party directory, so **two devices can only reach each other if they share this value**. `RAG_RAT_SYNC_RELAY` overrides per invocation. |
| `server_peers` | empty | Node ids dialed unconditionally, without consulting the discovery service. Tried before discovered peers. |
| `push_interval_secs` | `300` | Minimum seconds between device-side sync attempts. `0` attempts on every trigger. Also sets the TTL a serving host publishes its announcement under. |
| `discovery` | `true` | Use the peer-discovery service at all. `false` means peers come from `server_peers` and nowhere else — no queries, no announcements. |
| `discoverable` | `false` | Advertise this node so devices can find it. Requires `discovery`. Read by `rag-rat sync serve` only — see [Why `discoverable` belongs only on hosts](#why-discoverable-belongs-only-on-hosts). Fetching is **not** gated on it. |
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
  stop, and — because each sealed envelope is a version byte plus a fixed 80 bytes per recipient —
  the exact number of devices on the account, `(len - 1) / 80`, tracked across enrollments and
  removals without opening a single wrap. It can also publish junk under the tag, costing whoever
  fetches it a wasted slot. What it can no longer do is read a host's node id out of any
  announcement sealed after its removal. Rotating the tag itself is
  [#1081](https://github.com/cq27-dev/rag-rat/issues/1081); padding the envelope to hide device
  count is [#1087](https://github.com/cq27-dev/rag-rat/issues/1087).
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
