Added

- **Nightly container image.** Every night the master tip is published as
  `nightly` and `nightly-YYYYMMDD` (UTC date) for `linux/amd64` and
  `linux/arm64`, to the same registries as the release image. Each arch is
  booted on regtest first. Maintainers can publish one on demand from
  Actions → release → Run workflow with **nightly** ticked.
