//! Supervises the whisper-server child process (whisper.cpp Vulkan build).
//!
//! Implemented in Step 2: spawn `cfg.stt.server_bin` with the model, wait for the
//! HTTP healthcheck, restart on exit, and terminate it on daemon shutdown.
