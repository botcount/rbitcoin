Fixed

- **RPC auth schemes match in any case.** `Authorization: BEARER …` and
  `basic …` are accepted, since RFC 9110 makes auth schemes
  case-insensitive. Before, only `Bearer`/`bearer` and `Basic`/`basic`
  passed.
- **An empty RPC token file no longer blocks start.** A crash between
  creating and writing `rpc.token` left a zero-length file, and every later
  start failed with "empty". The node now replaces a zero-length
  `rpc.token` with a new token. A new token is written whole to a
  sibling file, synced, and renamed into place, so a crash can no longer
  leave a partial token that later starts accept.
