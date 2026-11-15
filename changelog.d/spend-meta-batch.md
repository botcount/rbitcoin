Changed

- **One confirm write reads spender meta once.**
  The spend directory answers every post-overlay offset in that write under
  one lock. Misses go through one peek plan, so two slots the grouper
  would merge share a peek across blocks. An open page keeps a 512-bit
  presence index beside its slot list. A slot the directory does not serve
  is still read from `spent.body`.
