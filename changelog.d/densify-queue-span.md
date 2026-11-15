Changed

- **A densify walk reads the body queue once for the heights it scans.**
  Each height used to take the queue lock to see if a body was there.
  The walk now records which heights in that span already have a row.
  A height that was empty stays empty for the walk. A height that had a
  row is read again, so a body confirm removed is not treated as in hand.
