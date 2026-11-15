Changed

- **The test guide names four layers.** A permutation unit is a valid mutant
  kill for one cell once a journey already reaches that function. Pins that
  only reread the test source or a log sentence are gone.
- **Peer and RPC tests wait on a condition.** A getdata stall backdates its
  clock. A reorg header batch waits out its announce interval. An RPC call
  waits until accept, `getrpcinfo`, or a store-budget waiter, instead of a
  fixed sleep.
