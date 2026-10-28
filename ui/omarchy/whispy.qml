// whispy dictation indicator for the Omarchy 4 bar.
//
// The bar-widget entry point of the `whispy.dictation` Omarchy plugin (see
// manifest.json). `whispy-daemon setup` installs this folder to
// ~/.config/omarchy/plugins/whispy.dictation/ and, the first time, puts the
// widget on the bar. It watches the same state.json the pill reads (no polling,
// no extra processes) and drives the daemon through whispy-client.
//
//   left click    open whispy's settings (stops dictation while recording)
//   right click   toggle dictation
//   middle click  cancel the current recording
//
// Settings (Omarchy's widget settings, or the widget's shell.json entry):
//   "hideWhenIdle": true   only show while recording/transcribing/flashing
//   "statePath": "..."     override $XDG_RUNTIME_DIR/whispy/state.json
//
// The same file also loads as an Omarchy custom QML module
// ({ "id": "whispy", "type": "qml" } with ~/.config/omarchy/bar/modules/whispy.qml):
// both get the same injected properties.
//
// Uses only QtQuick + Quickshell.Io and the `bar` API Omarchy injects (colors,
// font, size, run, tooltips, click targets), so it needs nothing beyond what
// Omarchy already ships.

import QtQuick
import Quickshell
import Quickshell.Io

