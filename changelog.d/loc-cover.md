Changed

- **Write loc lookup uses the create range.** A read takes the lowest pack whose range covers that id. Overlapping packs keep the lower height.
