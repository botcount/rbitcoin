Fixed

- **Consensus: IBD no longer halts on a spend of a later block's
  transaction behind a valid block.** When one write held a valid block
  and then a block spending a transaction that only a later block
  creates, the write was rejected, the valid block connected on its own
  retry, and the invalid block then halted IBD with "engine fault
  repeated" until a restart. Bitcoin Core rejects that block with
  `bad-txns-inputs-missingorspent` and carries on. The rejected write had
  already inserted its transactions into `tx.head` but did not record
  that, so the missing-parent check could not tell the index had caught
  up. A write now records its `tx.head` insert whether or not it is
  rejected, and the block is marked invalid
  ([089](docs/external_findings/089-missing-parent-verdict.md)).
