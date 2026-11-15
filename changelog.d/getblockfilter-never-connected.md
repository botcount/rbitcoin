Fixed

- **`getblockfilter` on a block that never connected.** A block the node
  knows only by its header (`submitheader`, or headers ahead of the tip) and
  a held side-branch block now get Core's `-5` `Filter not found. Block was
  not connected to active chain.` Before, the answer was `Block not found`,
  or the indexing error of the parent block. An unknown hash is still
  `Block not found`. Ports bitcoin/bitcoin#36613.
