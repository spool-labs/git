# tape-git-remote: verifiable decentralized git in one binary

`git clone`, `git pull`, `git push` against a Tapedrive tape. No server, no
hosting account. The binary is a standard git *remote helper*: drop
`git-remote-tape` on your `PATH` and git learns the `tape://` transport, the
same way it already knows `https://` and `ssh://`.

```console
$ git clone tape://7ebfEHSND45mPapMJ64MWVxgBafNoKwz4X2AdqqtiikL reed-solomon
$ cd reed-solomon && git log --oneline -1
d619d25 release 0.2.2
```

Every byte a clone receives is checked against an on-chain commitment before
git ever sees it, so you do not have to trust whoever served it.

> The bucket above is a **devnet** demo and its storage reservation expires. It is
> what every number in this README was measured against, not a permanent home.
> Reserve your own with `tape create`, below.

---

## The one idea

Git objects are content-addressed. An unnamed Tapedrive write is
content-addressed too, because its track key *is* `hash(payload)`. So git's
storage model and Tapedrive's turn out to be the same model, and a remote helper
is mostly just plumbing between them.

Two kinds of write do the whole job:

| what | how it's stored | why |
| --- | --- | --- |
| repository objects | **unnamed** content-addressed track, one per pushed packfile | immutable, deduplicating, excluded from the bucket's object listing |
| refs (branches, tags, HEAD) | **one named object**, rewritten each push | a named write appends a version, and `hash(name)` resolves to the newest, giving mutable-pointer semantics for free |

That's it. `refs` points at object ids, the pack list points at track numbers, and
everything else is git being git.

## Install

```console
cargo build --release
cp target/release/git-remote-tape ~/.local/bin/     # anywhere on PATH
```

`Cargo.toml` depends on the Tapedrive crates by version, from crates.io. To build
against a local checkout of the tape monorepo instead, copy the example config:

```console
cp .cargo/config.toml.example .cargo/config.toml
```

That file's `[patch.crates-io]` block redirects the seven `tape-*` crates to
relative paths. A patch resolves before the registry is consulted, so it works
even for versions that are not published yet, and neither `Cargo.toml` nor
anything in `src/` needs to change. `.cargo/config.toml` is gitignored, so local
paths stay local.

## Use it

Reserve a tape to push to. `tape create` files the keypair under
`~/.tape/cassettes/<address>.json`, which is exactly where this helper looks for
it, so push works with no further configuration.

```console
$ tape -u d create --capacity 100m --epochs 250
tape address:  7ebfEHSND45mPapMJ64MWVxgBafNoKwz4X2AdqqtiikL

$ cd my-repo
$ git remote add tape tape://7ebfEHSND45mPapMJ64MWVxgBafNoKwz4X2AdqqtiikL
$ git push tape --all
$ git push tape --tags
```

From then on everything is ordinary git:

```console
$ git clone tape://7ebfEHSND45mPapMJ64MWVxgBafNoKwz4X2AdqqtiikL
$ git pull tape main
$ git push tape main
```

Anyone can clone with no keypair, no stake, and no account. Only the holder of
the tape's keypair can push, so the tape's authority *is* the write ACL.

### How other people get at it

Cloning needs the binary and nothing else. No wallet, no account, no stake, no
permission granted by anyone. Verified with a completely empty environment:

```console
$ env -i PATH=/usr/bin:/bin:$HOME/.local/bin HOME=/tmp/stranger \
    git clone tape://7ebfEHSND45mPapMJ64MWVxgBafNoKwz4X2AdqqtiikL repo
Cloning into 'repo'...
tape: fetching pack 1 (219745 bytes)
...
```

`git ls-remote tape://<bucket>` also works, if you just want to see the refs.

Reading is open because the devnet nodes publish an `access_threshold` of zero.
Writing is gated by custody of the tape keypair. So the asymmetry is simple:
anyone reads, only the key holder writes.

