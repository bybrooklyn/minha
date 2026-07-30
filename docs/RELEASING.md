# Releasing Minha

This document describes the public repository's release process. It is an
operator checklist for maintainers; it does not claim that a provider account,
terminal, operating system, or downstream updater has been qualified.

## Before tagging

1. Confirm the intended version is represented consistently in the workspace
   metadata and the changelog. Version changes are Cargo/source changes and are
   outside a documentation-only hygiene task.
2. Review the complete diff and confirm no credentials, local databases,
   recovery state, generated build output, private prompts, or machine-specific
   paths are tracked.
3. Run the required local checks:

   ```sh
   cargo fmt --all -- --check
   cargo test --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   cargo doc --workspace --no-deps
   cargo audit
   cargo deny check
   ```

4. Record any checks that could not run and why. Local checks do not prove live
   Codex login, model entitlement, provider availability, terminal rendering,
   or human accessibility review.

## Tag and workflow behavior

The release workflow runs only for a pushed semantic version tag matching
`v*.*.*`, for example `v0.1.0`. It builds the `minha` package with Rust 1.97.0
for these targets:

Before any platform build starts, the tag-validation job verifies the Cargo
version and reruns formatting, workspace tests, Clippy with warnings denied,
rustdoc with warnings denied, RustSec audit, and cargo-deny policy. A tag does
not bypass the repository's normal quality gates.

| Target | Asset |
| --- | --- |
| `x86_64-unknown-linux-gnu` | `minha-x86_64-unknown-linux-gnu` |
| `x86_64-pc-windows-msvc` | `minha-x86_64-pc-windows-msvc.exe` |
| `x86_64-apple-darwin` | `minha-x86_64-apple-darwin` |
| `aarch64-apple-darwin` | `minha-aarch64-apple-darwin` |

Each binary is uploaded as a raw target-named asset alongside a file with the
same name plus `.sha256`. The checksum file uses the conventional
`<digest><two spaces><filename>` format. A `gh`-based updater can select the
asset by exact target name, download its matching checksum, and verify the
bytes before replacement. The workflow does not package archives, install
files, or claim code-signing/notarization/attestation.

On Unix, the updater can atomically replace the current executable after
verification. Windows does not permit replacing the running executable in the
same way; Minha stages the verified `.exe` beside the current binary and
reports manual replacement instructions after exit. Treat that as a staged
download, not a completed update.

The final job uses the official `gh` CLI and `GH_TOKEN` to create the release
and upload the staged assets. It generates GitHub release notes. No release is
created by local validation; a maintainer must push the tag with explicit
authority.

## Maintainer command

After review and any required version/changelog edits, a maintainer may create
and push a tag through the repository's normal protected-branch process:

```sh
git tag -a vX.Y.Z -m "Minha vX.Y.Z"
git push origin vX.Y.Z
```

Do not run the push from an automated agent without explicit authorization.
After the workflow completes, inspect the GitHub Actions job, release asset
names, and each checksum before sharing the public release. A successful
workflow is build and upload evidence only; it is not proof of runtime or
provider qualification on every target.

## Recovery and failure handling

- A failed build must be fixed in a normal branch/PR and retagged only after
  review. Do not manually replace a failed asset with an unverified binary.
- If the release job fails after creating a release, inspect the release and
  asset list with `gh release view vX.Y.Z`; repair only with a reviewed rerun or
  a new version according to repository policy.
- If a checksum does not match, treat the asset as unusable and do not ask
  users to bypass verification.
- Keep provider credentials, `~/.minha`, `.minha/*.sqlite3`, logs, and recovery
  artifacts out of release assets.
