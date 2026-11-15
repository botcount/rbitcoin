Added

- **`--sp-tweaks` can run beside `--prune-seqsigwit`.** Turn it on before
  taproot, or later once every height from that origin through the dropped
  range is sealed or still in the last 288 blocks. Prune can be turned on
  after the index is built when the index covers every height the cut will
  drop. Sealed rows keep being served from the index. A pruned
  `blockchain.tweaks.subscribe` reads an unsealed tip from the keep window.
  A confirm batch does not delete a witness until that height's tweak row
  is sealed, and only through the tip whose flush has landed. A crash that
  cut the witness stem before saving the prune height is treated as those
  heights already dropped. A silent-payment scan that cannot read a
  height returns that error and does not report the scan finished.
