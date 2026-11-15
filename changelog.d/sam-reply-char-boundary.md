Fixed

- **An I2P SAM reply with a non-ASCII character no longer panics the node.**
  The SAM key-value parser sliced a reply token at the key's byte length, so a
  multi-byte UTF-8 character across that index panicked at startup. It now
  skips that token (#1361).
