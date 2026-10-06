# Contributing to MIRA

Thanks for your interest in improving MIRA! Bug reports, fixes, docs and
features are all welcome.

## How this repository works

This GitHub repository is the **public mirror** of MIRA. Development happens in
an upstream source repository, and each release is published here as a single
"Sync public snapshot for vX.Y.Z" commit along with signed binaries.

Pull requests are still welcome here. Here's what happens to yours:

1. **You open a pull request** against `main`.
2. **Automated checks run:** a web UI build, a `--locked` release build, and the
   test suite. On your first PR you'll also be asked to sign our
   [CLA](#contributor-license-agreement).
3. **A maintainer reviews it.** We may ask for changes.
4. **Once accepted, it's carried upstream.** Your change is imported into the
   source repository (you stay the author), built and tested there.
5. **We merge your pull request here.**
6. **It ships in the next release.** We'll comment on your PR with the version.

Between steps 5 and 6, `main` may briefly contain your change ahead of the next
release tag. That's expected.

## Before you start

- **For anything bigger than a small fix, please open an issue first** so we
  can agree on the approach before you put time in.
- One logical change per pull request. Smaller PRs are reviewed faster.
- Please don't bump the version in `Cargo.toml`. Maintainers set it at release time.

## Building and testing

You'll need the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs
it automatically), Node 22, and on Linux: `build-essential cmake libclang-dev
pkg-config`.

```sh
# Web UI (embedded into the binary at build time)
cd web && npm ci && npm run build && cd ..

# Binary
cargo build --locked

# Tests that CI runs
cargo test --locked --lib
cargo test --locked --test api_smoke --test fresh_install_smoke
```

## Documentation

If your change affects user-visible behaviour, please update the docs in the
same pull request:

- `docs/`: the public user documentation (see [docs/CONTRIBUTING.md](../docs/CONTRIBUTING.md)
  for voice and frontmatter conventions).
- `mira-docs/`: the in-app help compiled into the binary.
- `mira-docs/settings-reference.md` is **generated** from
  `config/mira_config.schema.json`. Edit the schema, then run
  `python3 scripts/gen_settings_reference.py`. Never edit it by hand.

## Security issues

Please **don't** open a public issue or pull request for security
vulnerabilities. Report them privately via GitHub's
[security advisory form](https://github.com/Vexillon-ai/MIRA/security/advisories/new).

## Contributor License Agreement

Before we can accept your first pull request, you need to sign the **MIRA
Contributor License Agreement (CLA)**. The CLA bot will comment on your PR with a
link, and signing takes one click with your GitHub account. You only sign once.
It covers all your future contributions.

The CLA is a **license, not a copyright transfer**: you keep the copyright in your
work. It lets the project keep its licensing options open, and it commits us to
keep every accepted contribution available under the open-source license.

## License

MIRA is licensed under the [GNU AGPL v3.0 or later](../LICENSE).
