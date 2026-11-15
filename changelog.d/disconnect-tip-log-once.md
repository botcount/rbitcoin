Fixed

- **A disconnected block logs one `DisconnectTip` line.** Each block that a
  reorg or `invalidateblock` disconnects wrote the same warning twice. It
  now writes it once, after the disconnect is durable.
