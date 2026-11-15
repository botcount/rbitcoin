Fixed

- **The inbound eviction test now fails when any one protect is removed.**
  Its block senders were also the oldest peers, so longest-connected
  protection kept them with block-time protection gone. Each protected set
  now sits apart from the others, and the test asserts the one peer evicted
  (port of bitcoin/bitcoin#36470, #1479).