**The part that still needs solving is shipping the binary.** This example builds
against `tape-internal` by relative path, so nobody outside that checkout can
compile it. Publishing `tape-sdk` to crates.io, retargeting onto the public
`tape-client-core` crate, or just distributing prebuilt binaries would each fix
it. As it stands, "install `git-remote-tape`" is not yet an instruction a
stranger can follow.

**Clone with no binary at all** is possible and worth building. Git still
supports the dumb HTTP protocol, which is a purely static file layout:

```
HEAD                          ref: refs/heads/main
info/refs                     <sha>\t<refname> per line
objects/info/packs            P pack-<sha>.pack
objects/pack/pack-<sha>.pack
objects/pack/pack-<sha>.idx
```

Write those as **named** objects on push and the gateway's existing site serving
makes `git clone https://<repo>.git.miester.id/` work with unmodified git. If the
packs themselves were named with their dumb-HTTP paths, both access paths could
share one stored copy rather than duplicating it. Two honest caveats. Git probes
`info/refs?service=git-upload-pack` first and only falls back to dumb HTTP if
that probe fails, so this depends on how the gateway treats query strings. And
gateway bytes reaching stock git are **unverified**, because stock git has no way
to check an on-chain commitment, so that path trades integrity for reach.
`tape://` stays the trustless one.

### Configuration

| variable | default | meaning |
| --- | --- | --- |
| `TAPE_RPC_URL` | `https://api.devnet.solana.com` | Solana endpoint |
| `TAPE_GATEWAY_URL` | unset, so reads go direct to storage nodes | gateway for bulk reads, and worth **setting** |
| `TAPE_KEYPAIR` | `~/.config/solana/id.json` if it exists | payer for transaction fees, **push only** |
| `TAPE_CASSETTE` | `~/.tape/cassettes/<bucket>.json` | the tape's key, **push only** |

The keypairs are optional. When neither is found the remote is simply read-only,
which is what makes a bare `git clone` work on a machine that has never seen
Solana. Set `TAPE_KEYPAIR` explicitly and a bad path becomes an error rather than
a silent downgrade.

### Use a gateway

A gateway is worth setting up, or pointing at your own:

```console
$ export TAPE_GATEWAY_URL=https://gw.example.id
$ git clone tape://<bucket>
```

Same repository, same machine, cold clone both times:

| reads via | wall clock |
| --- | --- |
| storage nodes direct | 17.4 s |
| gateway | **3.1 s** |

It is also the only path that works from a network that only lets 443 out, since
direct reads talk to storage nodes on their own advertised ports.

**This costs nothing in trust.** Gateway bytes arrive unproven, so the helper
proves them itself before git ever sees them:

- the **ref index** is checked against its on-chain commitment via
  `Tapedrive::verify`, which is the root of trust for everything else
- each **pack** is checked against the digest recorded in that now-proven index

Bytes failing either check are thrown away and refetched from storage nodes, so a
gateway that is broken, stale, or actively lying costs you latency and nothing
else. Public gateways are rate limited. A `429` is honoured up to ten seconds a
few times over, and after that the helper stops leaning on someone else's
capacity and goes direct.

## Does it really handle branches, commits, checkouts?

Yes, and not by handling them case by case but by never looking at them.

This helper implements git's `fetch`/`push` dialect, which moves two things:
opaque packfile bytes and a ref map. Branches *are* the ref map. Commit
messages, authors, dates, merge topology, file modes, symlinks, submodule
gitlinks, annotated tags and GPG signatures all already live inside the objects
git hands us, so they survive byte-identically and SHA-1s do not change.
`git commit` and `git checkout` never contact a remote at all, being local
operations on objects already in `.git`.

Measured against a real repository (`tape-reed-solomon`, 3 branches, 30 commits,
339 objects) pushed to devnet and cloned back:

| check | source | clone |
| --- | --- | --- |
| object graph digest | `4cfa4051...` | `4cfa4051...` |
| raw bytes of all 30 commit objects | `470cd094...` | `470cd094...` |
| `v0.2.2^{tree}` | `656e301e` | `656e301e` |

