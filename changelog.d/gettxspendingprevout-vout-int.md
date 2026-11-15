Fixed

- **`gettxspendingprevout` and `getblockhash` read their index as Core's
  `int`.** A `vout` or `height` was cast to 32 bits, so `4294967296` looked
  up output 0 or the genesis hash. A value outside `int` is now `-1` `JSON
  integer out of range`. A negative `vout` is `-8` `Invalid parameter, vout
  cannot be negative`, a missing or null `vout` is `-3` `Missing vout`, and
  a negative height is `-8` `Block height out of range`, as in Core.
