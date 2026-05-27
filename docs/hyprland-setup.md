# Hyprland Setup

> Filled in during **Step 6** (thin client) and **Step 8** (QoL bindings).

## Push-to-talk binds

```
# Press to start, release to stop+transcribe.
bindd = SUPER, Space, Start dictation, exec, whispy-client start
bindr = SUPER, Space, Stop dictation, exec, whispy-client stop
# Cancel and discard.
bind  = SUPER SHIFT, Space, Cancel dictation, exec, whispy-client cancel
```

`bindd`/`bindr` give distinct press/release events for true push-to-talk. Measure the
release latency early; fall back to a toggle bind if it exceeds ~100 ms (see handoff risks).
