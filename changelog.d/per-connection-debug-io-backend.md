Changed

- **Electrum and Stratum v2 connect and disconnect lines log at DEBUG.**
  Each TCP connection wrote two INFO lines, so any client could fill the log
  by reconnecting. Run with `--log-level=debug` to see them. A disconnect
  caused by a request handler that panicked still logs at WARN.

Fixed

- **The startup line's `io=` names the backend in use.** It printed the raw
  `RBITCOIN_IO` text, or `default` when unset. It now prints `io_uring`,
  `pool`, `iocp`, or `pread`: the engine the store built, including the
  `pread` fallback when io_uring cannot open and the pool when Windows IOCP
  cannot open.
