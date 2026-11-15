Fixed

- **A datadir created for one signet no longer opens under another.** Every
  signet shares one genesis block, so the stored-genesis check let a datadir
  open under a different `--signet-challenge`, or under the default one.
  `{datadir}/network` now records the network and its P2P message start on
  the first start, and logs `datadir: no network marker in …; adopting …`.
  Once it exists, a start on another network or challenge refuses with
  `Wrong datadir for network?` before the store opens, before any peer, and
  before RPC serves.
- **Upgrade note for signet:** a datadir from before this release has no
  `network` file. Its first start after the upgrade records whatever
  network that start uses. A signet datadir can't be told apart by its
  genesis, so start it once with its original `--signet-challenge`.
  Started with another challenge first, it keeps that one and refuses the
  original from then on.
