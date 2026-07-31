# Peer sync (`[sync]`)

Sync replicates one account's signed memory op-log between that account's devices over an iroh QUIC
transport. Every device holds the whole log; there is no server that owns the data, and the relay
and discovery service both handle opaque bytes they cannot interpret.

This page is about **how to arrange your devices** — the one decision that determines whether sync
scales past a handful of machines — plus the `[sync]` table that expresses it. For minting an
account and enrolling devices, see `rag-rat sync --help` (`enable`, `init`, `join`).

## The two topologies

There are exactly two shapes worth running, and the difference is which devices **advertise
themselves** to the discovery service.

### Mesh — every device talks to every other

Each device advertises itself, discovers the others, and dials all of them on its sync cadence.
Nothing needs to be always-on: whichever devices happen to be awake together reconcile directly.

```toml
[sync]
discoverable = true          # on every device
```

Good for a few personal machines. It stops being the right shape quickly, for two independent
reasons:

- **Dials grow with the square of the device count.** Every device dials every other, and each dial
  carries two sessions (the account log, then content). Four devices is 12 peer dials per cadence
  across the account — 24 sessions; twenty devices is 380 dials. Nothing caps this, so it degrades
  as a slowdown and a lot of pointless traffic rather than an error.
- **The discovery service caps a tag at 8 live announcements** and refuses further publishes rather
  than evicting. A device holds about two live announcements of itself, so **roughly four devices
  can advertise at once**. Past that, some devices stop being discoverable — see
  [When the cap is reached](#when-the-cap-is-reached).

**Practical ceiling: about four devices.**

### Hub — devices reach one always-on host

One device (or a few) runs `rag-rat sync serve` and is the only thing that advertises itself.
Everything else leaves `discoverable` off, finds the host, and reconciles through it. Devices never
need to be awake at the same time: the host holds the log and hands it on.

```toml
# On the always-on host
[sync]
discoverable = true
```

```toml
# On every other device: nothing to set — `discoverable` is already false by default
[sync]
```

```bash
# On the host
rag-rat sync serve
```

This works because **fetching is never gated on `discoverable`**. A device that does not advertise
itself still queries the discovery service and finds the host. That asymmetry is deliberate: a
laptop behind NAT can reach a host without becoming discoverable, or reachable, itself.

The host does not have to be dedicated or hosted anywhere — a desktop that is usually on is a host.

**Practical ceiling: the number of advertisers, not the number of devices.** One or two hubs stay
inside the cap no matter how many devices sync through them, and dials grow linearly rather than
quadratically.

### Which to use

| | Mesh | Hub |
|---|---|---|
| `discoverable` | on everywhere | on the host(s) only |
| Always-on machine needed | no | yes |
| Devices must overlap in time | yes | no |
| Sessions per cadence | grows as N² | grows as N |
| Scales to | ~4 devices | many |

Start with mesh if you have two or three machines and no server. Move to hub the moment you have a
machine that is usually on, or a fourth device — it is a configuration change on each device plus
one long-running command, not a migration.

The two mix cleanly: a hub setup with one extra device also advertising is still a valid mesh of
two advertisers. What matters is the count of advertisers, not the labels.

## When the cap is reached

If more devices advertise than the discovery service has room for, publishes start being refused.
This degrades rather than breaking, and it is worth knowing exactly how:

- A device whose publish is refused **stops being discoverable itself**. It still fetches, still
  finds every other advertised device, and still dials and syncs with them.
- Peers listed in `server_peers` are unaffected — they never depended on discovery.
- Which devices hold the free slots shifts as announcements expire, so **discoverability flaps**
  rather than settling. A device that was findable this hour may not be next hour.

Nothing is lost and nothing errors; some devices are simply harder to find. The fix is to stop
advertising the devices that do not need to be found — which is the hub shape.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `relay_url` | the shipped relay | The iroh relay peers pin. Discovery is pinned to a single relay with no third-party directory, so **two devices can only reach each other if they share this value**. `RAG_RAT_SYNC_RELAY` overrides per invocation. |
| `server_peers` | empty | Node ids dialed unconditionally, without consulting the discovery service. Tried before discovered peers. |
| `push_interval_secs` | `300` | Minimum seconds between device-side sync attempts. `0` attempts on every trigger. Also sets the TTL this device publishes its announcement under. |
| `discoverable` | `false` | Advertise this device's node id so the account's other devices can find it. Fetching is **not** gated on this. |
| `discovery_node_id` | the shipped service | The discovery service's node id — a node id, not a URL; it is a separate peer reached through `relay_url`. `RAG_RAT_SYNC_DISCOVERY_NODE` overrides per invocation. |

### `server_peers` versus discovery

They are additive, and a peer listed in both is dialed once. Prefer discovery; reach for
`server_peers` when you want a peer dialed **regardless of whether the discovery service is
reachable** — it is the escape hatch that makes sync independent of that service.

An entry that is not a valid node id is logged, skipped, and **counted as a failed peer**, so a typo
shows up as an error rather than silently shrinking the peer list to a healthy-looking zero. Node
ids may be written as lowercase hex (the form the tools print) or as base32; the same node written
two ways is recognised as one peer and dialed once.

A device with no `server_peers` and no second device on its account roster does nothing — there is
provably nobody to reach, so it never contacts the discovery service.

## What each service learns

- **The relay** forwards opaque encrypted QUIC traffic between peers.
- **The discovery service** is a blind key-value store. Devices publish under a tag derived from
  account-scoped key material that only enrolled devices hold, so the service cannot tell which
  account a tag belongs to, and someone who has merely seen your account id cannot compute your tag
  or enumerate your devices.
- **Neither is trusted.** A discovered address is routing advice only: every peer, discovered or
  configured, passes full mutual roster authorization before a single log entry is exchanged. A
  forged announcement costs a failed dial and nothing else.

Discovery failing — unreachable, slow, rate-limited, or answering with nonsense — never fails a
sync. The configured peers are dialed exactly as they would have been.
