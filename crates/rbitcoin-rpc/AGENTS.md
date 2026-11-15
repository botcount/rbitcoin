# rbitcoin-rpc

Core-class JSON-RPC subset. Not full Core parity. Depends on query, store,
net, and consensus. The HTTP server is `src/server.rs`; methods are
`src/methods/`.

## Read first

Open the row that matches the change. Leave the other owners closed.

| Change | Read |
|--------|------|
| Method list, auth, permanent gaps | [`docs/rpc.md`](../../docs/rpc.md) |
| Intentional differences | [`COMPAT.md`](../../COMPAT.md) |
| What operators can pass | [`docs/operator/interfaces.md`](../../docs/operator/interfaces.md) |
| 0.8 mempool CORE_RPC over TCP + Core cookie (`--rpc-cookie-file`); unix `rpc.sock` optional | [`docs/operator/interfaces.md`](../../docs/operator/interfaces.md#mempoolspace-core_rpc), [`docs/rpc.md`](../../docs/rpc.md) |

## Verify

`cargo test -p rbitcoin-rpc --test suite`

Core functional (labeled job only, not the default pin):
[`.agents/skills/core-functional/SKILL.md`](../../.agents/skills/core-functional/SKILL.md).
