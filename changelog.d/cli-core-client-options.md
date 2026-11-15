Added

- **rbitcoin-cli gains four Core client options: `--named`, `--stdin`, `--rpcwait`, and `--getinfo`.**
  `--named` sends `name=value` pairs as a JSON object, `--stdin` appends extra arguments
  read one per line from standard input, `--rpcwait` retries a refused connection for
  up to ~30 seconds, and `--getinfo` prints a Core-style chain and network summary (no
  wallet section, since rbitcoin has none).
