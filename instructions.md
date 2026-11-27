# Instructions for ban-rpcs (exact text: copy from here)

The squash round 4dcc35b67 is PASS, but the second commit subject was garbled in chat.

1. Reword the second commit (same tree, same parent c1d733ae1) so its subject is exactly:

       rpc: add setban, listbanned, and clearbanned

   The first commit stays `net: manual ban list with banlist.json persistence`. Force-update muse/ban-rpcs.
2. Then open the PR as already authorized (head `botcount:muse/ban-rpcs`, base `master`, draft false) with this exact title:

       rpc, net: add setban, listbanned, and clearbanned

   Body as described before: a few short plain sentences, no diff recap, no AI or tool attribution, ending with `Fixes #1707.`
   If the PR is already open, PATCH its title to the line above instead.
3. Reply in chat with the PR URL only.
