---
description: Versions, upgrade steps, and per-version migration guides.
when: Updating an installed checkout or its CLI to a newer version.
---
# Upgrading

The version is `version` in [Cargo.toml](../Cargo.toml); `utsurigi-agent-hub --version`
prints it. Release `X.Y.Z` is the `main` commit tagged `vX.Y.Z`.

1. Record the installed version: `utsurigi-agent-hub --version`. If the CLI was
   already rebuilt, read `version` in Cargo.toml at the commit before the update
   (see `git reflog`).
2. Update the checkout to the target release tag, or to `main`.
3. Apply every guide below that is newer than the recorded version, oldest first.
   Versions without a guide need no action.
4. Rebuild with `cargo build --locked --release`, then run `validate` with
   `--config` for each Bot YAML.

## Guides

- [0.2.0](0.2.0.md): `storage.provider` defaults to `google-drive`.

## Releasing

Versions follow [Semantic Versioning](https://semver.org/). Before 1.0.0, bump the
minor version when users must act; otherwise bump the patch version. Change
`version` in Cargo.toml and commit the matching Cargo.lock update. A release that
needs user action adds `X.Y.Z.md` here and lists it under Guides. A change that
needs user action reaches `main` together with its version bump and guide. Never
edit a guide once it is on `main`; put corrections in the next version's guide.
After merging, tag the `main` commit `vX.Y.Z`.
