Fixed

- **Missed mutants through run 43177771634 are caught or removed.** Journeys fail when those operators change a session result. Compares and log counters no input can observe are gone.
- **`ibd: sizes` no longer prints prep-ahead pin counters.** The load thread no longer copies CreatePin occupancy into that line. The retain window still uses the live pin snapshot.
- **`ConfirmFeed::clear` keeps a clamped lead inside the installed allowance.** A refund cannot raise the remainder above that allowance minus the reservations still held. A charge whose reservation was already released stays spent.
