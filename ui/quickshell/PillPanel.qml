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

    function _parseAndApply(raw) {
        if (!raw || raw.length === 0) {
            // Empty file → keep last known state, let stale timer clear it
            return
        }
        try {
            const j = JSON.parse(raw)
            const st = (j.state || "idle").toString()

            // Stale check: if older than threshold, treat as idle
            const now = Date.now() / 1000
            if (typeof j.timestamp === "number" &&
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

    // Staleness ticker — every 1s, if last update > 5s old, force idle.
    Timer {
        interval: 1000
        running: true
        repeat: true
        onTriggered: {
            if (scope.dictationState === "idle") return
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
