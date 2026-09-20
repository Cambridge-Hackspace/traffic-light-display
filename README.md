# traffic-light-display

Software to drive the traffic light display at the
[Cambridge Hackspace](https://cambridgehackspace.com).

## Flashing

The light carries two firmware slots. A new board is flashed over USB once;
after that you push to it over the network from wherever you are.

### What you need

- The `esp` Rust toolchain. `rust-toolchain.toml` selects it; install it with
  [espup](https://github.com/esp-rs/espup).
- `espflash` 4.x: `cargo install espflash --locked`
- For USB flashing, permission on the serial port:
  `sudo usermod -aG dialout $USER`, then log out and back in.

### First flash, over USB

```sh
cargo run --release
```

That writes the bootloader, the partition table from `partitions.csv`, and the
application into `ota_0`, clears the slot selection, and opens the serial
monitor. You need it for a new board, and after any change to `partitions.csv`.

**Moving an older light onto this firmware** changes its partition layout, so it
has to happen over USB. The Wi-Fi credentials and display settings live in a
partition this layout leaves exactly where it was, so the light should rejoin
the network by itself. If it does not, it falls back to its own access point:
join `Traffic-Light` and open <http://192.168.71.1/>.

Check the boot log afterwards. It should list `otadata`, `ota_0` and `ota_1`,
say `No factory image, trying OTA 0`, and then print your existing marquee text
back at you — that last part is the proof the settings survived.

### Over the air, after that

```sh
cargo run --release -- 192.168.1.110    # a particular light
cargo run --release -- ota              # the remembered one, see below
```

This builds a release image, converts it, checks it is a real application image
of a plausible size that will fit the slot, asks the light what it is currently
running, asks you to confirm, uploads it with a progress bar, and then waits for
the light to come back. It confirms the result by comparing the hash esp-idf
stamps into the image with the one the light reports, so pushing a build it is
already running is fine and a silent rollback is not mistaken for success.

```
==> converting the ELF to an application image
==> checking the image
    target/ota/traffic-light-display.bin: 1107008 bytes, 54% of the slot
==> asking http://192.168.1.110:80 what it is running
    running 0.2.0 from ota_0
replace the firmware on 192.168.1.110 (0.2.0 -> 0.3.0)? [y/N] y
==> uploading to http://192.168.1.110:80/ota
################################################################## 100.0%
==> the device accepted it (ok)
==> waiting for 192.168.1.110 to come back
==> up on 0.3.0 from ota_1 (image hash matches)
```

The lamps show the upload as a progress bar while it runs, filling red, then
amber, then blue.

Useful flags:

| Flag | What it does |
|---|---|
| `--dry-run` | build and run every check, upload nothing |
| `--yes` | do not ask before overwriting |
| `--no-verify` | do not wait for the light to come back |
| `--check` | just report what it is running, and stop |
| `--user NAME` | portal username, if it is not the default `admin` |
| `--port N` | http port, if it is not 80 |
| `--timeout SEC` | how long to allow for the upload (default 300) |
| `--allow-debug` | permit pushing a debug build |

Anything after the address is passed straight through, so
`cargo run --release -- 192.168.1.110 --user chack --dry-run` works.

To set a default light so you can type `cargo run --release -- ota`, either
export `TLD_OTA_HOST=192.168.1.110` or write the address into a file called
`.ota-host` in the repo root. That file is not committed.

### Pushing without the toolchain

The script works on its own, so an image built elsewhere can be pushed with
nothing but `bash` and `curl`:

```sh
scripts/ota-push.sh --image traffic-light-display.bin 192.168.1.110
scripts/ota-push.sh --check 192.168.1.110
```

Both of those need the password too; see below.

### The password

Every page and endpoint the light serves is behind a password — the settings
page, the update endpoint, and the `/version` endpoint the push script reads. A
light that has never had one set stays open and says so in red at the top of its
page; firmware upload is the one thing it will not do until a password exists.
Set one under **Portal access** on the page.

The simplest thing is to put the credentials in `~/.netrc` (not in this repo)
and forget about them:

```
machine 192.168.1.110 login <user> password <the password>
```

```sh
chmod 600 ~/.netrc
```

`~/.netrc` supplies the username as well as the password, so with an entry there
`cargo run --release -- 192.168.1.110` needs nothing else.

Without a netrc entry the script prompts for the password, but it still has to
know the username, which it assumes is `admin`. If the light uses a different
one, say so:

```sh
cargo run --release -- 192.168.1.110 --user chack
export TLD_OTA_USER=chack        # or set it once for the shell
```

The password is never put on a command line and never read from an environment
variable, so it cannot end up in your shell history or in `ps` — it reaches curl
through a config file on standard input.

**If the password is lost**, hold the BOOT button on the ESP32 for five seconds.
The light reboots into its setup access point and forgets the password. Anyone
who can reach that button can already rewrite the Wi-Fi settings, so this gives
away nothing that was being protected.

### When it goes wrong

| What you see | What it means |
|---|---|
| `could not connect` | the light is off, on another network, or at a different address |
| `wrong password` | wrong password, or the right one with the wrong username — see `--user` |
| `the device refused; has a portal password been set on it?` | set one on the portal page first |
| `someone else is pushing to it right now` | wait for them to finish |
| `the image is too large for the slot` | the firmware has outgrown its slot; see `partitions.csv` |
| `came back running the image it had before` | it rolled back, because the new image did not do what the old one was doing — read the serial log with `espflash monitor` |
| the upload finishes, then a timeout | usually harmless: it rebooted before answering. Check with `scripts/ota-push.sh --check <host>` |

A bad image cannot strand the light permanently. Firmware pushed over the
network has to prove itself on the next boot by doing what the light was already
doing — an image delivered over the network has to get back on the network, and
one delivered through the setup access point has to bring that access point back
— and esp-idf reverts to the previous image if it does not. And USB always
works: `cargo run --release`.

### A note on scope

The password stops a stranger on the LAN from reflashing a traffic light. It is
HTTP Basic over plain HTTP on a hackspace network, not a security boundary you
should lean on for anything that matters.

## Development

```sh
cargo build --release                  # what CI builds
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --workspace -- -D warnings

# unit tests run on the host, not the esp32
cargo +stable test --target x86_64-unknown-linux-gnu --bin traffic-light-display

# the flashing scripts have their own tests, and need no hardware
scripts/ota-push.sh --self-test
scripts/test-ota-push.sh
```
