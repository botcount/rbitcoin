Fixed

- **A new block no longer stalls Electrum and Esplora while peers are told
  about it.** Deciding how to announce a tip to each peer read every parent
  by rebuilding that whole block from the archive, hundreds of rebuilds per
  peer, on the same workers that answer Electrum and Esplora. It now reads
  each parent from the stored header. Electrum tip notifications and block
  requests right after a new tip no longer wait hundreds of milliseconds.