This is also why the helper does **not** use git's `import`/`export`
(fast-import/fast-export) capabilities, which would be less code. Those
re-synthesize commits rather than carrying them, and do not reliably preserve
exact object bytes. "Verifiable" has to mean the hashes match.

## Why one pack per push, not one write per object

The obvious mapping, one Tapedrive write per git object, is the wrong one. For
the repository above:

- 339 objects, 1.93 MB of raw object content
- 172 of them exceed the 825-byte inline limit, so each would take the slow
  path: erasure-code, register, upload slices, collect signatures, certify
- the same history as a single packfile is **219,745 bytes**

Git's delta compression does 9x better than per-object storage and you get it
for free from `git pack-objects`. One track holds up to 64 MiB, so the entire
history fits in one write instead of 339.

End-to-end on devnet, measured through git itself:

```
git push tape --all      278 objects   219,745 B   7.3 s
git clone tape://...     full history              4.6 s
git commit + git push      3 objects     4,499 B   9.3 s
git pull tape v0.2.2     fast-forward              7.0 s
```

An incremental push carries only the new objects, not the history. Latency has a
floor of a few seconds per operation, dominated by RPC bootstrap and peer
discovery rather than the transfer.

Underneath, a read pulls only *k* slices rather than the whole stored footprint,
302,736 B across 7 of 20 for that first pack. Erasure coding also means writes
tolerate stragglers. Five of twenty slice uploads failed during one of these
pushes and it still succeeded on quorum, with the rest handed to the recovery
worker.

After the runs above the bucket holds 7 tracks and 449,859 of its 104,857,600
bytes, and its object listing contains exactly one entry:

```console
$ tape object ls --bucket 7ebfEHSND45mPapMJ64MWVxgBafNoKwz4X2AdqqtiikL
TYPE            SIZE  CONTENT-TYPE      NAME
object           563  application/json  git/refs.json
```

The six packs are unnamed, so they never show up as objects. That is what lets a
git remote and a served website share one bucket.

## Why reads are trustless

The helper reads through the SDK's direct peer path, which fetches the track's
on-chain record and checks the bytes against its commitment, using `hash(bytes)`
for inline tracks and the Merkle commitment for erasure-coded ones. A mismatch
fails with `CommitmentMismatch` rather than handing git bad data. Peer TLS is
pinned to each node's on-chain key, so a fake peer cannot get in the way either.
On top of that, the ref index records each pack's digest and the helper re-checks
it.

