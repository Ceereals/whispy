import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Wayland
import Whispy

// Whispy / PillPanel.qml — layer-shell host window for the dictation Pill.
//
// Drop into your shell.qml:
//
//   import Whispy
//   PillPanel {}
//
// It will:
//   • spawn a click-through overlay at the bottom of every screen
//   • watch $XDG_RUNTIME_DIR/whispy/state.json (or the path you set)
//   • parse {state, rms, error_kind, error_message, timestamp}
//   • auto-hide after success/error per Tokens.hold*Ms
//   • treat stale state (timestamp > 5s old) as idle
Scope {
    id: scope

    // ── Config ─────────────────────────────────────────────────────────────
    property string statePath: Quickshell.env("XDG_RUNTIME_DIR") + "/whispy/state.json"
    property bool   showLabel: false
    property bool   showGlow:  true

    // ── State derived from the file ────────────────────────────────────────
    property string dictationState: "idle"   // idle | recording | transcribing | success | error
    property real   rms:            0.0
    property string errorMessage:   ""
    property real   stateTimestamp: 0

    // ── File watcher ───────────────────────────────────────────────────────
    FileView {
        id: stateFile
        path: scope.statePath
        watchChanges: true
        onFileChanged: reload()

        onTextChanged: scope._parseAndApply(text())
    }

    // The daemon writes state.json atomically (tmp + rename), which swaps the
    // inode out from under FileView's inotify watch — so onFileChanged stops
    // firing after the first replace and the pill never leaves "idle". Poll
    // reload() to pick up every write regardless of the rename. reload() on
    // unchanged content is a no-op (onTextChanged only fires on real changes).
    Timer {
        interval: 100
        running:  true
        repeat:   true
        onTriggered: stateFile.reload()
    }

    function _parseAndApply(raw) {
        if (!raw || raw.length === 0) {
            // Empty file → keep last known state, let stale timer clear it
            return
        }
        try {
            const j = JSON.parse(raw)
            const st = (j.state || "idle").toString()

            // Stale check: only `recording` is refreshed continuously (RMS ticks);
            // the other states are written once, and a CPU transcription can take
            // longer than the threshold, so they must not be aged out.
            const now = Date.now() / 1000
            if (st === "recording" && typeof j.timestamp === "number" &&
                (now - j.timestamp) * 1000 > Tokens.staleThresholdMs) {
                scope._setState("idle", 0, "")
                return
            }

            scope._setState(
                st,
                typeof j.rms === "number" ? j.rms : 0,
                (j.error_message || "").toString()
            )
            scope.stateTimestamp = j.timestamp || now

            // Schedule auto-hide for terminal states
            if (st === "success") holdToIdle.interval = Tokens.holdSuccessMs, holdToIdle.restart()
            else if (st === "error") holdToIdle.interval = Tokens.holdErrorMs,   holdToIdle.restart()
            else                     holdToIdle.stop()

        } catch (e) {
            // Parse error: stay silent, keep last known state (handoff §8.2)
            console.warn("Whispy: state.json parse error:", e.message)
        }
    }

    function _setState(s, r, em) {
        if (scope.dictationState !== s) scope.dictationState = s
        scope.rms = r
        if (em.length > 0) scope.errorMessage = em
    }

    // After holdMs in a terminal state, drop back to idle so the pill hides.
    Timer {
        id: holdToIdle
        repeat: false
        running: false
        onTriggered: scope._setState("idle", 0, "")
    }

    // Staleness ticker — every 1s while recording, if the last RMS tick is > 5s
    // old (the daemon went away mid-recording), force idle.
    Timer {
        interval: 1000
        running: scope.dictationState === "recording"
        repeat: true
        onTriggered: {
            const now = Date.now() / 1000
            if (scope.stateTimestamp > 0 &&
                (now - scope.stateTimestamp) * 1000 > Tokens.staleThresholdMs) {
                scope._setState("idle", 0, "")
            }
        }
    }

    // ── One PanelWindow per screen ────────────────────────────────────────
    Variants {
        model: Quickshell.screens

        PanelWindow {
            id: panel
            required property var modelData
            screen: modelData

            // Layer-shell config (handoff §2)
            WlrLayershell.layer:          WlrLayer.Overlay
            WlrLayershell.exclusiveZone:  0
            WlrLayershell.keyboardFocus:  WlrKeyboardFocus.None
            WlrLayershell.namespace:      "whispy-pill"

            anchors {
                bottom: true
                left:   true
                right:  true
            }
            implicitHeight: Tokens.pillHeight + Tokens.marginBottom + 16
            color: "transparent"

            // Click-through: only the pill itself catches input (and it doesn't).
            mask: Region {}

            Pill {
                id: pill
                anchors.bottom: parent.bottom
                anchors.bottomMargin: Tokens.marginBottom
                anchors.horizontalCenter: parent.horizontalCenter

                dictationState: scope.dictationState
                rms:            scope.rms
                errorMessage:   scope.errorMessage
                showLabel:      scope.showLabel
                showGlow:       scope.showGlow
            }
        }
    }
}
