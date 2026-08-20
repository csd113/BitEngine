# Vendored `tor-hsservice` patch

This directory contains the crates.io `tor-hsservice` 0.45.0 package, checksum
`3291bbf7641b457a16ef803db4d585334e669fa72fe194a276e4c6fc631f858d`,
published from Tor Project Arti commit
`009354f78d1a61214a878d6f1712a50844e6c215`.

BitEngine carries one behavioral correction in `src/ipt_mgr.rs`. Upstream's
`expire_old_expiry_times` documentation says to delete publication records once
their expiry has passed, but the 0.45.0 predicate does the inverse: it retains
expired records and deletes records that are still valid. That makes a restarted
onion service forget introduction points which are still listed in live
descriptors. The local patch retains records only while `expiry > now` and adds
a regression test for past, boundary, and future expiries.

The defect was exposed against the public Tor network after accepted rapid
disable/enable transitions: a fresh independent C Tor client received SOCKS
reply `0x05`. The coincident preserved state is the stronger evidence for this
specific source defect: `ipts.json` contained three current LIDs while
`iptpub.json` contained only three expired `T+0s` LIDs. That is the exact
persisted-state signature produced by the inverted predicate at upstream
`src/ipt_mgr.rs::expire_old_expiry_times`; the SOCKS failure alone would not
establish that causal path.

Upstream source: <https://gitlab.torproject.org/tpo/core/arti>

Remove this override after upgrading to an upstream release containing the same
correction.
