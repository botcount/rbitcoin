Changed

- **Start no longer reads the whole fixed-seed table when it doesn't need
  to.** When the saved peer book already has a row for every net the node
  can dial, the table is not read at all. Otherwise seeds for nets the node
  won't use are skipped before they are parsed, so an onion-only node parses
  only the onion lines. The seeds added are the same as before.
