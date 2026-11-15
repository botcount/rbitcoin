Fixed

- **A block that connects again keeps the outputs it spent inside itself
  spent.** When a block was disconnected (reorg or `invalidateblock`) and
  the same block connected again, with a spend checkpoint (clean shutdown,
  open replay, or the 10-minute sync) while it was active and another
  before it came back, every output that block both created and spent read
  unspent. A later block or transaction spending one again was accepted,
  where Core rejects it with `bad-txns-inputs-missingorspent`. The
  checkpoint after the disconnect zeroes that block's spend slots, and only
  the first connect wrote its same-block spends. A connect of a block whose
  body is already archived now writes them, as open replay does. A node
  that already connected such a block again keeps those wrong spends after
  the upgrade, until `store/spend_durable` is deleted and the node
  restarted. Open then revalidates the whole chain and replays every
  height's spends
  ([116](docs/external_findings/116-reconnected-in-block-spend.md)).
