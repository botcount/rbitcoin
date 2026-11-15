Changed

- **Fixed-width reads no longer have a second failure arm.** Table headers, array slots, and locator checkpoints are a typed prefix. A short file or a ragged body still returns the same error. The decoded values are unchanged.
