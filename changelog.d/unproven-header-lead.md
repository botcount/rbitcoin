Changed

- **Confirm during an unproved header walk stays inside a matched lead.**
  Below the work floor the same confirm pipeline still checks blocks while
  headers are unproved. It may follow a second peer's matched headers by
  65,536 blocks or 1 GiB of Class A above that match, whichever comes first.
  A later match moves that line, so blocks already matched stay available to
  confirm. The match is the peer's address, and neither the match nor the
  byte count is saved. A restart measures a new lead from the confirmed tip,
  so two restarts can leave more than one lead on disk before a rewind.
  While the confirmed tip is under the floor, a rewind may drop 65,536
  blocks. Once that tip has proved the work, a rewind stops at 1,024. The
  confirm caps lift when the header walk itself proves the work. Abandoned
  blocks stay on disk.
- **A short header batch below the floor does not extend the walk.** A full
  2,000-header batch that passes context does, including a lighter
  challenger. A shorter batch extends only when the work after that batch
  meets the floor. Headers past the ordered-header soft cap stay in memory.
  A walk longer than six headers per second since genesis does not extend,
  and that peer leaves. That cap is fixed when the walk starts, from the age
  of the chain since genesis. A clock more than two hours behind the starting
  median does not start the walk and does not store headers. The same check
  runs again, and headers start once the clock is close enough. The walk uses
  the node clock. A second witness is another address, not another connection.
  `header.adopt` is rewritten only after the walk is proved.
