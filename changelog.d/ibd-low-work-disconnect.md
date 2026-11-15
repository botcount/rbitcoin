Fixed

- **Outbound peers on a low-work chain drop during IBD.** A headers
  message that ends the peer's chain and sits under `-minimumchainwork`
  disconnects outbound full-relay and block-relay peers. A later short
  fork does not drop a peer whose best-known header already meets the
  floor. An inv, block, or compact block does not replace that header
  with less or unknown work. The work measured for that header is kept
  after the header leaves the pending map. Inbound and manual peers stay.
  `feature_minchainwork.py` is run
  again. Port of
  bitcoin/bitcoin#36426 (#1484).
