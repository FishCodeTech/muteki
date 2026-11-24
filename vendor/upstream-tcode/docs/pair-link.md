# The invitation link

An invitation is a `tcode://pair?…` URL. It is the only way a device first
meets a machine: shown as a QR code on the machine, copied as text, scanned
or pasted on the device. This document is the format's contract. The encoder
and parser live in `crates/client/src/pairing.rs`; the machine fills the
routing hints in `crates/traverse/src/host.rs`.

## Design rules

- **The link is for first contact, not for routing.** It carries what the
  device cannot learn anywhere else: which machine, which secret, which
  Traverse instances the machine publishes to. Everything else is a hint
  that shortens the first connection and is dropped when the device can get
  it elsewhere (pkarr lookup, DNS-SD on the LAN, hole punching).
- **No addresses and no names.** A link is pasted into chats and
  screenshots, often across the internet. It never carries an IP address of
  any kind — public, private, link-local or loopback — nor the machine
  name. A private address is still a fact about the machine's network. The
  relay URL is the only location it names, and the relay sees only
  ciphertext. The UDP port alone is carried, so a user can type an address
  by hand when nothing else finds the machine.
- **Short enough for a comfortable QR.** A typical link is under 200
  characters, QR version 9 or lower at error-correction level M.
- **Parsing is strict.** Every field is validated; an unknown version, a
  missing required field, a malformed value or a link over 1024 bytes
  (surrounding whitespace aside) rejects the whole link.

## Format

```
tcode://pair?v=3&id=<endpoint id>&secret=<secret>[&space=<id>|&group=<id>][&traverse=<src>]*[&relay=<url>]&port=<udp port>
```

Parameters are form-urlencoded in this order. The parser accepts any order,
rejects duplicates of single-valued parameters, keeps a repeated `traverse`
value once and ignores parameters it does not know.

| Parameter | Required | Value |
|---|---|---|
| `v` | yes | The literal `3`. Older versions are rejected. |
| `id` | yes | The machine's iroh `EndpointId`, 64 lowercase hex characters. The dial target and the identity the QUIC handshake authenticates. |
| `secret` | yes | 16 random bytes as unpadded base64url, 22 characters. Sent to the machine over the pairing ALPN; only the machine ever checks it. |
| `space` | no | A space id (1–64 characters, no control characters). Present only on space links, which are reusable; machine invitations without it are single-use. |
| `group` | no | The machine's group id, 16 random bytes as unpadded base64url, on the invitation of a machine in a group; see [Machine groups](remote.md#machine-groups). Never with `space`. The invitation stays single-use: a device pairs with it, or another machine pastes it into **Join group** to become a member. |
| `traverse` | repeated, 0..8 | Every Traverse instance the machine currently publishes to, so the device can resolve the machine later. `official` names the official instance; any other value is a self-hosted instance's base URL (`http` or `https` with a host). No `traverse` parameter means the official instance only. The single value `off` means the machine publishes nowhere and is reachable on the LAN only. `off` cannot be combined with other values. |
| `relay` | no | The machine's current home relay URL (`http` or `https`). A dial hint: the device connects through it immediately instead of waiting for a pkarr lookup. |
| `port` | yes | The machine's bound UDP port as a canonical decimal (no sign, no leading zero), 1–65535 (47420 by default). Used only for a manually entered address. |

Everything the link does not carry is learned elsewhere:

- The machine name comes from the machine's `Paired` reply. While pairing is
  in flight the device shows only a connecting state.
- The machine's addresses: on the LAN, the DNS-SD browse (`_tcode._udp`)
  that runs for every first contact; elsewhere, the relay and hole
  punching through it. After pairing, the device remembers the addresses a
  connection actually used, with any typed address kept first in the saved
  list. Pkarr keeps the relay current.

## How the device uses the link

1. Parse. A link that does not parse is reported as not an invitation.
2. Install lookups for every `traverse` source, concurrently (official has a
   bundled manifest; a self-hosted one is fetched once, 10 s timeout).
3. Dial the `id` with `relay` as the only transport address; iroh adds
   whatever pkarr and DNS-SD resolve.
4. Present the `secret` (and `space`) over the pairing ALPN. The
   machine replies `Paired { host_name }`; the device saves the machine with
   that name, the `traverse` list, the relay and the addresses it learned.
   A machine in a group also lists the group's machines, which the device
   saves beside it.

### When the first connection does not establish

If no path opens within the connect budget (20 s; iroh gives up sooner when
it has no address to try at all), the device has not yet sent the secret, so
the invitation is still valid. The device then asks for the
machine's IP address, dials `<address>:<port>` together with the original
hints, and retries the same invitation. This covers a LAN where
multicast is blocked and there is no internet: the user reads the address
off the machine (`Addresses:` in the headless output, or the machine's
network settings) and types it. Only the address is typed; the port is never
asked for.

## Example

```
tcode://pair?v=3&id=a5f3c6ea…c5c5&secret=AAECAwQFBgcICQoLDA0ODw&relay=https%3A%2F%2Fuse1-1.relay.n0.iroh.link.%2F&port=47420
```

That machine publishes to the official instance only and listens on UDP
47420. With the full 64-character id the link is about 180 characters.
