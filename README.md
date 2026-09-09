# git-remote-tape

[![Crates.io](https://img.shields.io/crates/v/tape-git-remote.svg)](https://crates.io/crates/tape-git-remote)
[![Documentation](https://docs.rs/tape-git-remote/badge.svg)](https://docs.rs/tape-git-remote)
[![License](https://img.shields.io/crates/l/tape-git-remote.svg)](LICENSE)

A Git remote helper for storing repositories on
[Tapedrive](https://tape.network). Put `git-remote-tape` on your `PATH` and Git
learns the `tape://` transport, alongside `https://` and `ssh://`.

```console
$ git clone tape://<tape-address>
$ git pull
$ git push
```

Every byte received by Git is verified against an on-chain commitment, including
bytes served through a gateway. Cloning needs no wallet, account, or permission;
only the holder of the tape's key can push.

> [!NOTE]
> Tapedrive is in early access and invite-only.
>
> [Sign up](https://tape.network/#sign-up) for access, join the
> [Discord](https://discord.gg/dVa9TWA45X) to follow development, or read the
> [docs](https://docs.tape.network) for the full picture. Anyone can clone an
> existing public repository; creating a tape and pushing to it requires access.

## Why Git on Tapedrive?

Tapedrive stores writes by content: a track's key is derived from its payload.
Git works the same way, with content-addressed objects, so a Git remote
mostly connects two compatible storage models.

| Git data | Tapedrive storage | Role |
|----------|-------------------|------|
| Packfiles | Content-addressed writes | Immutable repository objects, deduplicated by content |
| Branches, tags, and `HEAD` | One versioned write | The mutable view of the repository |

The helper speaks Git's native `fetch` and `push` protocol. It moves packfiles
and refs without re-synthesizing commits, so commit hashes, authorship, merge
topology, tags, signatures, file modes, symlinks, and submodules are preserved.

## Quickstart

Install the remote helper from crates.io:

```console
$ cargo install tape-git-remote
```

This installs the `git-remote-tape` binary. Git discovers it automatically when
it encounters a `tape://` URL.

Install the [Tapedrive CLI](https://docs.tape.network/tools/cli), then reserve a
tape for the repository:

```console
$ tape -u d create --capacity 100m --epochs 250
tape address: <tape-address>
```

The CLI saves the tape key under `~/.tape/cassettes/<tape-address>.json`, where
the remote helper looks for it by default.

Add the tape as a remote and push:

```console
$ cd my-repository
$ git remote add tape tape://<tape-address>
$ git push tape --all
$ git push tape --tags
```

Anyone with the address and the remote helper can now clone it:

```console
$ git clone tape://<tape-address>
$ git ls-remote tape://<tape-address>
```

From then on, branches and tags work through ordinary Git commands.

## Verification and access

The repository's ref index is verified against its on-chain commitment. Each
packfile is then checked against the digest in that verified index before being
passed to Git.

When `TAPE_GATEWAY_URL` is configured, the helper tries the gateway first for
faster bulk reads. Gateway bytes receive the same verification. Invalid, stale,
or unavailable data is discarded and fetched directly from storage nodes, so a
gateway changes performance rather than the trust model.

Reads are public and require no keys. A push requires both a transaction fee
payer and the key controlling the destination tape.

> [!WARNING]
> Repositories are public and storage is append-only. Deleting a ref or
> force-pushing does not remove previously stored objects. Never push a secret
> that may need to be withdrawn later.

## Configuration

| Variable | Default | Purpose |
|----------|---------|---------|
| `TAPE_RPC_URL` | `https://api.devnet.solana.com` | Solana RPC endpoint |
| `TAPE_GATEWAY_URL` | Direct storage-node reads | Optional gateway for bulk reads |
| `TAPE_KEYPAIR` | `~/.config/solana/id.json`, when present | Transaction fee payer for pushes |
| `TAPE_CASSETTE` | `~/.tape/cassettes/<tape-address>.json` | Key controlling the tape |

When the push credentials are absent, the helper remains read-only and cloning
still works. An explicitly configured but invalid keypair path is treated as an
error.

## Current limitations

- Tapes reserve a fixed capacity for a fixed period. Use `tape resize` and
  `tape extend` to manage the reservation.
- Repositories are public. Private repositories would require client-side
  encryption and are not currently supported.
- The first clone fetches the complete history. Later fetches download only
  packs the local repository has not already installed.
- There is no remote garbage collection. Packs accumulate as the repository is
  pushed.
- Concurrent pushes are resolved optimistically. The helper retries a raced
  update, but sustained contention can require the user to retry the push.
- This is a Git transport, not a forge: it does not provide a code browser,
  issues, pull requests, or collaboration workflows.

## Library use

The crate's `index` module describes how a repository is laid out on a tape:
the name and content type of the ref index, its JSON encoding, and the digest
recorded for each pack. Tooling that publishes a repository through the SDK
directly, or lists a published repository's refs without invoking Git, should
build on those types so it stays in agreement with the helper. See the
[API reference](https://docs.rs/tape-git-remote) for details.

## Development

```console
$ make check
```

To develop against a sibling checkout of the Tapedrive monorepo, copy the local
Cargo patch configuration:

```console
$ cp .cargo/config.toml.example .cargo/config.toml
```

The copied file is ignored by Git and does not change the published dependency
configuration.

## Learn more

- [Tapedrive documentation](https://docs.tape.network)
- [Tapedrive CLI](https://docs.tape.network/tools/cli)
- [Tapedrive SDK quickstart](https://docs.tape.network/sdks/quickstart)
- [Discord](https://discord.gg/dVa9TWA45X)

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for
details.
