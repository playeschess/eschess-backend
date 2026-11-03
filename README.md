# eschess-backend

Rust backend for eSchess. The architecture is in [eschess-docs](https://github.com/playeschess/eschess-docs).

```
crates/domain   pure rules: money, clock, chess, rating (no I/O)
bins/api        REST API
```

More crates and binaries (ledger, storage, game, realtime, worker ...) are added step by step, following the
build order in the architecture document.

## Everyday commands

```
make run     # start the API, then: curl localhost:8080/health
make test    # all tests
make lint    # formatting and clippy, exactly what CI checks
```
