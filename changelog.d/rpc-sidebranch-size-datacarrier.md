Fixed

- **`getblock` on a side-branch block now reports `size`, `strippedsize` and
  `weight` at verbosity 1, and `getmempoolinfo.maxdatacarriersize` is a number.**
  Verbosity 1 of a block off the active chain left out the three size fields
  that Core prints (verbosity 2 and 3 already had them). `maxdatacarriersize`
  was `null`, which typed clients that model it as an integer reject; it now
  prints 100000, Core 32.1's default cap. Libre does not enforce a datacarrier
  cap, and that default does not bind a standard-weight transaction either.