Item {
    id: root

    // Filled in by the Omarchy bar after loading.
    property var bar
    property string moduleName
    property var settings

    // Current daemon state: idle | recording | transcribing | success | error.
    property string dictationState: "idle"
    property string errorMessage: ""
    property real stateTimestamp: 0

    readonly property bool hideWhenIdle: setting("hideWhenIdle", false) === true
    readonly property string statePath: String(setting("statePath",
        Quickshell.env("XDG_RUNTIME_DIR") + "/whispy/state.json"))

    // Recording publishes RMS ticks many times a second; no update for this long
    // means the daemon went away mid-recording. Other states are written once, so
    // they are never treated as stale (a CPU transcription can take seconds).
    readonly property int staleRecordingMs: 5000
    // How long the transient success/error marks linger (mirrors the pill).
    readonly property int holdSuccessMs: 600
    readonly property int holdErrorMs: 1200

    visible: !(hideWhenIdle && dictationState === "idle")
    implicitWidth: bar && bar.vertical ? bar.barSize : 26
    implicitHeight: bar && bar.barSize > 0 ? bar.barSize : 26

    function setting(name, fallback) {
        var value = settings ? settings[name] : undefined
        return value === undefined || value === null ? fallback : value
    }

    // Nerd Font glyphs, matching the ones Omarchy's own dictation indicator uses.
    readonly property string glyph: {
        switch (dictationState) {
        case "recording":    return "󰍬"  // mic
        case "transcribing": return "󰔟"  // hourglass
        case "success":      return "󰄬"  // check
        case "error":        return "󰍭"  // mic-off
        default:             return "󰍬"
        }
    }

    readonly property color glyphColor: {
        if (!bar) return "white"
        if (dictationState === "recording" || dictationState === "error") return bar.urgent
        return bar.foreground
    }

    readonly property string tooltip: {
        switch (dictationState) {
        case "recording":    return "whispy: listening \u00b7 click to stop, middle-click to cancel"
        case "transcribing": return "whispy: transcribing"
        case "success":      return "whispy: done"
        case "error":        return "whispy: " + (errorMessage || "error")
        default:             return "whispy: ready \u00b7 click for settings, right-click to dictate"
        }
    }

    // --- Commands ---------------------------------------------------------------
    // The bar runs commands through `bash -lc` with the shell's environment, which
    // may lack ~/.cargo/bin (cargo installs). Append it, so packaged binaries still
    // win and cargo-installed ones are found.
    readonly property string pathFix: 'export PATH="$PATH:$HOME/.cargo/bin"; '
    // Settings TUI in a floating terminal, like Omarchy's own TUIs; the TUI titles
    // its window "whispy-settings", so a second click focuses it instead.
    readonly property string settingsCommand: pathFix
        + 'omarchy-launch-or-focus whispy-settings '
        + '"omarchy-launch-tui --app-id=TUI.float whispy-daemon settings"'

    // The command a click with `button` runs in the current state.
    function commandFor(button) {
        if (button === Qt.MiddleButton) return pathFix + "whispy-client cancel"
        if (button === Qt.RightButton) return pathFix + "whispy-client toggle"
        if (dictationState === "recording") return pathFix + "whispy-client toggle"
        return settingsCommand
    }

    // --- Bar click-target contract ------------------------------------------
    // The bar routes left clicks to registered targets via triggerPress(button);
    // right/middle reach our own MouseArea below.

    property var registeredBar: null

    function triggerPress(button) {
        if (!bar) return
        bar.run(commandFor(button))
    }

    onBarChanged: {
        if (registeredBar && registeredBar.unregisterClickTarget)
            registeredBar.unregisterClickTarget(root)
        registeredBar = bar
        if (bar && bar.registerClickTarget) bar.registerClickTarget(root)
    }

    Component.onDestruction: {
        if (registeredBar && registeredBar.unregisterClickTarget)
            registeredBar.unregisterClickTarget(root)
    }

    // --- State ----------------------------------------------------------------

    FileView {
        path: root.statePath
        watchChanges: true
        onFileChanged: reload()
        onTextChanged: root.apply(text())
    }

    function apply(raw) {
        // Empty or half-read file: keep the last known state.
        if (!raw || raw.length === 0) return
        try {
            var j = JSON.parse(raw)
            var st = String(j.state || "idle")
            var now = Date.now() / 1000
            var ts = typeof j.timestamp === "number" ? j.timestamp : now
            if (st === "recording" && (now - ts) * 1000 > staleRecordingMs) st = "idle"
            dictationState = st
            stateTimestamp = ts
            if (st === "error") errorMessage = String(j.error_message || "")

            if (st === "success") { hold.interval = holdSuccessMs; hold.restart() }
            else if (st === "error") { hold.interval = holdErrorMs; hold.restart() }
            else hold.stop()
        } catch (e) {
            console.warn("whispy bar module: state.json parse error:", e.message)
        }
    }

    // Drop the success/error mark back to idle (the daemon does too; this just
    // keeps the bar honest if that write is missed).
    Timer {
        id: hold
        repeat: false
        onTriggered: root.dictationState = "idle"
    }

    Timer {
        interval: 1000
        repeat: true
        running: root.dictationState === "recording"
        onTriggered: {
            if ((Date.now() / 1000 - root.stateTimestamp) * 1000 > root.staleRecordingMs)
                root.dictationState = "idle"
        }
    }

    // --- View -------------------------------------------------------------------

    Text {
        id: icon
        anchors.centerIn: parent
        text: root.glyph
        color: root.glyphColor
        opacity: root.dictationState === "idle" ? 0.6
               : root.dictationState === "recording" ? root.pulse : 1.0
        font.family: root.bar && root.bar.fontFamily ? root.bar.fontFamily : "monospace"
        font.pixelSize: 14
    }

    // Slow pulse while recording, so it reads as "live" at a glance. Animated on
    // its own property so the icon's opacity binding stays intact.
    property real pulse: 1.0
    SequentialAnimation on pulse {
        running: root.dictationState === "recording"
        loops: Animation.Infinite
        alwaysRunToEnd: true
        NumberAnimation { to: 0.45; duration: 600; easing.type: Easing.InOutQuad }
        NumberAnimation { to: 1.0; duration: 600; easing.type: Easing.InOutQuad }
    }

    MouseArea {
        anchors.fill: parent
        acceptedButtons: Qt.RightButton | Qt.MiddleButton
        onClicked: function(mouse) { root.triggerPress(mouse.button) }
    }

    HoverHandler {
        onHoveredChanged: {
            if (!root.bar) return
            if (hovered) root.bar.showTooltip(root, root.tooltip)
            else root.bar.hideTooltip(root)
        }
    }
}
