Fixed

- **`submitblock` no longer marks a valid block invalid when a competing
  block connects during its input check.** The check read spentness at the
  live tip on every input. A sibling from a peer that connected mid-check
  and spent one of the same coins made the submitted block
  `bad-txns-inputs-missingorspent`, cached invalid until restart or
  `reconsiderblock`, and its children were `bad-prevblk`. The check now
  reads the tip once and reads every input at that height. Its reject
  stands only if the tip is unchanged and no block was disconnected while
  it ran (a reorg undone before it finished counts). Otherwise the block
  takes the serialized accept path, and an equal-work sibling is
  `inconclusive`, as in Core.
