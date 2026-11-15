Fixed

- **An unconnecting `headers` message no longer answers our `getheaders`.**
  As in bitcoin/bitcoin#36422, only an empty `headers` message, headers
  that connect, or a low-work continuation clear the outstanding request.
  A header that does not connect sends a new `getheaders` only after that
  answer or after 2 minutes (Core `HEADERS_RESPONSE_TIME`). Before, the
  in-flight mark never expired, and any `headers` message cleared it.
  `p2p_sendheaders.py` runs again (#1497).
