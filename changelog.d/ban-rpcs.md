Added
- **`setban` / `listbanned` / `clearbanned` RPCs.** An operator can ban an
  IPv4/IPv6 address or subnet, or an onion/I2P identity, by hand: adding a
  ban disconnects matching sessions and refuses new ones until the ban
  expires or is removed. Manual bans persist across restarts in
  `{datadir}/banlist.json` (Core's shape). Automatic discouragement for
  misbehavior stays a separate in-memory set and is not listed or cleared
  by these methods. A manual ban can name loopback (`127.0.0.1`, `::1`),
  as in Core; discouragement still never records a loopback peer.
