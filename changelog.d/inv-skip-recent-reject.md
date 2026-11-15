Fixed

- **A rejected transaction is not requested again when another peer announces it.** The node asked every new announcer for a tx it had just rejected, by txid or by wtxid. That includes the child of a rejected parent, which Core never requests again.
