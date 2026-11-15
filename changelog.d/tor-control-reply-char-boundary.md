Fixed

- **A Tor control reply with a multi-byte character at byte 3 or 4 no
  longer panics.** The reader sliced the line at those byte indexes, so
  the start, or the onion keeper after a reconnect, panicked. A status
  code that is not three digits, such as `25é` or `2x0`, is now an
  `unexpected reply` error; `2x0 OK` used to pass as a success. A 5xx
  code is still a refusal when a wide character follows it, as in
  `550é x`.
