// Tests for the Omarchy bar module (../whispy.qml), run headless with stub
// Quickshell modules (./stubs) and a mock of the `bar` API Omarchy injects:
//
//   QT_QPA_PLATFORM=offscreen qmltestrunner -import ui/omarchy/tests/stubs \
//       -input ui/omarchy/tests/tst_whispy.qml

import QtQuick
import QtTest

Item {
    id: host
    width: 100; height: 40

    property var ran: []
    property var registered: []

    QtObject {
        id: mockBar
        property color foreground: "#ffffff"
        property color urgent: "#ff0000"
        property string fontFamily: "JetBrainsMono Nerd Font"
        property int barSize: 28
        property bool vertical: false
        function run(cmd) { host.ran.push(cmd) }
        function registerClickTarget(t) { host.registered.push(t) }
        function unregisterClickTarget(t) {}
        function showTooltip(t, s) {}
        function hideTooltip(t) {}
    }

    Loader { id: loader; source: "../whispy.qml" }
    property alias mod: loader.item

    TestCase {
        name: "WhispyBarModule"
        when: windowShown

        function initTestCase() {
            verify(loader.status === Loader.Ready, "whispy.qml failed to load")
            mod.bar = mockBar
            mod.settings = ({})
        }

        function stateJson(st, ageSecs, err) {
            return JSON.stringify({ state: st, rms: 0.3, error_kind: null,
                error_message: err || null, timestamp: Date.now() / 1000 - (ageSecs || 0) })
        }

        function test_registers_as_click_target() {
            compare(host.registered.length, 1)
            verify(host.registered[0] === mod)
            verify(typeof mod.triggerPress === "function")
        }

        function test_every_click_opens_the_settings() {
            var states = ["idle", "recording", "transcribing"]
            for (var i = 0; i < states.length; i++) {
                mod.apply(stateJson(states[i]))
                host.ran = []
                mod.triggerPress(Qt.LeftButton)
                mod.triggerPress(Qt.RightButton)
                mod.triggerPress(Qt.MiddleButton)
                compare(host.ran.length, 3)
                for (var j = 0; j < host.ran.length; j++) {
                    verify(host.ran[j].indexOf("omarchy-launch-or-focus whispy-settings") >= 0, host.ran[j])
                    verify(host.ran[j].indexOf("whispy-daemon settings") >= 0, host.ran[j])
                    // A click never starts, stops, or cancels dictation.
                    verify(host.ran[j].indexOf("whispy-client") < 0, host.ran[j])
                }
            }
            mod.apply(stateJson("idle"))
        }

        function test_commands_find_cargo_installs() {
            // The bar's shell may lack ~/.cargo/bin; it is appended, never prepended.
            verify(mod.commandFor(Qt.LeftButton).indexOf('PATH="$PATH:$HOME/.cargo/bin"') >= 0)
        }

        function test_states_map_to_glyph_and_color() {
            mod.apply(stateJson("recording"))
            compare(mod.dictationState, "recording")
            compare(mod.glyph.codePointAt(0), 0xF036C)
            compare(mod.glyphColor, mockBar.urgent)

            mod.apply(stateJson("transcribing"))
            compare(mod.glyph.codePointAt(0), 0xF051F)
            compare(mod.glyphColor, mockBar.foreground)

            mod.apply(stateJson("error", 0, "model not loaded"))
            compare(mod.glyph.codePointAt(0), 0xF036D)
            compare(mod.tooltip, "whispy: model not loaded · click for settings")
        }

        function test_long_transcription_is_not_stale() {
            // Written once; a slow CPU transcription must not drop to idle.
            mod.apply(stateJson("transcribing", 30))
            compare(mod.dictationState, "transcribing")
        }

        function test_stale_recording_is_idle() {
            mod.apply(stateJson("recording", 30))
            compare(mod.dictationState, "idle")
        }

        function test_success_flash_reverts_to_idle() {
            mod.apply(stateJson("success"))
            compare(mod.dictationState, "success")
            tryCompare(mod, "dictationState", "idle", 2000)
        }

        function test_malformed_input_keeps_state() {
            mod.apply(stateJson("transcribing"))
            mod.apply("{not json")
            mod.apply("")
            compare(mod.dictationState, "transcribing")
        }

        function test_hide_when_idle_setting() {
            mod.apply(stateJson("idle"))
            verify(mod.visible)
            mod.settings = ({ hideWhenIdle: true })
            verify(!mod.visible)
            mod.apply(stateJson("recording"))
            verify(mod.visible)
            mod.settings = ({})
        }

        function test_size_follows_bar() {
            compare(mod.implicitHeight, 28)
            mockBar.vertical = true
            compare(mod.implicitWidth, 28)
            mockBar.vertical = false
            compare(mod.implicitWidth, 26)
        }
    }
}
