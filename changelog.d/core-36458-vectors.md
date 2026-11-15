Changed

- **Core's tests run from Bitcoin Core master `d206226b`, after v32.1.**
  The consensus suites gain the vectors Core added since v32.1, including
  bitcoin/bitcoin#36458's interpreter edges (CSV with -2, IFDUP on negative
  zero, ROLL with repeated values, a taproot key that is not on the curve,
  and LOW_S, STRICTENC and NULLFAIL alone): `script_tests` 1280/1280,
  `tx_valid` 122/122, `tx_invalid` 97/97. The harness now passes each flag
  as written, so the LOW_S rows check that the interpreter itself requires
  strict DER, and a CONST_SCRIPTCODE row without WITNESS reaches the
  interpreter. A target dir staged at the old pin picks up the new JSON.
  Official `bitcoind` binaries and the release check stay on v32.1. The
  5 new functional scripts are inventory skips. Run-set scripts stay
  skips until master's new checks pass: reply order before a pong
  (#1483), exclusive P2P bind (#1485), `submitpackage` `other-wtxid`
  (#1486), orphan requests after a rejected parent (#1487), the
  custom-signet datadir name (#1488), and an unconnecting `headers`
  answering `getheaders` (#1497). The functional shim now reads
  `whitelist=` from `bitcoin.conf` (#1476).
