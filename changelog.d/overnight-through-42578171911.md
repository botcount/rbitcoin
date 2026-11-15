Fixed

- **Missed mutants in rbitcoin-net through run 42578171911 are caught or removed.** Journeys now fail when those operators change a peer-visible result. Compares no caller can observe are gone. A restored leaf under an existing fork tip stays off the tip list. An equal-height block parked by this accept is dropped again when the attempt fails, except an equal-work loser and a hold that was already there.
