Fixed

- **The RPC "at max connections" reject line is logged at most once a second
  on a thread.** A burst of connections past the 256-connection cap does not
  append one line per connection. This is the same limit the self-connection
  and header-reject lines already have.
