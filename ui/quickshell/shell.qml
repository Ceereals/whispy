// whispy dictation pill — standalone Quickshell instance.
//
// Installed by `whispy-daemon setup` to ~/.config/quickshell/whispy/shell.qml and
// run as its own user service (whispy-pill.service: `quickshell -c whispy`), so the
// pill needs no edits to your main shell.qml. The `Whispy` module it imports lives
// at ~/.config/quickshell/Whispy/ (Quickshell adds the config root to the import path).
//
// Already run your own Quickshell bar? You can instead drop `import Whispy` +
// `PillPanel {}` into that shell and skip this service (`setup --no-pill`).
import Quickshell
import Whispy

ShellRoot {
    // Layer-shell overlay pill; reads $XDG_RUNTIME_DIR/whispy/state.json.
    PillPanel {}
}
