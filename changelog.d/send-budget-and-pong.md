Fixed

- **Compact-filter replies and served compact blocks count their real
  payload on the per-peer send budget.** A `cfilter`, `cfheaders`,
  `cfcheckpt`, or `cmpctblock` is charged the consensus encoding of that
  message, counted from the fields it already holds, not a flat 64 or
  1,024 bytes and not a throwaway encoding. One reply may still cross the
  4 MiB cap. The next request from that peer waits until the writer drains.
  One `getcfilters` stops after the filter that crosses the cap.
  `getcfcheckpt` still returns a filter header every 1,000 heights. Each
  header is one idx slot, not every height from 1,000 through the stop.
- **A peer gets one pong, and that pong counts on the send budget.**
  Tip-follow and IBD each keep the latest ping nonce, including nonce 0,
  on a single queued pong. Writing it subtracts that one charge. IBD pings
  past the per-peer message window take the existing misbehavior disconnect.
  A block this node requested still skips that window.
- **IBD stall counts solicited block bytes.** A peer with getdata on the
  wire and a stale block clock is withholding when only pings or headers
  have arrived. A finished block, or a block frame still on the socket
  that is longer than a headers message, keeps that peer once 64 KiB has
  arrived. Relative-slow zero-throughput sampling uses the same delta. A
  peer that sends no socket bytes stays the quiet case.
- **A proved header walk does not rewrite `header.adopt` on every short
  batch.** The in-memory tip still advances, and each reply stays a
  checkpoint so a lighter fork can be seen. The file is written when the
  walk first proves the floor and when the tip is 2,000 headers past the
  last saved checkpoint. Promoting a competing chain writes that chain's
  tip. An unproved walk still does not rewrite the file. A save that does
  not stick is written again on the next proved batch.
- **A pong that arrives while a pong is being written is still answered.**
  Tip-follow and IBD each emit that nonce and clear the pending slot, so
  the next ping queues one new pong.
