Changed

- **A restart with a known peers book no longer asks the DNS seeds at once.**
  As in Core, the node dials the addresses it already knows and waits 11 s
  (5 min when the book has 1,000 or more rows). It asks the DNS seeds only
  if fewer than two outbound full-relay peers are connected by then. An
  empty book still asks them at start. IBD now also dials addresses that
  join the book while it runs.
