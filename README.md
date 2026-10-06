# lgtv-wake

Turns an LG webOS TV on and switches it to the gaming PC's input when an Xbox
controller connects on Linux, or a key or mouse button is pressed, and turns it
off when the last controller is switched off with a long press of the Xbox button.

udev starts one `lgtv-wake watch` process per connected controller, keyboard and
mouse, as a systemd user unit. The process watches the device's buttons and exits
when the device disconnects.

- **Wake:** a controller connects and either sends input or stays connected
  for `wake_delay_secs`. A controller waking up from idle and immediately
  dropping again (a "ghost" reconnect) doesn't wake the TV.
- **Off:** the controller disconnects after the Xbox button was held for
  `long_press_secs`, no other controller is connected, and the TV is still on
  the configured input. If the TV is on something else, it's left alone.
- **Idle off:** in Game Mode, a controller with no input for `idle_off_mins`
  is disconnected over Bluetooth, which powers it off, and the TV turns off
  under the same conditions as a long press. Steam's own idle setting can't
  power off an Xbox controller over Bluetooth, which otherwise stays on for
  ~42 minutes. In Desktop Mode, controllers are left alone. Set
  `idle_off_mins = 0` to turn this off. This uses `loginctl` (to read the
  session's desktop) and `bluetoothctl`.
- **Keyboard and mouse:** a key press or mouse click (not movement or
  scrolling) on a USB or Bluetooth keyboard or mouse, including ones with
  their own dongle, turns the TV on and switches to `input`. Each device does
  this at most every 30 seconds, and the TV is left alone if it's already on
  `input`. Keyboards and mice never turn the TV off, and stay connected. The
  udev rule gives the logged-in user read access to mice, as systemd already
  does for keyboards.

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

`setup` is safe to run again. To upgrade later, run `lgtv-wake update`. `setup` does the following:

1. copies itself to `~/.local/bin/lgtv-wake`;
2. writes `~/.config/lgtv-wake/config.toml` if it doesn't exist, asking for the TV's IP and MAC address
   (or pass `--host`, `--mac`, `--broadcast`, `--input`);
3. installs the user unit `~/.config/systemd/user/tv-controller@.service` and reloads systemd;
4. installs the udev rule `/etc/udev/rules.d/90-lgtv-wake.rules` with `sudo`
   and applies it to the keyboards and mice already connected
   (`--no-sudo` prints the commands instead);
5. pairs with the TV if there's no client key yet (`--no-pair` skips it).
   The TV must be on: accept the prompt on screen.

Both files are in [`deploy/`](deploy/) and are embedded in the binary.

Then turn a controller on or press a key, and watch the decisions:

```sh
journalctl -t lgtv-wake -f
```

## Commands

| Command | |
|---|---|
| `pair [--force]` | Pair with the TV and save the client key |
| `status` | Print the power state and current input; exits 1 if the TV doesn't answer |
| `on` | Wake the TV (Wake-on-LAN, retried until `wake_timeout_secs`) and switch to `input` |
| `off` | Turn the TV off, only if it's on `input` |
| `watch <device>` | The per-device loop that the udev rule starts (Linux only) |
| `setup` | Install everything (Linux only) |
| `update [--force]` | Download the latest release, check its SHA-256 and run its `setup` (Linux only) |

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
# idle_off_mins = 15
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

TLS uses rustls with the `ring` provider only. `aws-lc-rs` needs cmake and a C
toolchain for cross builds, so `cargo tree -i aws-lc-rs --target all` should
stay empty. `tokio-tungstenite`'s `rustls-tls-webpki-roots` feature brings in
a root store, but it's never used: the pinned-certificate `ClientConfig` is
passed in through `Connector::Rustls`.

## Releasing

Bump `version` in `Cargo.toml`, commit, then tag and push:

```sh
git tag vX.Y.Z && git push origin vX.Y.Z
```

The release workflow builds a static `x86_64-unknown-linux-musl` binary and attaches it,
with a `.sha256`, to a GitHub release.
