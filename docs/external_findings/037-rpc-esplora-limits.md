# 037 — RPC body, work queue, waits, and Esplora client id

**Severity:** high
**Status:** fixed
**Found by:** coordinated review, 2026-09-21 and 2026-09-22 (C12, C13, and the authenticated-RPC and loopback-client notes)

HTTP JSON-RPC checks the bearer on the request parts. A missing or wrong
token never buffers the body, and the node reads at most
`UNREAD_BODY_MAX` (64 KiB) of it. Before any answer goes out (401, 404,
405), the node reads a body no handler read frame by frame and drops each
frame, up to that cap and the read timeout, so a small refused request
gets its answer and not a reset. A broken body is then 400 and a stalled
one 408, the status an authorized request gets. A larger body, declared or
streamed, is answered at once with `Connection: close` and not read
further. A client that sends it with its headers can still read a reset
instead of the answer, as with an authorized 413. A body a client holds
for `Expect: 100-continue` is answered at once with `Connection: close`
and not read. The
authenticated body cap is the named `RPC_MAX_HTTP_BODY` (32 MiB, Bitcoin
Core `MAX_SIZE`): a larger body with a valid bearer is 413 and the method
does not run. REST stays at `REST_MAX_HTTP_BODY` (2 MiB) and does not use
that ceiling. A body under the cap that applies still dispatches.

`--rpc-work-queue` defaults to 16, matching Core `-rpcworkqueue`. A full
queue is HTTP 503. **0** is that same default queue.

`waitforblock`, `waitforblockheight`, `waitfornewblock`, and
`getblocktemplate` longpoll wait on the async runtime (tip broadcast, the
method timeout, or stop). The blocking pool only performs the cheap read
after that wait, so one long-poll does not occupy a blocking thread for
the whole interval.

Esplora uses `X-Rbitcoin-Client` as a join-cache key only for a unix
socket or when join-header trust is set. The public listener does not
read the header. Loopback alone does not.
`SECURITY.md` names the REST cap as concurrent requests, not an
accepted-socket cap.

**Regression:** `rbitcoin-rpc` `server::unread_body_is_read_before_the_answer_up_to_the_cap`,
`server::tests::oversized_bearer_body_is_413_and_small_body_still_runs`,
`server::tests::large_block_submit_and_proposal_clear_http_413`,
`server::tests::waitfor_does_not_hold_the_blocking_pool`,
`rbitcoin-node` `config::tests::rpc_work_queue_zero_is_the_default_finite_queue`,
`rbitcoin-esplora` `server::tests::http_sh_join_last1_last_bulk_and_header_trust`.
