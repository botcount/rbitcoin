Changed

- **Fixed-width reads no longer have a second failure arm.** The undo record, the alloc counts, the tweak offset run, the txstat cell, and the txstat blob slot are a typed prefix. A short header, a non-page undo, a run past the segment, or a fk past the count still returns the same error. The decoded values are unchanged.
