# Use Tcode from other devices

Tcode runs on the machine that holds your projects, provider tools and terminal
processes. You can open that machine from another desktop, a phone, a tablet or
a browser. Native apps reach the machine over **Traverse**: one encrypted
connection per device, direct when the network allows it and through a relay
when it does not. The browser client is a direct-access page served by
`tcode-headless` on the same machine or LAN. For a shorter introduction, see
[Use Tcode from other devices in the README](../README.md#use-tcode-from-other-devices).
The plan behind this model is [#376](https://github.com/Tryanks/tcode/issues/376).

## Concepts

| Term | Meaning |
| --- | --- |
| Machine | The computer where Tcode runs providers and terminals and stores projects and threads. It is the desktop app with **Let other devices connect to this machine** on, or `tcode-headless serve`. A machine is identified by a key it generates once and keeps in `traverse.json` in its data directory; the public half is its **machine id** (64 hex characters). |
| Device | A desktop, phone or tablet running the Tcode app that opens a machine. A device also has its own key, kept in `device.json` in its data directory. The machine keeps an allow list of device ids; every connection is authenticated by that key, so a device that is not on the list is refused before any application data flows. |
| Invitation | A `tcode://pair?…` link, shown as a QR code and copyable as text. It carries the machine id, a random 16-byte secret, the Traverse instances the machine publishes to, its current relay and its UDP port — never an IP address or the machine's name; the format is [The invitation link](pair-link.md). Scanning or pasting the link is the whole pairing: an invitation lasts five minutes, admits one device, is replaced by the next one and is invalidated after five wrong secrets. The machine enforces all of this; Traverse never sees an invitation. |
| Space | A named set of projects on one machine, with a reusable pairing link. A device paired through that link becomes a member and sees only the space's projects and threads. The owner manages the projects and members. |
| Group | Machines that admit the devices any of them admits, so a device paired with one member reaches every member. Off until a machine creates one; see [Machine groups](#machine-groups). |
| Traverse | The relay and lookup services a machine publishes to so devices off its network can find and reach it. A machine keeps a list of sources, each switched on or off: **Official** (always first, on by default) uses the relays and lookup service listed in the manifest bundled with Tcode; each **Self-hosted** source is your own `tcode-traverse` instance. With every source off the machine uses no service at all. Traverse sees only encrypted traffic; the machine authenticates devices itself. |
| Direct / Relay | How one live connection is carried. Direct means the two ends exchange UDP packets with each other; Relay means the packets go through a Traverse relay because no direct path was found. The connection banner and the machine's device list show which one is in use, and it can change while connected. |

Pair each device separately. Project files, provider processes and terminal
processes stay on the machine; devices receive the thread content and other
data they need to show and operate that work.

## Shared spaces

A space shares selected projects with trusted collaborators. Members run
commands with this machine's agents and permissions. This is scoped visibility
and control, not isolation: provider turns and terminals run under the machine's
account and can access what that account permits.

### Owner

1. Open **Settings → Remote → Shared spaces** on the machine, or from a
   full-access device connected to it, and create a named space.
2. Add existing projects in that section, or right-click a project or thread
   in the sidebar and choose **Share to «X»** for the space. Sharing a thread
   shares its project. Choose the checked item again to remove the project.
3. Copy the space link or let the member scan its QR code. The **Machines**
   page also shows the machine's spaces. A headless `serve` prints available
   space links at startup.
4. Turn off **Link enabled** to pause new joins, or regenerate the link to replace
   its secret. Manage members in the space's expanded row: move a device to
   another space or remove it. Removing a member offers to regenerate the
   link as well, with that option selected by default.

Adding or removing a project, moving or removing a member, or deleting a space
closes affected member connections. Devices reconnect with their current access;
a removed member has to pair again.

### Member

Pair with the space link by scanning or pasting it like any invitation. The
connection banner shows **Space «X» · machine name**. The sidebar contains only
the space's projects and threads, with no **Add project** action. Members can
create threads, send turns, and use terminals and git in those projects. They
can archive threads, but cannot delete threads or projects, change host
settings, providers, plugins or hosting, or use the preview tunnel. Settings
contains device-local pages and **Archived Threads**. Sidebar preferences and
read state stay on the device.

Each device belongs to at most one space in this version. A device already
paired with the machine, fully or through a space, is refused as **already a
member** and keeps the access it has; the owner can move a member to another
space. The pairing screen says **This device is already paired with that
machine. Connect to it from Machines; to join this space instead, ask the
owner.** A paused, dead or missing space link says **This space link is paused
or no longer valid. Ask the owner to turn it back on or send a new one.** A
wrong secret says **The machine rejected this invitation; create a new one
there and paste it here.** A connection
failure that found no path to the machine asks for the machine's IP
address (see [When nothing finds the machine](#when-nothing-finds-the-machine)).
A malformed link says **That is not
a Tcode invitation link.** Pairing errors leave the device without new access.

### Link lifecycle and stored access

A space link is reusable and does not expire after one join. It dies after
five cumulative wrong secrets; the owner regenerates it to accept new members.
Pausing or regenerating the link, including after it dies, affects only new
joins. Existing members keep access until the owner removes them, deletes the
space or revokes their device. A removed member can join again with the old
link if it remains enabled and valid.

`traverse.json` uses version 3 to store spaces and device access, and version
4 once the machine is in a group, which adds the group; an older build reads
only version 3. Loading a version 2 file migrates existing paired devices to
explicit full access and starts with no spaces. Existing devices do not become
space members as part of the migration.

## Machine groups

A group is for one person's machines: a headless machine on a NAS, a desktop,
a laptop. Pair a phone with any of them and it reaches all of them; remove a
lost phone on any of them and every one refuses it. No machine is in charge.
The machine that started the group can stop or leave, and the others carry on.

**The risk is yours to take.** Every member can admit devices into every
other member, so a compromised member, or a leaked invitation, reaches the
whole group. Groups are off until you create one; leave them off if you do
not need them.

Inside a group there is one way in: a member's invitation. A device that
pairs with it joins the group, and another machine joins with the same kind
of invitation. Every full-access device of a member is a member; only space
members stay with their own machine.

### Create or join

1. On the first machine, open **Settings → Remote → Machine group** (on a
   headless machine, in its logged-in browser) and create a group. Tcode
   first lists the full-access devices paired with this machine: every
   member of the group will admit them.
2. On each other machine, with hosting on, choose **Join group** in the same
   section and paste an invitation from a machine already in the group (its
   **Machines** page, or **Settings → Remote** in a headless machine's
   browser). The join spends that invitation, as a device's pairing would,
   and lists the devices this machine brings first. If the join finds no
   path to that machine, type its IP address, as when pairing a device.
3. A desktop joins together with its own window, so the window reaches every
   member from **Machines**.

While a machine is in a group, its invitation says so and names the group;
the link carries the group's id.

### Pair a device

Scan or paste any member's invitation: the device is paired with that
machine, and the machine tells the others. The device saves every member
under **Your machines**. A space link never brings a device into the group.

**Accept new devices** governs the group too. While it is off on a machine,
a device another member admits is not admitted there; it is listed in that
machine's group section, with **Admit**, and stays out until you admit it.

### Remove and leave

**Remove** under **Connected devices** removes a full-access device from
every member: each closes its connections and refuses it from the moment it
hears of the removal. A member that is offline hears it when it next reaches
another member, and admits the device until then. Only a new pairing brings
the device back.

**Leave group** on a machine tells the members and forgets the group. The
machine keeps every device on its allow list, and the other members keep
theirs. A member can also remove another machine from the group; that
machine leaves the same way the next time it hears of it.

Members reach each other over `tcode/group/1` on their own UDP port: at the
relay and addresses each reports for itself and the ones a connection last
used, and through the lookups of the Traverse instances the dialling machine
publishes to. They exchange the group when it changes and every five minutes.

## Use the desktop app as the machine

1. Open the desktop app on the machine where your projects and agent CLIs live.
   Configure the providers and add your projects there.
2. Open **Settings → Remote**. Set **Machine name** (shown to devices) and
   choose the **Traverse** sources: switch **Official Traverse** on or off,
   and add a **Self-hosted Traverse** by its base URL with **Add self-hosted
   Traverse**; each added instance has its own switch and can be edited or
   removed. Turning every source off leaves only the LAN and an address typed on
   the device. A switch, addition or removal applies at once and an
   edited URL when its field is left, restarting the endpoint if hosting is
   already on.
3. Turn on **Let other devices connect to this machine**. The desktop binds its
   Traverse endpoint to UDP port `47420` on all IPv4 and IPv6 interfaces (IPv4
   only where IPv6 is unavailable), so the invitation's port, firewall rules
   and the LAN lookup of paired devices survive restarts, and advertises the
   port on the local network as `_tcode._udp` (see
   [Finding the machine again](#finding-the-machine-again)). Allow that UDP
   port through the machine's firewall for direct LAN connections; a relayed
   connection needs only outbound access.
4. Open **Machines** (the sidebar entry under the search field). Under **Let
   other devices connect**, scan the QR code with the phone, or use **Copy
   invitation link** and paste the link into Tcode on the other device. The QR
   is redrawn with the machine's current relay while the invitation is valid,
   so a link copied a minute later still works. Use **New
   invitation** when one expires or you need to add another device. While
   hosting is off, or **Accept new devices** is off, this section says so and
   leads to **Settings → Remote**.
5. Check **Connected devices** in **Settings → Remote** after pairing. Each
   row shows the device's name and operating system (for example
   `Xiaomi 15 · Android 15`) and **LAN**, **Tunnel**, **Relay (‹region›)** or
   **Offline**; the list refreshes every two seconds. Use **Remove** to
   withdraw a device's access.

Hosting keeps running while this window is connected to another machine:
devices that opened this machine continue working, and **Let other devices
connect to this machine** stays available. The desktop app accepts native
devices only; it does not serve the browser app.

## Run Tcode without the desktop app

For a NAS or homelab, use the published Docker images and the commented
[Compose template](../docker-compose.yml). The [Docker guide](docker.md)
covers persistence, agent selection, login and `PUID`/`PGID` ownership.

### Install and start

1. Download an archive from
   [Releases](https://github.com/Tryanks/tcode/releases). Names follow
   `tcode-headless-<version>-<platform>-<arch>`: Linux uses `.tar.gz`, macOS and
   Windows use `.zip`; platforms are `linux`, `macos`, `windows`, and
   architectures are `x64` or `arm64`. Windows contains `tcode-headless.exe`.
2. Extract the archive and install the executable. For Linux x64, replace
   `VERSION` with the release version without its leading `v`:

   ```sh
   tar -xzf tcode-headless-VERSION-linux-x64.tar.gz
   install -d "$HOME/.local/bin"
   install -m 755 tcode-headless "$HOME/.local/bin/tcode-headless"
   ```

3. Install and authenticate your agent CLIs under the account that will run
   Tcode. Ensure that account's `PATH` includes them and that it can access your
   project directories.
4. Start Tcode with a persistent data directory:

   ```sh
   "$HOME/.local/bin/tcode-headless" serve \
     --name build-server \
     --data-dir "$HOME/.local/share/tcode-host"
   ```

   Startup prints `Machine id: …`, then the invitation: its remaining
   lifetime, `Relay: …` (or `Relay: none (LAN only)` with Traverse off), the
   machine's current `Addresses:`, the `tcode://pair?…` link and its QR code.
   The addresses are for you to read, not part of the link: type one on a
   device that cannot find the machine (see
   [When nothing finds the machine](#when-nothing-finds-the-machine)).
   With Traverse on, `serve` waits up to five seconds for a relay before
   printing the invitation, so the link names one and works off the LAN. It then prints
   **Set a password on first open** or **Password protected** and
   `Browser: http://127.0.0.1:47420/`, the browser page (see
   [From a browser](#from-a-browser)).

   `--traverse official` (the default) or `--traverse https://traverse.example`
   names a Traverse source; repeat the option to publish to several
   (`--traverse official --traverse https://a.example --traverse
   https://b.example`), or give `--traverse off` alone for none; see
   [Traverse](#traverse). `--browser-listen ADDR:PORT` binds the browser page
   elsewhere (`--listen` is accepted as an alias); `serve` refuses a bind
   beyond loopback until a password exists and exits before starting anything
   else. To preset the browser password, supply `serve --password PASSWORD`
   or set `TCODE_PASSWORD` in the service environment; the CLI option takes
   precedence. A preset updates the password on startup and keeps existing
   browser tokens.
5. The headless machine binds its Traverse endpoint to UDP port `47420`
   (`serve --port PORT` binds another; `serve` fails at startup when the port
   is taken, for example by the desktop app hosting on the same machine) and
   prints it as `UDP port:`. The port is fixed so that the invitation's port,
   firewall rules and the LAN lookup of paired devices survive restarts, and
   the machine advertises it on the local network as `_tcode._udp` (see
   [Finding the machine again](#finding-the-machine-again)). For direct LAN
   connections allow inbound UDP to that port; a relayed connection needs
   only outbound access.
6. To see the invitation again while it is still valid, run in another shell
   on the same machine:

   ```sh
   "$HOME/.local/bin/tcode-headless" pair --data-dir "$HOME/.local/share/tcode-host"
   ```

   `pair` reads `invitation.json`, which `serve` keeps current in the data
   directory: written whenever an invitation is minted, at startup or from a
   paired device, and removed once it is used, expires, pairing is turned off
   or `serve` stops. `pair` cannot mint one: when it reports no valid
   invitation, create one from the logged-in browser's **Settings → Remote →
   New invitation** or restart `serve`.
7. The logged-in browser's **Settings → Remote** shows this machine's
   invitation QR and link, creates a new invitation, lists connected devices
   with their path, and removes devices: the native devices, then the
   browsers logged in to this machine. A native device's own **Settings →
   Remote** configures that device's hosting, not this machine's.
   **Accept new devices** controls whether the machine accepts pairings; it
   defaults to on and is saved in `traverse.json`. Turning it off discards the
   current invitation and does not disconnect already paired devices.
8. Open this machine from another device using the instructions below. To
   prepare projects with the local desktop UI, stop `tcode-headless` first and
   open the desktop app with the same data directory. Do not run two local Tcode
   processes against the same directory.

On Unix, Ctrl-C requests shutdown and a store flush. The CLI reference is:

```sh
tcode-headless --help
```

Add the installed directory to your `PATH` to use that short command. Help is a
top-level option; the subcommands do not accept `--help`.

To change or reset the browser password, stop the headless process, then run:

```sh
tcode-headless set-password --data-dir /path/to/tcode-host --password NEW_PASSWORD
```

`TCODE_PASSWORD` also supplies the password for this command. Existing browser
tokens remain valid by default, so logged-in browsers stay logged in. Add
`--revoke-tokens` to invalidate every browser token, then restart the host.
Update any service preset too, or it will replace the password on the next
startup. Do not run this command against a data directory used by a running
process. The password and browser tokens are separate from native pairing:
`set-password` never changes the machine key or the device allow list.

### Data directory

The `tcode-headless --data-dir` option takes precedence over `TCODE_DATA_DIR`.
Without the option, `TCODE_DATA_DIR` selects the store; otherwise Tcode uses
`~/.tcode` on every platform. This includes settings,
threads, the machine key and allow list (`traverse.json`), browser login
records (`remote.json`), the cached Traverse manifest and the current
invitation. It does not move project working directories into the store.

For example, this selects the same store as the explicit path above:

```sh
TCODE_DATA_DIR="$HOME/.local/share/tcode-host" \
  "$HOME/.local/bin/tcode-headless" serve --name build-server
```

Keep this directory across restarts and back it up: the machine key is the
machine's identity, and a device stays paired with that key, not with an
address. Use a separate directory for a separate machine identity. The desktop
app also honors `TCODE_DATA_DIR`, including as a device when adding or
connecting to a machine.

Earlier versions kept the data directory in the platform app-data directory
(`~/Library/Application Support/tcode`, `~/.local/share/tcode`,
`%APPDATA%\tcode`). When neither the option nor `TCODE_DATA_DIR` is given, the
first start of this version moves that directory into `~/.tcode`, provided
`~/.tcode` holds no data yet; `serve` prints its progress, Ctrl-C stops it
without losing anything, and the next start continues. `LEGACY_TCODE_DATA_DIR`
names another directory to move from, and moves it even into a data directory
chosen with `--data-dir` or `TCODE_DATA_DIR`. A name present in both
directories stops the move before anything is moved. Until the move is done,
`set-password` refuses to write into the new directory.

### Run with systemd

On a Linux system with systemd, create a user unit at
`~/.config/systemd/user/tcode-headless.service`. This example uses the
executable and data directory above. Extend `PATH` with the absolute directories
containing your agent CLIs; a service does not read your interactive shell
setup.

```ini
[Unit]
Description=Tcode without the desktop app

[Service]
Type=simple
WorkingDirectory=%h
Environment=PATH=/usr/local/bin:/usr/bin:/bin
ExecStart=%h/.local/bin/tcode-headless serve --name build-server --data-dir %h/.local/share/tcode-host
Restart=on-failure
RestartSec=5
KillSignal=SIGINT
KillMode=mixed
TimeoutStopSec=60

[Install]
WantedBy=default.target
```

`SIGINT` uses Tcode's shutdown-and-flush path. Stop any foreground Tcode process
using the same data directory, then load and start the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now tcode-headless.service
journalctl --user -u tcode-headless.service -f
```

Treat the journal as sensitive: startup prints the invitation link, and the
link is the secret. Configure user lingering through your system administrator
if this service must run after logout. Reprint the invitation with the same
`pair` command above while it is valid.

### Build with browser support

Release archives include the browser bundle. For a source build, run these
commands from the repository root:

```sh
rustup target add wasm32-unknown-unknown
# The CLI must match the wasm-bindgen version locked in Cargo.lock.
cargo install wasm-bindgen-cli --locked --version "$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"$/\1/p;q;}' Cargo.lock)"
crates/web/build.sh
cargo build -p tcode-headless --release --features web
```

The script optionally uses `wasm-opt` if installed. Build the bundle before the
`tcode-headless` binary: the `web` Cargo feature embeds `index.html`,
`auth.mjs`, `tcode_web.js` and `tcode_web_bg.wasm` from `crates/web/dist`. It
is a build option, not a `serve` flag. The resulting executable is
`target/release/tcode-headless`.

Without `web`, the browser listener answers static requests with 404 while
native devices still pair and connect over Traverse. The browser runs the same
shell as the desktop app; it lays itself out from the canvas size, so a wide
tab gets the desktop split and a narrow one gets the compact stack.

## Open a machine

### From another desktop

1. Open **Machines** on this device — the sidebar entry under the search field,
   or the opening screen when no machine has been added yet.
2. Under **Add a machine**, choose **Paste an invitation link** and paste the
   machine's invitation link (`tcode://pair?…`). A link that parses is sent at
   once: the page shows **Connecting to the machine…**, and when the machine
   admits this device the window opens that machine's projects and threads.
   A failure is shown under the field, and **Connect** retries. When nothing
   found the machine, the page also asks for its IP address (see
   [When nothing finds the machine](#when-nothing-finds-the-machine)).
3. Choose another machine under **Your machines** to switch again;
   **This machine** restores the local workspace, and a machine row's
   **⋯ → Disconnect** leaves without removing it. Switching closes only this
   device's link, not Tcode on either machine. Opening **Machines** by itself
   does not change the connection.

You can also pair from the desktop executable. Replace the link with the
machine's current invitation:

```sh
tcode --pair 'tcode://pair?v=3&id=…&secret=…&relay=…&port=47420'
```

This saves the machine in this device's `hosts.json` and prints its machine
id. Replace `MACHINE_ID` below with the printed id:

```sh
tcode --connect MACHINE_ID
```

`--connect` accepts a saved machine id, not an address. Use the same device
data directory for `--pair` and `--connect`.

### From a phone or tablet

1. Install the Android debug APK or re-sign and install the unsigned iOS IPA.
2. The app opens on **Machines** — the same screen every device has. Choose
   **Scan a QR code** and point the camera at the invitation on the machine, or
   **Paste an invitation link**. Allow camera access when the system asks.
   Native QR scanning uses AVFoundation on iOS and CameraX/ML Kit on Android;
   a simulator's permission and cancel flows do not prove that real camera
   recognition works.
3. A scanned or pasted invitation pairs and connects in one step: the page
   shows **Connecting to the machine…** and then opens the machine's threads.
   A failure is shown under the rows that started the attempt; when nothing
   found the machine it asks for the machine's IP address there.
4. Open a thread from the list, or use **+** to start one. Read replies, send or
   queue a message, steer a running turn, stop it, and answer approvals — the
   same views the desktop shows, laid out for the width.
   The **+** at the left of the message field opens **Photo library**, the
   system picker (PHPicker on iOS, the photo picker on Android 13 and later,
   the document picker below that). Images are fitted on the device before
   they leave it: no longer than 2048 pixels on either side, HEIC converted to
   JPEG, then stored in the thread's attachments directory on the machine.
   Over a LAN there is no further limit. Across the internet the device
   applies **Attachment limit over the internet** from its own settings
   (2 MB unless changed): a larger image asks before going over a direct
   path, and is refused over a relay, because a relay is shared with everyone
   on the service and the connection's other messages would wait behind it.
5. **Settings** is the full settings page, not a reduced copy. **Back** returns
   list → machines, which disconnects; leaving a thread keeps the connection.
   You connect to one machine at a time.

A tablet wide enough for the split gets the split, and rotating it back to
portrait returns to the stack with the same thread and draft. A desktop window
never enters the stack, however narrow it is.

### From a browser

The browser client is for direct access: a tab on the machine itself, or on
its LAN when you bind the listener there. It does not use Traverse and has no
relay; to reach a machine from elsewhere, use the native app.

1. Open the `Browser:` link printed by `tcode-headless`. By default the page is
   served on `http://127.0.0.1:47420/`, reachable only from the machine. To
   use it from another device on the LAN, start with an explicit bind such as
   `--browser-listen 0.0.0.0:47420` and open
   `http://<machine LAN address>:47420/`. `serve` refuses that bind until a
   password exists (`--password`, `TCODE_PASSWORD` or `set-password`), so a
   LAN never sees the first-open setup page.
2. On first open, set a password with at least eight characters and confirm it.
   Later visits without a saved token show **Log in**. A successful login saves
   a browser token and opens Threads; a valid stored token skips the form.
3. A rejected or revoked token returns to the login form. Five wrong passwords
   lock browser login for five minutes. Native pairing is unaffected by this
   lockout.
4. **Settings → Remote** manages this machine's native pairing: **Accept new
   devices**, the current invitation QR and link, **New invitation**, and
   removing paired devices. Phones and other desktops always pair with an
   invitation, never with the browser password. The device list shows the
   native devices, then every browser logged in to this machine (browser ·
   operating system, **Direct** while its tab is connected). **Remove** on a
   browser invalidates its token and closes its socket; that tab returns to
   the login form.
5. The address is fixed to the page's origin: **Machines** lists the machine
   that served the page and offers no way to add another. Open another
   machine's URL to use that machine. Resizing the page switches between the
   shared wide and compact layouts.

The browser stores machines and tokens in this origin's `localStorage` under
`tcode.hosts`, the last machine under `tcode.last_host`, its device id under
`tcode.device_id` and pending writes under `tcode.outbox.<machine id>`.
Clearing site data means you must log in with the password again. The page
and the listener ship together: the page opens its WebSocket with protocol
version 5, and a listener of another version answers **Protocol mismatch**
until the page is reloaded from the running `tcode-headless`. The page is
plain HTTP and its WebSocket is `ws://`; anyone who can capture that LAN
traffic can read the password, the token and the work. If you put a TLS
proxy or tunnel in front of it, that is your own setup: the WebSocket follows
the page's scheme, so `https://` pages use `wss://`.

### Device preferences

Appearance, language, device name and the internet attachment limit belong to
this device, not the machine:
an explicit choice on this device overrides the connected machine's replicated
setting, and restoring that row reveals the machine's setting again. Native apps
keep these preferences in `mobile.json` next to `hosts.json` in their own data
directory; the browser keeps them in `localStorage`. iOS seeds the device name
from the device name, Android from the device model; the name and operating
system are sent to the machine when pairing and connecting, and the machine's
device list follows them.

## Traverse

A machine has a list of Traverse sources, each on or off: **Official**,
always first and only ever switched off, and any number of **Self-hosted**
instances by base URL. On the desktop it is in **Settings → Remote →
Traverse**; headless repeats `serve --traverse official|<url>`, or takes
`--traverse off` alone. The machine's relay list is the union of every
enabled source's relays — iroh picks its home relay among all of them by
latency — and it publishes its lookup record to every enabled source's pkarr
URLs. Each source's manifest is loaded and refreshed on its own: one that
cannot be fetched is left out until it arrives, without holding back the
others. Devices need no Traverse setting: the invitation lists every enabled
source (no entry when only the official service is on, `off` with none
enabled; see [The invitation link](pair-link.md)), and each saved machine
keeps its own list, so one phone can use an official-Traverse machine and a self-hosted one
at the same time. A device's own endpoint uses the relays and lookups of the
instances its saved machines publish to, and of a machine it is pairing with:
a device whose machines are all Off has no relay and contacts no service, and
the official manifest is loaded only while some machine uses the official
service. Adding or removing a machine applies this to the running endpoint.

While hosting, each enabled source's row in **Settings → Remote** shows how
it is doing: a dot and a word for its manifest (**live** fetched by this run,
**cached** from an earlier run, **bundled** with Tcode, **failed** with none),
and a one-line summary such as `3 relays · best 42 ms (eu) · lookup ok`.
Opening it lists every relay with the latency the machine last measured (`—`
before it has), marks the home relay and shows why its connection fails if it
does, and gives each pkarr URL's lookup check: shortly after every publish and
every five minutes the machine reads its own record back from that URL, and
the check fails if the record is missing, names another relay or has not been
rewritten for longer than the republish interval. `tcode-headless serve`
prints the same summary, one `Traverse <source>: …` line per source, once the
sources settle and again whenever a manifest, home relay or lookup outcome
changes.

A Traverse instance describes itself with a JSON manifest: a list of relays
(`url`, optional `quic_port` and `region`) and a list of pkarr lookup URLs.
Relay and pkarr URLs must be `https`. The machine builds its relay list from
the manifest and publishes a signed record of where it can be reached to
every pkarr URL in it; a device resolves the machine id through the same
pkarr URLs. Lookup is pkarr over HTTPS only; there is no DNS lookup. A
refreshed manifest is applied to the running endpoint: relays that
disappeared are removed, new ones added, lookup services rebuilt.

| Source | Machine | Devices | The service sees |
| --- | --- | --- | --- |
| **Official** (default) | Uses the manifest bundled with Tcode, refreshed from the repository. At the time of writing it lists n0's public relays (`*.relay.n0.iroh.link`, regions `na-east`, `na-west`, `eu`, `ap`, QUIC port 7842) and n0's pkarr relay (`https://dns.iroh.link/pkarr`). These are n0's infrastructure: n0 states that the public relays are rate-limited and offer no uptime guarantee, and that the lookup service is fine for production when its performance is acceptable. | A device with a machine on the official service uses the same relay list for its own home relay and the same lookup service to resolve machines. There is no device-side switch. | Relays see machine and device ids, the encrypted connection and its volume. The lookup service stores, per machine id, the machine's signed record: its relay URL only, republished every five minutes; direct addresses are filtered out before publication and are exchanged over the encrypted connection instead. Anyone who knows a machine id can read that record. Devices publish nothing. |
| **Self-hosted** | Fetches `<base>/relays.json` from your instance and uses its relays and pkarr store. With nothing cached yet, hosting waits for one fetch; if the instance is unreachable it starts anyway without that instance's relays and lookup — the LAN and the other enabled sources still work, and with no other source the invitation names no relay — and applies the manifest to the running endpoint once a background refresh (every five minutes) fetches it. It never falls back to the official service. | A device takes the base URL from the invitation, fetches the same manifest, and adds that instance's relays and lookup to what its other machines brought. Until that manifest loads the device has no relay and no resolver for that instance, only the saved addresses and the LAN lookup. Its home relay is chosen among all of them; a device whose machines are all self-hosted never contacts the official service. | Your instance sees what the official one would. The official service is not used for this machine; it sees the device's end only if the device also has a machine on it. |
| **All off** | No relay and no lookup service: the endpoint publishes nothing beyond its LAN advertisement and dials nothing but direct addresses. The invitation names no relay (`Relay: none (LAN only)`). | The device finds the machine through the LAN lookup, or at the IP address typed when nothing found it, and dials the addresses it learned on later connections. This machine brings no relay to the device's endpoint; with no other machine on a service, the device has none. | Nothing about this machine. |

A device that connected successfully saves how it reached the machine —
the direct addresses that worked, newest first, up to 16, and the relay —
from the pairing connection on, so the next launch starts from what worked
last. On the same network a machine whose address changed is found again
without any service; see [Finding the machine again](#finding-the-machine-again).

### Offline, unreachable or rate-limited

- **The manifest cannot be fetched.** The official mode keeps the cached copy
  (`traverse-manifest-official.json` in the data directory) or the bundled one;
  a fetched manifest stays fresh for one hour, and after a failure the next
  attempt waits five minutes, so an offline machine does not pay a network
  timeout on every check. A bundled manifest newer than the cache wins. A
  self-hosted machine keeps its cached copy; with no cache it hosts without
  a relay until the manifest arrives, see the table.
- **No route to a relay.** The machine still hosts; the invitation names no
  relay, and devices on the same network connect directly.
- **A relay is rate-limited or refuses a connection.** Connections fall back
  to whatever path remains: a direct path if one exists, another relay from
  the manifest otherwise. Tcode does not show a separate error for this; the
  connection banner shows **Reconnecting** with the failure reason until a path
  works. A self-hosted instance is the way out of another operator's limits.
- **The lookup record is stale or missing.** A device keeps dialling the relay
  and addresses it saved from the last connection, so a machine that has not
  moved is still reachable without the lookup.

### Self-hosting Traverse

`tcode-traverse` is one binary that is a complete Traverse instance: an iroh
relay with QUIC address discovery on UDP 7842, a pkarr store for machine
lookup, and the `relays.json` manifest a machine reads. It is the same server
for any operator. Releases publish it as
`tcode-traverse-<version>-<platform>-<arch>` archives next to `tcode-headless`.
Configuration, TLS modes (Let's Encrypt, manual, reloading, or plain HTTP
behind your own proxy), the pkarr store limits, metrics, the optional region
lock and a Docker build are in the
[server README](../crates/traverse-server/README.md).

Point a machine at it with **Add self-hosted Traverse** and the base URL on
the desktop, or `tcode-headless serve --traverse https://traverse.example.org`.
The official source stays on unless you switch it off (headless: list
`--traverse official` too to keep it), so a machine can publish to both.
Nothing is configured on devices; they take the URL from the invitation.

## Remote Preview routing

Windows, Linux and Android embedded previews use the connected machine's network for
all HTTP and HTTPS traffic. `http://localhost:5173/app` opens the dev server on
the machine, even when it listens only on loopback. The URL stays unchanged in
the address bar, redirects, scripts and `preview_status`. No dev-server port
needs to be exposed. Open-in-system-browser still opens on this device and
cannot use the embedded preview's routing.

Preview traffic rides the attachment's own Traverse connection: each browser
connection becomes one tunnel that names its `host:port`, and the machine
dials that address the way any local program would. There is no separate proxy
port, no proxy credential and nothing to configure in hosting settings; the
paired, end-to-end encrypted connection is the authority. Plain HTTP is
forwarded; HTTPS stays an opaque tunnel with certificate validation performed
by the client webview, without TLS interception. The machine connects directly
to destinations; OS routing and global TUNs apply. Tcode does not read
upstream-proxy environment variables or system proxy settings.

One attachment holds at most 56 tunnels at once. Dialling a destination times
out after 10 seconds, and a tunnel idle for 60 seconds is closed. Revoking a
device or stopping hosting closes its connection and every tunnel on it; while
the attachment is reconnecting, opening a tunnel fails at once, and after it
reconnects new tunnels use the new connection. Connection logs contain the
destination host and port, never request bodies. For headless diagnostics use
`RUST_LOG=tcode_traverse=info`.

Android requires WebView's `PROXY_OVERRIDE` capability. It removes implicit
localhost bypasses, waits for the override before navigation, and clears it when
the attachment's views are destroyed. The override is process-wide, so embedded
preview views belong to one attachment.

macOS and iOS attached Preview use per-port forwarding for HTTP(S) loopback
URLs: `localhost`, IPv4 loopback addresses and `::1`. WebKit applies a website
store's proxy configuration to public destinations but bypasses destinations on
the viewing device's own interfaces, so a proxy cannot keep loopback on the
machine. The requested host and port are sent unchanged in the tunnel's opening
line. The viewing device allocates its own loopback port, so a viewer service at
the remote port cannot collide with it. `localhost` reserves both IPv4 and IPv6
listeners at that port. Numeric loopback URLs need that same address to be
bindable on the viewing device; unassigned
addresses such as `127.0.0.2` can report an allocation error. Tcode does not
add loopback aliases to the OS. No OS DNS, hosts file or proxy settings are
changed. Public/LAN destinations are **direct from the viewer**, as the Preview
note explains; this is a remote dev server workflow, not arbitrary remote
browsing.

Each browser slot owns its forwarding table and a separate nonpersistent WebKit
website store. Relative assets and WebSockets derived from the page's actual
location use the same forwarded port. Top-level loopback links/redirects pass
through the same mapper, including redirects to another port. The native request
is retained when changing its URL; HTTP bodies, site authorization, WebSocket
frames and TLS records are not interpreted by the forwarding transport. Closing
the preview or changing attachment drops listeners and accepted tunnels; merely
hiding a retained preview keeps them alive. A new attachment receives new routes
and a fresh store.

The address bar, copy and stored URL retain logical remote intent. Native
back/forward/reload reuse the live mappings. `preview_status.url` and
`preview_wait_for` use the logical remote URL; `actual_url` is added when it
differs. `preview_evaluate`, page scripts and `window.location` see the
**actual local URL**. Open externally uses that live mapped URL and stops
working when the owning slot closes. Without a suitable live route, it reports
that Preview must open the page first, rather than opening the viewer's
unrelated original-port service.

Mapping preserves the requested hostname but changes its port. The HTTP Host
header and browser Origin therefore contain the viewing port. HTTPS retains the
hostname/SNI and normal WebKit certificate validation; a development certificate
must already be trusted and cover that hostname. Port-sensitive host/origin
allowlists, CORS, CSP and OAuth callbacks can need app configuration. Cookies are
not isolated by port, which is why attached slots use separate website stores.
There is no blanket certificate bypass or credential injection into site auth.

Absolute subresource URLs, hardcoded API/HMR ports and worker-owned endpoints are
not automatically rewritten. Configure them to derive their endpoint from the
actual page location or an explicitly resolved live forward; entering an
additional page URL creates its mapping but does not rewrite application code.
Navigation, transport and authentication failures use Preview's existing error
surface and automation errors. Local macOS Preview retains ordinary direct
networking and its existing store.

Windows uses WebView2's proxy configuration with implicit loopback bypass
disabled; Linux gives WebKitGTK's ephemeral context a custom proxy with no
ignored hosts; Android uses the WebView proxy override. All three point the
engine at
an attachment-owned loopback HTTP proxy that speaks `CONNECT host:port` and
absolute-form requests. The proxy opens one tunnel per browser connection,
rewrites an absolute-form request line to origin form, drops its own
hop-by-hop headers and copies everything else verbatim, WebSocket upgrades
included. It serves one request per connection and answers ambiguous
framing (chunked plus `Content-Length`, unknown transfer codings, origin-form
requests) with `400`. Unsupported proxy facilities fail closed; there is no
direct-network fallback. Closing the attachment cancels the listener and
active connections. macOS and iOS keep their per-browser URL mapping over the
same tunnels.

When the main connection reconnects and its saved addresses change, existing
Preview browsers follow the new connection: their local proxy/forwarding ports,
URLs, history and website stores survive, connections made over the old
connection end, and new ones use the replacement. Tcode does not replay
interrupted requests or reload pages automatically: use the existing Reload
action if a page was interrupted. Another machine cannot retarget an existing
attachment's Preview.

The browser client cannot override its browser's proxy.

## Security

### What is encrypted, and what a relay sees

Every native connection is a QUIC connection between the device key and the
machine key, encrypted end to end. A relay forwards packets between two ids;
it sees the ids, timing and volume, not the content. A direct connection on
your LAN is encrypted the same way. Traverse is never part of authentication:
the machine accepts a connection only from a device id on its allow list, and
answers anything else with `unpaired` and closes it.

The lookup service holds a public, signed record per machine: the machine's
relay URL, never its IP addresses. It tells anyone who knows the machine id
which relay reaches it; it does not let them connect, because the machine
refuses unpaired devices. On the local network the machine's DNS-SD
advertisement likewise tells anyone on the LAN its machine id, name and UDP
port, and nothing more: a connection still needs a paired device key.

The browser client is different: it is plain HTTP on the machine's own
listener, protected by the password and a bearer token. Keep it on loopback or
a trusted LAN.

### Keys and files

The machine's `traverse.json` holds its secret key, its name, the allow list
(device id, name, operating system, first-connection time), the **Allow other
devices** switch and, in a group, the group's machines and devices.
`remote.json` holds the browser side: the id and name browsers record this
machine under, the password hash (randomly salted PBKDF2-HMAC-SHA256, 600,000
iterations) and one record per logged-in browser with its token hash. The
password and the raw tokens are never stored, and nothing in `remote.json` is
part of the machine's identity. A headless machine upgraded from the previous
transport therefore gets a new machine id the first time it starts — the old
`remote.json` seed is not adopted — and every device pairs again with a new
invitation; the browser password and browser logins are kept.

A device's `device.json` holds its secret key, name and platform. `hosts.json`
holds the machines it paired with: id, name, Traverse URL, relay, direct
addresses, last connection time and group — routing hints, not credentials.
Pending writes are in `outbox-<hex of machine id>.json`. Phone records are in
the app's private data directory; Android uses its `filesDir`.

On Unix, Tcode writes these files with mode `0600`. That is a filesystem
permission, not encryption. **A stolen `device.json` is that device's
access:** whoever holds the key can connect to every machine that lists the
device until the device is removed on each machine, or once on any member of a
group. **A stolen `traverse.json` is the machine's identity:** its holder can
run a machine that paired devices would trust and, in a group, admit devices
into every member. There are no per-device project permissions or read-only
device roles; a paired device has the same access as the machine's own window,
including Preview tunnels to the machine's loopback and network.

### Revocation

On the machine, use **Settings → Remote → Connected devices → Remove**
(on the desktop, or in the logged-in browser of a headless machine). The device is
taken off the allow list and saved, its live connections are closed at once,
and a new connection from it is refused from that moment. The device shows
**Access rejected · Pair again** and stops retrying. Pairing it again needs a
new invitation. A member of the machine's group is removed from every member
of the group; see [Remove and leave](#remove-and-leave).

A browser logged in to a headless machine is listed after the native devices
in the logged-in browser's **Settings → Remote**; **Remove** there
invalidates its token and closes its socket at the next message or
keep-alive, and that tab returns to the login form. Native devices do not
list browsers; `set-password --revoke-tokens` on the stopped host removes
them all.

**Remove** under **Your machines** on a device only forgets that device's
record; it does not withdraw anything on the machine.

There is no `tcode-headless` remove subcommand. To remove a device without a
paired client or the browser, stop `tcode-headless`, back up `traverse.json`,
remove the matching entry from its `devices` array, preserve its permissions,
and restart. Do not edit that file while Tcode is running: it holds the allow
list in memory and writes it back.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| **That is not a Tcode invitation link** | The pasted text is not a complete `tcode://pair?v=3&…` link (see [The invitation link](pair-link.md)). A link from an older Tcode is not accepted either. Copy the link again from the machine or scan the QR. |
| **The machine rejected this invitation** | The invitation expired (five minutes), was already used, was replaced by a newer one, or five wrong secrets invalidated it. Create **New invitation** on the machine or restart `serve`; get a separate invitation for each device. |
| **This machine is not accepting new devices** | Turn on **Accept new devices** on the machine (desktop **Settings → Remote**, or the logged-in browser of a headless machine). |
| Pairing asks for **Machine's IP address** | No path to the machine opened within 20 seconds: not through its relay, its Traverse lookup or the LAN lookup. The invitation is still valid. Check that Tcode is running, that the two are on the same network or the machine has a relay (`Relay:` in the `serve` output), and that the machine's UDP port is not blocked on the LAN; then type the machine's IP address in that field (see [When nothing finds the machine](#when-nothing-finds-the-machine)). |
| **Reconnecting to ‹machine›… (attempt N)** stays up | The device keeps trying the saved relay and addresses with growing delays, and on every attempt looks for the machine on its own network (see [Finding the machine again](#finding-the-machine-again)). If the machine moved to another network with Traverse off and the device did not move with it, pair again with a new invitation. Direct paths need UDP between the two ends; a relayed connection reaches the relay over HTTPS (TCP), so it still works where UDP is blocked, as long as the machine publishes to a relay. |
| **Access rejected · Pair again** | The machine no longer lists this device (removed, or the machine's data directory was replaced). Pair again with a new invitation if access is intended. |
| **Protocol mismatch · Update the app** | The machine and the device run different protocol versions. Update both. |
| **Connected … · Relay** when both are on the same LAN | Hole punching has not found a direct path yet, or the LAN blocks UDP between the two. The path can switch to **Direct** while connected; the machine's fixed UDP port (`47420`) allowed through its firewall helps. |
| `tcode-headless pair` says there is no valid invitation | The last one was used or expired, or `serve` is not running. Create a new invitation from the logged-in browser's **Settings → Remote**, or restart `serve`. |
| A self-hosted machine hosts with `Relay: none (LAN only)` | The instance's `relays.json` could not be fetched and nothing is cached; the machine is reachable on the LAN only until a background refresh (every five minutes) gets the manifest. Check the URL, the instance and its certificate; Tcode does not fall back to the official service. |
| Browser password is wrong or forgotten | After five wrong attempts, wait five minutes. To reset it, stop the host and use `set-password`; add `--revoke-tokens` if logged-in browsers should lose access. |
| Browser page unreachable from another device | The listener binds `127.0.0.1:47420` by default. Start `serve --browser-listen 0.0.0.0:47420` and open the machine's LAN address; `serve` refuses that bind until a password is set. |
| Browser shows **Protocol mismatch** | The page in the tab is from another build than the running `tcode-headless`. Reload the page. |
| Browser shows 404 | Use a `tcode-headless` build with `web`. The desktop app and builds without the bundle do not serve the browser app. |
| Preview cannot load a dev server | Check that the dev server runs on the machine, hosting is on, the device is still paired, and its WebView supports proxy routing. Use `localhost` for a machine-loopback server. If a request was interrupted by a network move, wait for the main connection to recover and use Reload. |

**Syncing… / 同步中** means the workspace is waiting for its baseline: applied
Index and Settings snapshots, plus SessionStatus and SessionEvents for the
selected thread. A newly attached workspace shows neutral loading rows until
these arrive; it never labels an unknown list as empty. Cached lists and thread
content remain visible during reconnect, with Syncing until replay completes.
**Reconnecting · attempt N** appears immediately when a connection is lost,
with its failure reason. Retry delays grow exponentially from one to thirty
seconds with ±20% jitter (and a final 30 s cap). Only a connection stable for
at least 30 s resets backoff; a quick handshake followed by another disconnect
does not. A network change reported by the device ends the current wait
early and tries again at once.

A connection attempt has 20 seconds to find a path and complete the handshake.
Once connected, both ends send QUIC keep-alives every 10 seconds and drop a
connection with no packets for 30 seconds. On top of that the device sends an
application ping after 10 seconds without a message from the machine and
reconnects if nothing arrives within another 20 seconds. Browsers send the
application ping after 15 seconds of silence with the same 20-second reply
budget; browser timers may be delayed in background tabs, and making the page
visible or coming online wakes reconnection.

Native foreground cancels pending backoff. After at least 10 s in the background
the app reconnects outright and replays subscriptions from applied cursors;
after a shorter absence it probes once with the application ping. Foreground
and Active from the same return do not issue duplicate probes. On Android, the
system's default-network callback (a change of transport or validation, not a
signal-strength update) and every resume, and on iOS every return to the
foreground, additionally tell the device endpoint to rebind its paths and probe
every live connection at once, so a stale link does not wait for the idle
timer. The desktop relies on the timers alone.

Outgoing commands and queries are bounded to 256 lines and 8 MiB per attachment,
including lines held for retry. A full queue rejects the new line with
`queue_full` (QueueFull) and logs the error; older lines are retained. Subscription
updates coalesce by topic outside those slots. The store exposes the pending
count through `queued_outgoing()`.

A disconnected device keeps cached thread content for reading; unvisited threads
may have only cached list information. Queries need a live connection, while
retained writes follow the [outbox policy](#weak-networks) and remain pending
until acknowledged. Subscriptions resume after reconnecting. Temporary network
failures keep retrying. A machine that refuses the device stops at **Offline**
with **Pair again**; protocol mismatches stop with **Update the app**.

## Limits

- Each device selects its own thread independently. Navigating on your phone
  does not change the thread selected on another desktop. Actions still operate
  on the same machine's projects and threads; selection independence is not
  access isolation.
- A device's terminal scrollback is the machine's retained ring, capped at
  **1000 rows**. Older output is gone from the machine too, so no device can
  scroll further back than that. During a large output burst the visible screen
  keeps updating, while scrollback catches up after scrolling has stopped for
  100 ms. This avoids repeatedly sending the whole retained ring between PTY reads.
- Android and iOS have an embedded preview with history, JavaScript automation,
  navigation errors and visible-page PNG capture.
- The browser client has no Preview tab. It already reaches the machine by
  address, so start the dev server listening on the LAN (for example
  `npm run dev -- --host`) and open `http://<machine-ip>:<port>/` in a new
  browser tab. The agent's preview tools are answered by a native client
  attached to the same machine.
- Phones, tablets and browsers do not run providers, terminals or project files
  locally; the machine does. Every product view is present on them — threads,
  chat, approvals, the terminal, diff, plan, preview (except in the browser),
  search, import and export — and what differs is which *operations* the device
  can perform. A native file
  dialog, an embedded preview browser, macOS permission grants, dictation and
  "open in editor" need a capability the device either has or does not; where it
  does not, the view says which machine can do it and offers what it can (open
  externally, copy, type the value). Prepare projects and provider installations
  on the machine.
- One machine holds at most 64 open streams per device connection, of which
  56 may be Preview tunnels; a device that opens more is refused with **too
  many tunnels** until one closes.
- The Android arm64 APK is signed with the project's release key, so each
  release installs over the previous one. Android rejects an update signed with
  a different key: uninstall first when switching between a release and a
  local build, or from a release older than the key. To sign a local release
  build with the same key, export `TCODE_ANDROID_KEYSTORE` (path to the
  keystore), `TCODE_ANDROID_KEYSTORE_PASSWORD`, `TCODE_ANDROID_KEY_ALIAS` and
  `TCODE_ANDROID_KEY_PASSWORD` before running `crates/android/host/build.sh
  --release`; with none of them set, Gradle's per-machine debug keystore signs
  the build. The iOS IPA is unsigned: re-sign it with your own Apple ID, or
  install it with a sideloading tool such as AltStore or Sideloadly. For
  Android, enable USB debugging and authorize your computer, then replace
  `VERSION` with the downloaded release version:

  ```sh
  adb install -r tcode-VERSION-android-arm64.apk
  ```

## History replay limits

Opening a conversation subscribes to its latest 200 event records. A session
snapshot includes the absolute `from` offset and `total` record count; the client
can request older records with `SessionHistoryPage { session_id, before, limit }`.
The exclusive `before` cursor pages backwards; replies are always chronological
and include their absolute `from`. Backwards pages contain at most 200 records.

Each snapshot or history page is capped at 8 MiB including its serialized wire
envelope. The host shrinks the contiguous range and sets `truncated` when the byte
limit reduces the requested count. A record that cannot fit alone returns
`history_record_too_large`; records are never split or silently skipped.

`Subscription.after` remains the absolute next event cursor, not a count of loaded
records. A reconnect replays from that cursor exactly. Byte-limited forward tails
start at `after`; advancing the applied cursor obtains the next contiguous tail.
A fresh subscription (no cursor) starts at the latest history window. Switching
conversations retires its subscription request IDs, and stale replies cannot
replace the currently selected conversation.

## Weak networks

Reads and writes have separate lifetimes. A transport-reported disconnect fails
in-flight queries immediately with `disconnected`; the workspace refreshes its
baseline during Syncing. A read issued without a connection fails the same way.
While connected, a query without a result for 15 seconds fails with `timeout`.

Retained commands use a client-generated UUID v4 key. They remain in an ordered
outbox until Ack, including across reconnects, attachment changes and client
relaunch. Native clients atomically replace an `outbox-<encoded machine id>.json`
file beside `hosts.json`, with private file permissions; browsers use the
origin's `tcode.outbox.<machine id>` localStorage entry. Clearing client data
clears these pending writes. An admission storage error is a real failure, not a
successful send.

After hello and subscription replay, every unacknowledged write is sent in queue
order without waiting for the previous Ack; the stream and the host preserve that
order, and a rejected write fails only itself. The same key is used on every
redelivery. The outbox is bounded at 256 entries and 8 MiB of serialized
entries. The oldest surplus entry fails
with `outbox_full`. A write already received by the host cannot be recalled by
client-side eviction.

The runtime retains the last 512 completed command keys and Ack results per
authenticated device, including rejected commands. Cache hits refresh their LRU
position and return the original Ack without executing again. This cache survives
a reconnect, but not a host-process restart or eviction from the 512-key
window. Exactly-once redelivery applies within that window. Keys are scoped by
the authenticated device id on the machine, never by a prefix the client
supplies, so one device's cache cannot be shared with or spoofed by another.

If a connected retained command has no Ack for 30 seconds, the client logs a
stalled acknowledgement and forces reconnect. The pending command remains in the
outbox. The request deadlines and liveness constants live together in
`crates/client/src/heartbeat.rs`: native idle probe at 10 seconds, browser idle
probe at 15 seconds, a 20-second liveness reply budget, a 15-second query
timeout and the 30-second command timeout.

A pending user message is muted with **Sending…**, or **Waiting for connection…**
when the workspace is reconnecting or syncing. It becomes normal only after Ack.
A rejected send or outbox overflow shows an inline error with **Retry** and
**Discard**. Retrying a terminal failure creates a new action and key. An approval
decision in the outbox disables the approval buttons and keeps a pending caption.
Transient reconnects belong in the connection banner, not failure toasts.

### Finding the machine again

A pairing identifies the machine by its key, independently of its address.
After every successful connection the device saves the relay and the direct
addresses that actually carried it — including a direct path found later by
hole punching — so the next attempt starts from what worked last. Between
attempts the lookup service (when the
machine publishes to one) supplies the machine's current relay and addresses;
a home relay lets the two ends learn each other's direct addresses and try
hole punching, and the connection can move from **Relay** to **Direct** — or
back — without reconnecting. The path shown in the banner and the machine's
device list follows that change.

On the same network the device also looks the machine up itself, whatever the
Traverse setting, so a paired machine is found again after a new DHCP lease,
after both moved to another network, or after a restart, on a LAN with no
internet and no Traverse. There is still no list of nearby machines: the
device only ever resolves the ids it is already paired with, and only the
machine advertises. Each attempt to connect uses exactly two sources, and
nothing else:

- The addresses saved for that machine, handed over at once and dialled
  together, in the order `hosts.json` keeps them: the address that carried
  the last successful connection first, older ones after it.
- A DNS-SD browse for `_tcode._udp` (multicast DNS, UDP 5353) that runs at
  the same time for about 2.5 seconds and adds every address it finds for
  the machine as it resolves. A machine advertises one instance of that
  type while it hosts — desktop and headless alike, whatever its Traverse sources
  — with its bound UDP port and a TXT record `v=1`, `id=<machine id>`,
  `name=<machine name>`; loopback is never advertised, and the record
  follows the machine's interfaces as they change. The device keeps only the
  instance whose `id` is the machine it wants. This is a standard
  advertisement, so `dns-sd -B _tcode._udp` on macOS or Android's
  `NsdManager` list it too; on iOS the app browses through the system's
  Bonjour daemon (the `NSBonjourServices` entry in `Info.plist`), and on
  Android it holds the Wi-Fi multicast lock for the browse.

A candidate address is never authorization: the QUIC handshake verifies the
machine's key, so nothing but the machine can answer. The lookup ends as soon
as the connection is up, and a change of the device's own interface addresses
starts it again at once. There is no subnet scan and no other fallback: where
the saved addresses are stale and multicast is blocked, the machine is not
found. Both the desktop app and `tcode-headless` bind UDP `47420` unless told
otherwise so that firewall rules and the invitation's port survive restarts.

With Traverse off there is no relay and no lookup service: the device has the
addresses it saved and the LAN lookup. A machine that moved to a network the
device is not on needs a new invitation.

### When nothing finds the machine

An invitation names no address ([The invitation link](pair-link.md)): a first
pairing reaches the machine through its relay, its Traverse lookup or the
DNS-SD browse above. Where none of them works — a LAN that blocks multicast
and has no internet, say — the first attempt opens no path within 20 seconds
and the pairing page asks for the machine's IP address instead of reporting
the transport's cause. The secret has not been sent, so the invitation is
still valid. Read the address off the machine (`Addresses:` in the
`tcode-headless serve` output, or the machine's network settings) and type
the address only, IPv4 or IPv6: the port comes from the invitation. **Try
again** (under the field on the Machines page) or **Connect** (on the paste
page) sends the same invitation again, dialling that address together with
everything the first attempt tried. If that address finds no path either,
the field says **Nothing answered at ‹address›**. The address that
carried the pairing is saved like any other, so the machine is reached there
again later. The machine's UDP port must accept
inbound traffic on the LAN for this to work.
