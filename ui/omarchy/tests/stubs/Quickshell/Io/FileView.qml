import QtQuick
// Test stub: content is set by the test instead of read from disk.
QtObject {
    property string path
    property bool watchChanges
    property string _content: ""
    signal fileChanged()
    signal textChanged()
    function reload() {}
    function text() { return _content }
    function setContent(s) { _content = s; textChanged() }
}
