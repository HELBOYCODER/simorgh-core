# NOTICE

Simorgh is a rebranded derivative work of the open-source project
**ZeroNet / Zray-Core**.

- Upstream source: https://github.com/zeghostwriter/ZeroNet
  (the Rust engine is published there as the `ZeroNet`/Zray-Core
  repository; core engine crate family `zero-*` and CLI `zray`).
- Upstream license: **MIT** — see `LICENSE`, retained verbatim with its
  original copyright ("ZeroNet contributors").
- Upstream copyright is not reassigned. The engine crates (`zero-core`,
  `zero-config`, `zero-net`, `zero-dns`, `zero-router`, `zero-security`,
  `zero-transport`, `zero-protocol`, `zero-runtime`, `zero-evasion`,
  `zero-observatory`, `zero-tun`, `zero-scanner`, `zero-discovery`,
  `zray-cli`, `zray-mobile`, `zeronet-tui`) and their code remain the work
  of the upstream authors.

## What Simorgh adds

- `crates/simorgh-daemon` — a new headless daemon (`simorghd`) that exposes
  the full engine feature set (the same surface the upstream Android app
  drives over JNI, documented in its `native-contract.md`) over a loopback
  HTTP/JSON RPC, so a native macOS GUI can drive the engine without JNI or
  a TUI.
- `docs/rpc-contract.md` — the HTTP contract the GUI team consumes.
- Vendoring adjustments: the mobile app tree (`ZeroNet-Mobile`), fuzz
  targets, and Android app build steps were removed from the workflows;
  two functions in `zray-mobile` (`stats_value`, `last_error_message`) were
  made public for daemon reuse. Internal crate names are left untouched.

## Third-party components

The engine vendors or links third-party code under their own licenses,
unchanged from upstream: rustls (the shaped fork pinned in
`Cargo.toml`, MPL-2.0), netstack-smoltcp (vendored in `vendor/`, see
`vendor/netstack-smoltcp/PATCHES.md`), ring/aws-lc (Apache-2.0/MIT),
boringtun, smoltcp, quinn/h3, and the crates.io dependencies recorded in
`Cargo.lock`. WARP account registration talks to Cloudflare's API; users
accept Cloudflare's terms themselves. The upstream project's use of the
name "ZeroNet" refers to that project, not to the unrelated zerone.io
network.
