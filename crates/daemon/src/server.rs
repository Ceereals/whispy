//! Unix socket server: line-based JSON protocol (`Cmd` in, `Resp` out).
//!
//! Implemented in Step 2: bind `cfg.ipc.socket_path()`, remove a stale socket on
//! start, dispatch `Ping`/`Status` now and `Start`/`Stop`/`Cancel` in later steps.
