# Peer sync (`[sync]`)

Sync replicates one account's signed memory op-log between that account's devices over an iroh QUIC
transport. Every device holds the whole log; there is no server that owns the data, and the relay
and discovery service both handle opaque bytes they cannot interpret.

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
— and only the hosts advertise, so the per-tag announcement limit is a limit on **hosts**, not on
devices. One or two hosts stay well inside it no matter how many devices sync through them.

Adding hosts is how you spread load or place one nearer a group of devices; devices that dial more
than one host also propagate changes between those hosts.

## The limit on advertised hosts

The discovery service holds at most 8 live announcements per account and refuses further publishes
rather than evicting. A host keeps about two live announcements of itself, so **roughly four hosts
can advertise at once**. Devices cost nothing here — they only fetch.

Four hosts is far more than most accounts want, so this is unlikely to bind. If it does, it degrades
rather than breaking:

- A host whose publish is refused **stops being discoverable**. It keeps serving normally, and any
  device that reaches it through `server_peers` is unaffected.
- Which hosts hold the free slots shifts as announcements expire, so **discoverability flaps** — a
  host findable this hour may not be next hour.

The fix is to pin the hosts in `server_peers` rather than relying on discovery for all of them.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `relay_url` | the shipped relay | The iroh relay peers pin. Discovery is pinned to a single relay with no third-party directory, so **two devices can only reach each other if they share this value**. `RAG_RAT_SYNC_RELAY` overrides per invocation. |
| `server_peers` | empty | Node ids dialed unconditionally, without consulting the discovery service. Tried before discovered peers. |
| `push_interval_secs` | `300` | Minimum seconds between device-side sync attempts. `0` attempts on every trigger. Also sets the TTL a serving host publishes its announcement under. |
| `discoverable` | `false` | Advertise this node so devices can find it. Read by `rag-rat sync serve` only — see [Why `discoverable` belongs only on hosts](#why-discoverable-belongs-only-on-hosts). Fetching is **not** gated on it. |
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

Note that this makes the device independent of discovery, not silent towards it — a pass still
queries the service on its cadence, in case the account advertises a host you have not pinned.
There is currently no switch that turns discovery off outright.

## What each service learns

- **The relay** forwards opaque encrypted QUIC traffic between peers.
- **The discovery service** is a blind key-value store keyed on a tag it cannot link to an account.
  The tag comes from account-scoped key material only enrolled devices hold — not from the account
  id, which every host you have ever dialed knows — so an outsider cannot compute it or find your
  hosts. What the service itself *can* see under that pseudonym is how many nodes advertise beneath
  a tag and when they stop renewing. Unlinkability is the guarantee; hiding host count and liveness
  from the service is not.
- **Neither is trusted.** A discovered address is routing advice only: every peer, discovered or
  configured, passes full mutual roster authorization before a single log entry is exchanged. A
  forged announcement costs a failed dial and nothing else.

Discovery failing — unreachable, slow, rate-limited, or answering with nonsense — never fails a
sync. The configured peers are dialed exactly as they would have been.

One known gap: a device removed from the roster can no longer sync, but keeps the ability to compute
the account's discovery tag, so it can still see which hosts are advertised and when. Fixing that
needs rotatable discovery key material —
[#1080](https://github.com/cq27-dev/rag-rat/issues/1080). Pin your hosts in `server_peers` if that
matters to you before it lands; discovery is then not in the path at all.
