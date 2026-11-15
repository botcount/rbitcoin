Fixed

- **A compact block is relayed only after its body matches the header's
  witness commitment.** A peer that delivered a new tip block with a
  changed witness or coinbase nonce, as a full `block`, a `cmpctblock`, or
  a `blocktxn` reply, made the node announce that body to its
  high-bandwidth compact peers before connect rejected it. The node then
  could not serve the block, so those peers waited on it, and a Bitcoin
  Core peer disconnected the node at its block-download timeout. That body
  is no longer announced. The real block is still announced before
  connect, and the bad body is handled as before: the hash is not marked
  invalid. As in Core, no compact block is announced before connect at a
  height where segwit is not active.
