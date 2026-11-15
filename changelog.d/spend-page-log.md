Fixed

- **A torn historical spend page below `spend_durable` stays spent while `spend.pages.commit` exists.**
  Confirm records every published slot, including the short tail, in a directory
  and does not write that `spent.body` slot. Schema is 28. A schema-27
  binary refuses the datadir, so it cannot write a spend under a live page
  log. Open refuses a commit record whose offset is not a page boundary,
  whose image is longer than one page, or that names a page twice. The spend
  checkpoint writes a new generation file of every page still named and
  renames it over `spend.pages` before it advances the marker, and the next
  checkpoint folds a generation the previous one left
  quiet. A retry of a failed compose writes the frozen pages and every note
  taken after that freeze into the replacement commit, including a newer slot
  on a page the previous commit sealed and a page a disconnect put back in
  the open directory. A page whose new notes are no longer best-chain still
  names that sealed image. Folding the first page does not rewrite the file header, and it does
  not publish a shorter `spent.body` end while confirm is still appending. The
  body copy holds the `--datadir-hdd` spindle until it returns, and does not hold the utxo view. A checkpoint below a loaded commit height
  filters that image before fold, even when the held bit was never written.
  Unlinking a finished generation takes the same lock as a fold hold. A spend
  noted while that filter walks the image stays out of the checkpoint height
  already sampled, so a later fold does not seal it at the lower tip. A
  rejected spend that `spent.body` already holds is zeroed in the next image
  instead of being copied back. The old commit stays on disk until that
  replacement is renamed. It is removed when the body does not hold the
  rejected spend, unless a zero already stored on that page is still hiding
  a body spend.
  A page that still has a best-chain slot keeps that image across a failed
  body read, and a zero image already stored is folded after a later
  disconnect instead of being deleted. A zero already stored on that page
  stays in the next image when a new note lands on it, when the page
  still has a best-chain slot, and when this checkpoint rejects a
  different slot on that page. A different page whose slots are all
  still best-chain stays named when this checkpoint rejects one page,
  including beside a zero page that is kept.
  A missing or short image file beside a
  commit that names bytes refuses to open, including a record past the
  committed length. A committed image shorter than the page does not answer
  for a later slot, and the next compose copies that body tail instead of
  padding zeros. Startup replay does not wait on the edge cap when no sync
  thread is bound, and a confirm batch does not wait while it holds the utxo
  view. The next confirm insert waits at 2,000,000 open edges. A 1 GiB log
  asks for one extra checkpoint, not another on every later batch. Neither is a
  knob. Once the commit names a generation, a failed rename of that file still
  keeps the publish, and the next checkpoint writes the next generation
  instead of replacing the file the commit already names. A keep error
  while filtering notes taken during compose leaves those notes in the
  open directory, and the next checkpoint still publishes them. A spend
  the directory still serves, or whose body bytes do not match, is written
  again when annotate would otherwise skip it, so a checkpoint that drops
  the image cannot leave that best-chain spend unspent. Open drops a commit
  record at or past the repaired `spent.body` end instead of refusing to
  start, and cuts a page that straddles that end. Spender overflow is
  synced before the page commit is renamed. A disconnect holds the fold
  through the confirmed tip frozen at the checkpoint, not only the snapshot
  height, and a second disconnect during a deferred publish keeps that hold.
  That tip is written on the commit. A checkpoint below a loaded commit holds
  the fold before it widens the cool range. Open holds when the commit height
  or that tip is above the chain, not only when the marker is. A directory
  sync that fails after the commit rename still keeps that generation, and
  the generation file stays until that sync succeeds. A disconnect during
  the held keep does not clear that hold, so the next checkpoint does not
  fold the old image. The fold deletes the commit, syncs the directory,
  and only then deletes the page files.
  The previous image stays readable until the directory swap. A quiet page
  is copied onto `spent.body` before the next checkpoint carries it, and
  the page file is streamed. A failed checkpoint waits one second while the
  edge cap is still armed, and the full ten minutes otherwise. Startup
  replay checkpoints a chunk once the open directory reaches 2,000,000
  edges, and the next checkpoint folds that generation. It does not wait
  for a sync thread. Spender overflow is synced again after the freeze and
  before the commit rename, so a multi-list node noted after the first sync
  is durable before that commit names it. A note that lands after the body
  length is sampled is composed at the length taken with the freeze. A reader
  of many outputs checks the page log before `spent.body`. A spend accepted
  after the freeze raises the sealed tip, so a later disconnect below that
  tip holds and does not fold it. A same-spender annotate whose log already
  serves the slot does not read `spent.body`, so a torn body page does not
  fail that rewrite. A miss still refuses a reserved flag.
