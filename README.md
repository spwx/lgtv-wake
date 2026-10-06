# lgtv-wake

Turns an LG webOS TV on and switches it to the gaming PC's input when an Xbox
controller connects on Linux, and turns it off when the last controller is
switched off with a long press of the Xbox button.

udev starts one `lgtv-wake watch` process per connected controller, as a
systemd user unit. The process watches the controller's buttons and exits
when the controller disconnects. Nothing runs while no controller is connected.

- **Wake:** a controller connects and either sends input or stays connected
  for `wake_delay_secs`. A controller waking up from idle and immediately
  dropping again (a "ghost" reconnect) doesn't wake the TV.
- **Off:** the controller disconnects after the Xbox button was held for
  `long_press_secs`, no other controller is connected, and the TV is still on
  the configured input. If the TV is on something else, it's left alone.

It's a single static binary with no runtime dependencies. The TV's TLS
certificate is pinned (`certs/lg-c6.der`), and the config and client key live
in `~/.config/lgtv-wake/`.

## Install (Linux)

Download the latest release and run `setup`:

```sh
curl -fsSLo lgtv-wake https://github.com/spwx/lgtv-wake/releases/latest/download/lgtv-wake-x86_64-linux-musl
chmod +x lgtv-wake
./lgtv-wake setup
rm lgtv-wake
```

`setup` is safe to run again (for example to upgrade). It:

1. copies itself to `~/.local/bin/lgtv-wake`;
2. writes `~/.config/lgtv-wake/config.toml` if it doesn't exist, asking for the TV's IP and MAC address
   (or pass `--host`, `--mac`, `--broadcast`, `--input`);
3. installs the user unit `~/.config/systemd/user/tv-controller@.service` and reloads systemd;
4. installs the udev rule `/etc/udev/rules.d/90-lgtv-wake.rules` with `sudo`
   (`--no-sudo` prints the commands instead);
5. pairs with the TV if there's no client key yet (`--no-pair` skips it).
   The TV must be on: accept the prompt on screen.

Both files are in [`deploy/`](deploy/) and are embedded in the binary.

Then turn a controller on and watch the decisions:

```sh
journalctl --user -u 'tv-controller@*' -f
```

## Commands

| Command | |
|---|---|
| `pair [--force]` | Pair with the TV and save the client key |
| `status` | Print the power state and current input; exits 1 if the TV doesn't answer |
| `on` | Wake the TV (Wake-on-LAN, retried until `wake_timeout_secs`) and switch to `input` |
| `off` | Turn the TV off, only if it's on `input` |
| `watch <device>` | The per-controller loop that the udev rule starts (Linux only) |
| `setup` | Install everything (Linux only) |

`pair`, `status`, `on` and `off` also work on macOS, with the config in
`~/.config/lgtv-wake/` there too. Each machine needs its own client key.
Set `RUST_LOG=debug` for more detail.

## Config

```toml
host = "192.168.1.50"        # use the IP, so DNS can't block a wake
mac = "aa:bb:cc:dd:ee:ff"
broadcast = "192.168.1.255"
input = "HDMI_1"
# optional, with these defaults:
# wake_delay_secs = 5
# long_press_secs = 5
# wake_timeout_secs = 20
# tls = "insecure"
```

If a TV firmware update changes its certificate, connections fail with a pin
mismatch. Either replace `certs/lg-c6.der` and make a new release, or set
`tls = "insecure"` to accept any certificate from `host`.

To re-pin, fetch the TV's leaf certificate (the first one printed) and convert it to DER:

```sh
openssl s_client -connect <tv-ip>:3001 -showcerts </dev/null 2>/dev/null \
  | openssl x509 -outform DER -out certs/lg-c6.der
```

## Development

```sh
cargo test
cargo clippy --all-targets
cargo zigbuild --release --target x86_64-unknown-linux-musl   # Linux build from macOS
```

Tests that talk to a real TV are ignored by default: `cargo test -- --ignored live_`.

## Releasing

Bump `version` in `Cargo.toml`, commit, then tag and push:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The release workflow builds a static `x86_64-unknown-linux-musl` binary and attaches it,
with a `.sha256`, to a GitHub release.
