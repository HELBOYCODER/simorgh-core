<div align="center">

# Simorgh Core

### The Simorgh engine: a Rust censorship-circumvention proxy with a headless daemon a native macOS GUI can drive.

کارایی، سرعت و آزادی — بر پایه‌ی زیرونت / Zray
</div>

---

**Simorgh** is the engine layer of the Simorgh macOS client. It is a
rebranded, MIT-licensed derivative of
[zeghostwriter/ZeroNet](https://github.com/zeghostwriter/ZeroNet) — see
`NOTICE.md` and `LICENSE` for full attribution — with one substantial
addition: `crates/simorgh-daemon`, a **headless daemon (`simorghd`)** that
exposes the engine's full feature set over a loopback HTTP/JSON RPC so a
native GUI can drive it without a TUI, JNI, or any hand-written FFI.

The engine (the `zero-*` crates, kept under their original names) is a
from-scratch Rust network core. Configurations that work with Xray on
Simorgh work here too: VLESS, REALITY, Vision, XHTTP, VMess, Trojan,
Shadowsocks, Hysteria2, TUIC, AnyTLS and WARP.

## What lives where

```
crates/
  zero-core … zero-observatory   the engine: config, transport, routing, DNS,
                                 TLS shaping, TUN, evasion, observatory
  zero-tun                       utun/wintun/TAP devices + userspace stack
  zero-scanner / zero-discovery  clean-IP scanning, feed discovery, link
                                 parsing, config building, WARP registration
  zray-cli / zray-mobile         the standalone CLI engine; the mobile C ABI
  zeronet-tui                    the upstream desktop app (kept, unused by the GUI)
  simorgh-daemon                 ★ new: the headless daemon (binary `simorghd`)
docs/rpc-contract.md             ★ the HTTP surface the Simorgh GUI consumes
```

## The daemon in one paragraph

`simorghd` boots, writes a rotated `simorgh.log` into its data dir, installs
a no-op socket protector (a desktop process needs none), binds a random
loopback port, generates a bearer token and tells the world about both with
a single stdout line:

```text
SIMORGH_READY {"port":53123,"token":"…"}
```

After that everything is JSON over HTTP, with the method set, request
schemas and event schemas of the upstream mobile contract
(`build_config`, `parse_links`, `start`/`reload`/`stop`, `stats`, …, and the
long-running jobs `discover`, `test_links`, `scan`, `warp_register`, streamed
as NDJSON events identical to the mobile ones). The complete,
example-by-example specification is
**[`docs/rpc-contract.md`](docs/rpc-contract.md)** — that is the document the
GUI team builds against, and the authoritative one.

### Quick start

```bash
cargo build -p simorgh-daemon --release
target/release/simorghd --data-dir "$(mktemp -d)" --token-file /tmp/simorgh.token &
# read SIMORGH_READY from stdout, or the token from the file
```

```bash
DIR=$(mktemp -d); ./simorghd --data-dir "$DIR"      # note port+token on the READY line
TOK=… PORT=…
curl -s -X POST -H "Authorization: Bearer $TOK" -d '{}' \
     "http://127.0.0.1:$PORT/rpc/is_running"
# {"running":false}
```

Security posture: loopback only (bind-time and per-connection), a
constant-time-compared bearer token on every request, 2 MB request cap, and
no endpoint accepts arbitrary outbound destinations the way an open proxy
would — `simorghd` is a control plane, not a transit point.

VPN mode (`mode:"vpn"` configs, with a `tun` inbound) opens a macOS `utun`
directly, like the desktop TUI does; that needs root, and without it the
daemon answers `{"error":"requires root"}` instead of blocking on an
elevation prompt.

## Building

- Toolchain: stable Rust (edition 2021, `rust-version` 1.95; the daemon has
  been built with stable ≥ 1.98). `rust-toolchain.toml` pins `stable`.
- `cargo build --release -p simorgh-daemon` builds `simorghd` for the host.
- The engine's own CLI is `cargo build --release -p zray-cli` (binary
  `zray`, `zray run config.json`).
- macOS universal binaries (aarch64 + x86_64, `lipo`) are produced in CI by
  `.github/workflows/release-engine.yml` on `engine-v*` tags; the artifact
  is `SimorghEngine-macOS-universal`.

## Tests and checks

`cargo test -p simorgh-daemon` covers the daemon's own pieces (bearer
comparison, log rotation, level parsing). The engine crates carry their
upstream test suites; `cargo test --workspace` runs everything CI runs
(`.github/workflows/ci.yml`).

## License

MIT — `LICENSE`, upstream copyright intact; attribution and a description of
what Simorgh changed are in `NOTICE.md`.
