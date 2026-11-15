Changed

- **A stuck download gate reads the height occupant before the hash map.** When that occupant still needs a body, the gate opens without scanning every header height.
