Fixed

- **A lost outbound onion or I2P peer is replaced.** The refill after a drop
  and the stale-tip rotation each dial one peer, and that one dial always went
  to clearnet. It now goes to an onion or I2P address when no outbound onion
  or I2P peer is live. While one is live, it stays on clearnet as before.
- **Stale-tip rotation no longer drops onion and I2P peers first.** It counted
  every onion and I2P peer as one network group, so with two of them it chose
  one of those before any clearnet peer. Each network now has its own groups,
  split by the first four address bits as in Bitcoin Core.
