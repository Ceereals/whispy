import QtQuick
import QtQuick.Layouts
import QtQuick.Shapes
import QtQuick.Effects
import Whispy

// Whispy / Pill.qml — the morphing dictation pill.
//
// Drop into any layer-shell PanelWindow. Bind `dictationState`, `rms`,
// `errorMessage`. The pill morphs width on state change and cross-fades
// inner content. Single Item — never two pills.
Item {
    id: root

    // ── Public API ─────────────────────────────────────────────────────────
    property string dictationState: "idle"   // idle | recording | transcribing | success | error
    property real   rms:            0.0
    property string errorMessage:   ""
    property bool   showLabel:      false
    property bool   showGlow:       true

    // ── Derived ────────────────────────────────────────────────────────────
    readonly property bool isVisibleState: dictationState !== "idle"
    readonly property int  targetWidth: {
        switch (dictationState) {
            case "recording":    return showLabel ? Tokens.widthRecordingLbl    : Tokens.widthRecording
            case "transcribing": return showLabel ? Tokens.widthTranscribingLbl : Tokens.widthTranscribing
            case "success":      return showLabel ? Tokens.widthSuccessLbl      : Tokens.widthSuccess
            case "error":
                const est = errorMessage.length * 7.6 + 80
                return Math.min(Tokens.widthErrorMax, Math.max(Tokens.widthErrorMin, est))
        }
        return Tokens.widthRecording
    }

    width:  targetWidth
    height: Tokens.pillHeight

    // Enter/exit: fade + slide-up 8px
    opacity: isVisibleState ? 1.0 : 0.0
    Behavior on opacity { NumberAnimation { duration: Tokens.durEnter; easing.type: Easing.OutCubic } }
    Behavior on width   { NumberAnimation { duration: Tokens.durMorph; easing.type: Easing.InOutQuad } }

    // Combined transform: shake (x) + slide-in (y)
    transform: [
        Translate { id: slideT; y: root.isVisibleState ? 0 : 8;
                    Behavior on y { NumberAnimation { duration: Tokens.durEnter; easing.type: Easing.OutCubic } } },
        Translate { id: shakeT; x: 0 }
    ]

    SequentialAnimation {
        id: shakeAnim
        NumberAnimation { target: shakeT; property: "x"; from: 0;  to: -4; duration: 40 }
        NumberAnimation { target: shakeT; property: "x"; from: -4; to:  4; duration: 50 }
        NumberAnimation { target: shakeT; property: "x"; from:  4; to: -3; duration: 50 }
        NumberAnimation { target: shakeT; property: "x"; from: -3; to:  3; duration: 50 }
        NumberAnimation { target: shakeT; property: "x"; from:  3; to:  0; duration: 60; easing.type: Easing.OutQuad }
    }

    property string _prevState: ""
    onDictationStateChanged: {
        if (dictationState === "error" && _prevState !== "error") shakeAnim.restart()
        _prevState = dictationState
    }

    // ── Surface ────────────────────────────────────────────────────────────
    Rectangle {
        id: surface
        anchors.fill: parent
        radius: Tokens.pillRadius
        color: Tokens.pillBg
        border.color: Tokens.pillBorder
        border.width: 1
        antialiasing: true
        clip: true

        // Recording ambient glow (1Hz pulse, independent of rms)
        Rectangle {
            anchors.fill: parent
            radius: parent.radius
            color: "transparent"
            border.color: Tokens.accentRecordingGlow
            border.width: 0
            opacity: (root.dictationState === "recording" && root.showGlow) ? 1 : 0
            Behavior on opacity { NumberAnimation { duration: Tokens.durFadeContent } }

            SequentialAnimation on border.width {
                running: root.dictationState === "recording" && root.showGlow
                loops:   Animation.Infinite
                NumberAnimation { from: 0; to: 4; duration: Tokens.durRecPulse / 2; easing.type: Easing.InOutSine }
                NumberAnimation { from: 4; to: 0; duration: Tokens.durRecPulse / 2; easing.type: Easing.InOutSine }
            }
        }

        // Error tint
        Rectangle {
            anchors.fill: parent
            radius: parent.radius
            color: Tokens.accentErrorBg
            opacity: root.dictationState === "error" ? 1 : 0
            Behavior on opacity { NumberAnimation { duration: Tokens.durFadeContent } }
        }

        // ─── RECORDING content ────────────────────────────────────────────
        Item {
            id: recordingLayer
            anchors.fill: parent
            opacity: root.dictationState === "recording" ? 1 : 0
            visible: opacity > 0.01
            Behavior on opacity { NumberAnimation { duration: Tokens.durFadeContent; easing.type: Easing.OutQuad } }

            // Smoothed RMS, ticked on every rms change
            property real smoothedRms: 0
            Connections {
                target: root
                function onRmsChanged() {
                    recordingLayer.smoothedRms +=
                        (root.rms - recordingLayer.smoothedRms) * Tokens.rmsSmoothing
                }
            }

            // Per-frame wobble timer (sin envelope per bar)
            property real t: 0
            NumberAnimation on t {
                running: root.dictationState === "recording"
                loops:   Animation.Infinite
                from:    0; to: 6.2831853; duration: 4000
            }

            Row {
                anchors.fill: parent
                anchors.leftMargin: 14
                anchors.rightMargin: 14
                spacing: showLabel ? 12 : 12

                // Mic icon
                Item {
                    width: 16; height: 16
                    anchors.verticalCenter: parent.verticalCenter
                    Shape {
                        anchors.fill: parent
                        antialiasing: true
                        ShapePath {
                            strokeColor: Tokens.accentRecording
                            strokeWidth: 2
                            fillColor: "transparent"
                            capStyle: ShapePath.RoundCap
                            joinStyle: ShapePath.RoundJoin
                            PathSvg { path: "M9 2 a3 3 0 0 1 6 0 v9 a3 3 0 0 1 -6 0 z M5 10 v2 a7 7 0 0 0 14 0 v-2 M12 21 v-2" }
                        }
                    }
                }

                // Waveform bars
                Item {
                    width: Tokens.waveBarCount * Tokens.waveBarWidth
                           + (Tokens.waveBarCount - 1) * Tokens.waveBarGap
                    height: Tokens.waveMaxHeight
                    anchors.verticalCenter: parent.verticalCenter

                    Row {
                        anchors.centerIn: parent
                        spacing: Tokens.waveBarGap
                        Repeater {
                            model: Tokens.waveBarCount
                            delegate: Rectangle {
                                required property int index
                                width: Tokens.waveBarWidth
                                radius: 1.5
                                color: Tokens.accentRecording
                                anchors.verticalCenter: parent.verticalCenter

                                readonly property real centerness:
                                    1 - Math.abs(index - (Tokens.waveBarCount - 1) / 2)
                                        / ((Tokens.waveBarCount - 1) / 2)
                                readonly property real envelope: 0.35 + centerness * 0.65
                                readonly property real wobble:
                                    (Math.sin(recordingLayer.t * 3 + index * 0.7)
                                   + Math.sin(recordingLayer.t * 5.3 + index * 1.4)) * 0.18
                                readonly property real targetH:
                                    Math.max(3, Math.min(
                                        Tokens.waveMaxHeight,
                                        (recordingLayer.smoothedRms * envelope * 1.6
                                       + wobble * recordingLayer.smoothedRms
                                       + 0.18) * Tokens.waveMaxHeight))

                                height: targetH
                                opacity: 0.55 + Math.min(0.45, targetH / Tokens.waveMaxHeight)
                                Behavior on height {
                                    NumberAnimation { duration: Tokens.durBar; easing.type: Easing.OutQuad }
                                }
                            }
                        }
                    }
                }

                Text {
                    visible: root.showLabel
                    anchors.verticalCenter: parent.verticalCenter
                    text: "Listening…"
                    color: Tokens.textPrimary
                    font.family: Tokens.fontFamily
                    font.pixelSize: Tokens.textSize
                    font.weight: Tokens.textWeight
                }
            }
        }

        // ─── TRANSCRIBING content ─────────────────────────────────────────
        Item {
            id: transcribingLayer
            anchors.fill: parent
            opacity: root.dictationState === "transcribing" ? 1 : 0
            visible: opacity > 0.01
            Behavior on opacity { NumberAnimation { duration: Tokens.durFadeContent; easing.type: Easing.OutQuad } }

            Row {
                anchors.centerIn: parent
                spacing: 10

                Item {
                    width: 18; height: 18
                    anchors.verticalCenter: parent.verticalCenter
                    // Track ring (full circle, dim)
                    Shape {
                        anchors.fill: parent
                        antialiasing: true
                        ShapePath {
                            strokeColor: Tokens.accentTranscribingDim
                            strokeWidth: 2.4
                            fillColor: "transparent"
                            PathAngleArc { centerX: 9; centerY: 9; radiusX: 7; radiusY: 7; startAngle: 0; sweepAngle: 360 }
                        }
                    }
                    // Spinning arc (¾ circle)
                    Item {
                        anchors.fill: parent
                        RotationAnimator on rotation {
                            running: root.dictationState === "transcribing"
                            loops:   Animation.Infinite
                            from: 0; to: 360
                            duration: Tokens.durSpinner
                        }
                        Shape {
                            anchors.fill: parent
                            antialiasing: true
                            ShapePath {
                                strokeColor: Tokens.accentTranscribing
                                strokeWidth: 2.4
                                fillColor: "transparent"
                                capStyle: ShapePath.RoundCap
                                PathAngleArc { centerX: 9; centerY: 9; radiusX: 7; radiusY: 7; startAngle: 0; sweepAngle: 270 }
                            }
                        }
                    }
                }

                Text {
                    visible: root.showLabel
                    anchors.verticalCenter: parent.verticalCenter
                    text: "Transcribing…"
                    color: Tokens.textPrimary
                    font.family: Tokens.fontFamily
                    font.pixelSize: Tokens.textSize
                    font.weight: Tokens.textWeight
                }
            }
        }

        // ─── SUCCESS content ──────────────────────────────────────────────
        Item {
            id: successLayer
            anchors.fill: parent
            opacity: root.dictationState === "success" ? 1 : 0
            visible: opacity > 0.01
            Behavior on opacity { NumberAnimation { duration: Tokens.durFadeContent; easing.type: Easing.OutQuad } }

            // Draw progress 0→1
            property real progress: 0
            onVisibleChanged: if (visible) drawAnim.restart()

            NumberAnimation {
                id: drawAnim
                target: successLayer
                property: "progress"
                from: 0; to: 1
                duration: Tokens.durCheckDraw
                easing.type: Easing.OutCubic
            }

            Row {
                anchors.centerIn: parent
                spacing: 8

                Item {
                    width: 20; height: 20
                    anchors.verticalCenter: parent.verticalCenter
                    // Circle backdrop
                    Shape {
                        anchors.fill: parent
                        antialiasing: true
                        opacity: 0.28
                        ShapePath {
                            strokeColor: Tokens.accentSuccess
                            strokeWidth: 2
                            fillColor: "transparent"
                            PathAngleArc { centerX: 10; centerY: 10; radiusX: 8; radiusY: 8; startAngle: 0; sweepAngle: 360 }
                        }
                    }
                    // Check stroke — drawn via dashOffset
                    Shape {
                        anchors.fill: parent
                        antialiasing: true
                        ShapePath {
                            strokeColor: Tokens.accentSuccess
                            strokeWidth: 2.4
                            fillColor: "transparent"
                            capStyle: ShapePath.RoundCap
                            joinStyle: ShapePath.RoundJoin
                            strokeStyle: ShapePath.DashLine
                            dashPattern: [14, 14]
                            dashOffset: 14 - successLayer.progress * 14
                            PathSvg { path: "M6.25 10.42 l2.5 2.5 l5 -5.42" }
                        }
                    }
                }

                Text {
                    visible: root.showLabel
                    anchors.verticalCenter: parent.verticalCenter
                    text: "Done"
                    color: Tokens.textPrimary
                    font.family: Tokens.fontFamily
                    font.pixelSize: Tokens.textSize
                    font.weight: Tokens.textWeight
                }
            }
        }

        // ─── ERROR content ────────────────────────────────────────────────
        Item {
            id: errorLayer
            anchors.fill: parent
            opacity: root.dictationState === "error" ? 1 : 0
            visible: opacity > 0.01
            Behavior on opacity { NumberAnimation { duration: Tokens.durFadeContent; easing.type: Easing.OutQuad } }

            Row {
                anchors.fill: parent
                anchors.leftMargin: 16
                anchors.rightMargin: 16
                spacing: 10

                Item {
                    width: 16; height: 16
                    anchors.verticalCenter: parent.verticalCenter
                    Shape {
                        anchors.fill: parent
                        antialiasing: true
                        ShapePath {
                            strokeColor: Tokens.accentError
                            strokeWidth: 2
                            fillColor: "transparent"
                            capStyle: ShapePath.RoundCap
                            joinStyle: ShapePath.RoundJoin
                            PathSvg { path: "M14.49 12 L8 1 L1.51 12 A1.33 1.33 0 0 0 2.67 14 L13.33 14 A1.33 1.33 0 0 0 14.49 12 Z M8 6 V9 M8 11 H8.01" }
                        }
                    }
                }

                Text {
                    anchors.verticalCenter: parent.verticalCenter
                    text: root.errorMessage
                    color: Tokens.textPrimary
                    font.family: Tokens.fontFamily
                    font.pixelSize: Tokens.textSizeError
                    font.weight: Tokens.textWeight
                    elide: Text.ElideRight
                    width: errorLayer.width - 26 - 16 - 16
                }
            }
        }
    }

    // Drop shadow on the whole pill (Qt6 MultiEffect — no QtGraphicalEffects)
    layer.enabled: true
    layer.effect: MultiEffect {
        shadowEnabled: true
        shadowColor: Tokens.pillShadowColor
        shadowBlur: Tokens.pillShadowBlur
        shadowVerticalOffset: Tokens.pillShadowOffsetY
        shadowHorizontalOffset: 0
    }
}
