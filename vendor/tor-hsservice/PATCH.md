# Vendored `tor-hsservice` patch

This directory contains the crates.io `tor-hsservice` 0.47.0 package, checksum
`55809560d9295dcc2ee77002c5a4275d1f5f5a54203de7cd6acb52215f5344bd`,
published from Tor Project Arti commit
`ce8bc6e0998bd5a4efdf06dd62dce53c98ea1087`.

The 0.47.0 release still contains the inverted predicate, so the existing
correction and regression test are retained on the updated upstream source.

BitEngine carries one behavioral correction in `src/ipt_mgr.rs`. Upstream's
`expire_old_expiry_times` documentation says to delete publication records once
their expiry has passed, but the 0.47.0 predicate does the inverse: it retains
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
