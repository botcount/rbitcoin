Fixed

- **The script pool tests no longer flake on another test's wave (#1547).** Two tests ran without the lock the other pool tests hold. One published a wave that a steal test or a free worker could claim. The other armed the park-delay hooks that a locked test also arms.
