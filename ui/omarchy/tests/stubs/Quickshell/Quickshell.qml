// Test stub for Quickshell: only the env() lookup the bar module uses.
pragma Singleton
import QtQuick
QtObject {
    function env(name) { return name === "XDG_RUNTIME_DIR" ? "/nonexistent-runtime" : "" }
}
