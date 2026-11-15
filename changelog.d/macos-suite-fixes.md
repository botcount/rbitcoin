Fixed

- **Five default-suite tests no longer fail on every macOS run.**
  Two `rbitcoin-rpc` server tests bound their Unix socket under a labeled
  temp directory, past macOS's 104-byte `sun_path`. They now use
  `TempDir::for_socket`. `asmap_loopback_peers_report_their_asn` (binds
  `127.0.0.2`) and `outbound_full_relay_refills_to_max_outbound` (`ip` /
  `unshare`) are ignored off Linux with a reason. `sv2_tp_bootstrap` skips
  the documented tip-event rebuild that arrives when the hold check builds
  first, instead of reading it as the constraints rebuild.
