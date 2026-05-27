//! Text injection: clipboard save/restore + ydotool Ctrl+V.
//!
//! Implemented in Step 5: save the current clipboard (`wl-paste`), `wl-copy` the
//! transcript, paste via `ydotool key <injection.paste_keys>`, then restore the
//! previous clipboard after `injection.restore_clipboard_delay_ms` (skip restore
//! for non-text MIME types).
