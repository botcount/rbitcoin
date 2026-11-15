Fixed

- **A small bulk outspends request can no longer exhaust memory.** `GET /txs/outspends` and `POST /internal/txs/outspends/by-txid` refused nothing but a 51st public txid, so 50 copies of one 80,000-output txid in a 3 KB URL grew past a gigabyte and could get the node OOM-killed. Both routes now answer 400 `Too many outputs requested` when the listed txids' output counts sum past 250,000, the internal route takes at most 1,000 txids, and every outspends array (including `/tx/:txid/outspends`) is encoded slot by slot instead of building a JSON tree first.