When a gateway is configured its bytes go through the same standard before
reaching git, either proven against the on-chain commitment or discarded and
refetched from storage nodes. See [Use a gateway](#use-a-gateway). The guarantee
is identical either way and only the latency differs.

## Known limits

Worth reading before trusting this with anything that matters.

- **Storage is prepaid and expires.** A tape is reserved for a capacity and a
  number of epochs, costing `954 flux x MB x epochs` on devnet at roughly an hour
  per epoch, up to 256 epochs ahead. Past expiry the reservation lapses, so reach
  for `tape extend` and `tape resize`. This is the sharpest difference from a
  hosted forge.
- **Concurrent pushes are handled, but optimistically.** Tapedrive has no
  compare-and-swap. It resolves a name to its newest version and cannot reject a
  write based on the previous one, so a naive read-modify-write silently loses a
  simultaneous pusher's refs. Instead the helper writes, then checks the version
  list to see whether anything landed between the version it merged against and
  its own. If something did, it re-merges against the version it shadowed and
  writes again, up to five attempts. Re-deciding every refspec against the newer
  base *is* the merge, because the fast-forward checks re-run against reality, so
  a genuine divergence surfaces as `non-fast-forward` on that ref rather than
  quietly clobbering. Because storage is append-only, a lost race destroys
  nothing. Every index version and every pack stays readable, and a retry
  converges. What this is *not* is a lock. A push that keeps losing eventually
  gives up and asks you to retry.
- **Nothing can be un-pushed.** Storage is append-only. Force-pushing rewrites
  refs but never removes objects, so history cannot be lost, and an accidentally
  committed secret cannot be withdrawn.
- **Every repository is public.** The write side is already access-controlled,
  since only the tape's keypair can push, but reads are open to anyone holding the
  bucket address, permanently. A private repository here has to mean *encrypted*
  rather than *access-listed*. See below.
- **A freshly written track is not instantly readable.** It can be certified
  on-chain before enough peers will serve its slices, so reads retry with backoff
  over about twelve seconds. This matters most for the index once it outgrows the
  825-byte inline limit and becomes erasure-coded, which is why the encoding is
  kept as small as it is.
- **No remote `gc`.** Packs accumulate, one per push. A repack path that writes
  one consolidated pack and truncates the list is not implemented yet.
- **Fetch is whole-history.** The helper installs every pack the remote has that
  the local repo lacks, rather than resolving the specific object ids git asked
  for. Packs already installed are recorded in `.git/tape/installed-packs`, so
  `git pull` is incremental, but the first clone always reads everything.
- **`git fsck` reports dangling objects for refs you did not fetch.** Packs are
  the unit of transfer, so a fetch installs whole packs, including objects
  belonging to ref namespaces your refspec skipped. Harmless, and those objects
  become reachable the moment you fetch the refs.
- **A pushed repo is not a browsable website.** Tapedrive's gateway can serve a
  tape as a static site, but that reads the *named* object index and packs are
  unnamed. The two namespaces cannot collide, so publishing an HTML view alongside
  the git data into the same bucket would work. It just isn't implemented.

## Private repositories (not implemented)

Access control cannot work here, so this has to be encryption. The bucket address
is public, the data is permanent, and nothing can be withdrawn, so an ACL you
could add today would be a promise the storage layer is unable to keep.

The shape that does work is `git-remote-gcrypt`'s: encrypt each pack before it is
written, and encrypt the ref index too. The index matters as much as the packs,
because plaintext refs leak branch names, the commit graph's shape, and who is
working on what. What stays visible regardless is on-chain metadata: that the
bucket exists, how large each write was, and when each push happened. Push
cadence and rough diff sizes are not hideable.

Two consequences worth deciding on before building it:

- **Content addressing and encryption pull against each other.** Encrypt
  deterministically and identical packs dedupe, but equality leaks. Use a fresh
  nonce per write and dedup goes away. For git the nonce is the right call, since
  packs are already unique per push.
- **Revocation is impossible.** Append-only storage means a former collaborator
  keeps every pack they could already decrypt, forever. Rotating the key protects
  future pushes and nothing else. That is a property of the medium rather than a
  gap in the implementation, and it should be stated plainly to anyone who asks
  for "private repos".

Integrity survives fine. The on-chain commitment covers the ciphertext and an
AEAD tag authenticates the plaintext, so the verified-read story gets slightly
stronger rather than weaker. I would reach for `age` (the `rage` crate) with
per-collaborator recipient keys rather than hand-rolling any of it.

## Layout

```
src/main.rs    the stdin/stdout protocol loop: capabilities / list / fetch / push / option
src/push.rs    refspec decisions, the concurrency merge, publishing the index
src/fetch.rs   installing packs, and remembering which ones are already here
src/store.rs   Tapedrive I/O: gateway-then-direct proven reads, certified writes
src/index.rs   the ref index object, kept small enough to stay inline
src/git.rs     subprocess plumbing to pack-objects and index-pack
```

## Development

```console
$ cargo test           # 18 unit tests, no network required
$ cargo clippy --all-targets
```

The tests cover the parts worth testing without a chain: refspec parsing,
fast-forward and superseded-ref decisions, HEAD stickiness, pack-header parsing,
digest matching, index round-tripping, and a guard that a realistic index still
fits inside the 825-byte inline write limit.

Formatting follows the parent project. `rustfmt.toml` sets
`disable_all_formatting`, so layout is by hand and deliberate.
