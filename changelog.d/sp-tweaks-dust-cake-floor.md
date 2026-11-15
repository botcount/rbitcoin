Fixed

- **`--sp-tweaks-dust 545`, not 546, matches Cake electrs.** The node omits a
  P2TR output with `value <= SATS`, and Cake's electrs keeps
  `value >= sp_min_dust` (546). Help and docs named 546 as the matching value,
  which omits a 546-sat output Cake's server serves. Behavior is unchanged;
  a Cake Wallet backend should set `--sp-tweaks-dust 545`.
