Changed

- **One helper where the same row, decode, or fixture was copied.** `testmempoolaccept` rows, Electrum broadcast hex checks, and boolean conf errors each go through one function. Header-peer order uses one sort key. Peer, RPC, and SV2 tests build their default fixture in one place. Results and error strings are unchanged.
