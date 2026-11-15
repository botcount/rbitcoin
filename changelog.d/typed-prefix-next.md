Changed

- **Fixed-width reads no longer have a second failure arm.** Overflow rows, the tweak prefix, and the sorted-head header are a typed prefix. A ragged file or a foreign version still returns the same error. The decoded values are unchanged.
