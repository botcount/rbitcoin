Fixed

- **`--proxy` now resolves `--connect` and `addnode` names.** The node
  hands the name to the SOCKS proxy (address type 3). Before, the OS resolver
  looked it up first, so clear-text DNS queries for the peer left the host
  outside the proxy, again every two seconds while the dial failed. The
  name is also the key of its session now: a redial pass no longer opens a
  second session to a name that is up, two names on one port stay apart, a
  `--trust-peer` hostname's session is the trusted one, a `--connect`
  name is dialed by catch-up and tip-follow, `disconnectnode` takes the
  name `getpeerinfo` lists as `addr`, and a peer reached by name is told the
  onion address we listen on, as a proxied IP peer is. Past 1024 distinct
  proxied names a new one is refused instead of sharing one key (#1348).
- **`--only-net onion` no longer asks clearnet seeds.** With no IPv4 or
  IPv6 reachable, the node sends no DNS seed query, local or through
  `--proxy`, and adds only the fixed onion seeds. Before, it looked up the
  seed hostnames anyway and dropped the answers. With `--proxy` it added no
  fixed seeds at all, so it had no first peer (#1348).
- **The I2P session uses the router's tunnel length.** The node no longer
  sets `inbound.length=1 outbound.length=1` in `SESSION CREATE`, which made
  one-hop tunnels. The router default (three hops) applies, as in Core
  (#1348).
