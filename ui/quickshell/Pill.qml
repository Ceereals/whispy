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

    // ── Background + shadow ────────────────────────────────────────────────
    // The pill body and its drop shadow. Isolated on its own layer so the blur
    // is regenerated only when the silhouette changes (width morph / enter),
    // NOT on every inner-content frame (bars, spinner, glow) — that re-render
    // was the source of the global jank.
    Rectangle {
        id: shadowBg
        anchors.fill: parent
        radius: Tokens.pillRadius
        color: Tokens.pillBg
        border.color: Tokens.pillBorder
        border.width: 1
        antialiasing: true
        layer.enabled: true
        layer.effect: MultiEffect {
            shadowEnabled: true
            shadowColor: Tokens.pillShadowColor
            shadowBlur: Tokens.pillShadowBlur
            shadowVerticalOffset: Tokens.pillShadowOffsetY
            shadowHorizontalOffset: 0
        }
    }

    // ── Surface ────────────────────────────────────────────────────────────
    // Transparent clip container for the animated state layers; body/border/
    // shadow come from shadowBg beneath.
    Rectangle {
        id: surface
        anchors.fill: parent
        radius: Tokens.pillRadius
        color: "transparent"
        antialiasing: true
        clip: true

        // Recording ambient glow (pulse, independent of rms).
        // Pulse OPACITY (cheap GPU compositing), not border.width — animating a
        // rounded-rect border re-tessellates the stroke every frame and stutters.
        Rectangle {
            id: glow
            anchors.fill: parent
            radius: parent.radius
            color: "transparent"
            border.color: Tokens.accentRecordingGlow
            border.width: Tokens.glowBorderWidth   // FIXED — never animated
            antialiasing: true
            visible: root.dictationState === "recording" && root.showGlow
            opacity: 0
            SequentialAnimation on opacity {
                running: glow.visible
                loops:   Animation.Infinite
                NumberAnimation { from: Tokens.glowPulseMin; to: 1.0; duration: Tokens.durRecPulse / 2; easing.type: Easing.InOutSine }
                NumberAnimation { from: 1.0; to: Tokens.glowPulseMin; duration: Tokens.durRecPulse / 2; easing.type: Easing.InOutSine }
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

            // ── Voice → waveform pipeline ──────────────────────────────────
            // Mic RMS is tiny (peak ~0.015). Normalize by sensitivity, lift
            // quiet speech with a perceptual curve → `target` (0..1).
            property real target: 0
            Connections {
                target: root
                function onRmsChanged() {
                    const n = Math.min(1, Math.max(0, root.rms / Tokens.waveSensitivity))
                    recordingLayer.target = Math.pow(n, Tokens.wavePerceptual)
                }
            }

            // Driven by a ~60 Hz Timer (NOT FrameAnimation: this layer-shell
            // overlay isn't continuously repainted, so FrameAnimation never
            // ticks — but writing env/phase here schedules the repaint itself).
            // `env` is a time-constant follower (fast attack / slow release);
            // `phase` advances the per-bar organic wobble. Decoupled from the
            // ~20 Hz RMS feed so motion stays fluid and realtime.
            property real env:   0
            property real phase: 0
            function _seed() { env = 0; phase = 0 }
            onVisibleChanged: if (visible) _seed()

            Timer {
                interval: Tokens.waveTickMs
                repeat:   true
                running:  root.dictationState === "recording"
                onTriggered: {
                    const dt  = Tokens.waveTickMs / 1000
                    const t   = recordingLayer.target
                    const tau = t > recordingLayer.env ? Tokens.waveAttackTau
                                                       : Tokens.waveReleaseTau
                    recordingLayer.env   += (t - recordingLayer.env) * (1 - Math.exp(-dt / tau))
                    recordingLayer.phase += dt
                }
            }

            // Centered group: [mic][waveform][optional label]. Centering the
            // whole group (not just the bars) keeps left/right padding symmetric.
            Row {
                anchors.centerIn: parent
                spacing: Tokens.micWaveGap

                // Mic icon. The path is authored in a 24×24 viewBox, so it's drawn
                // into a 24px Shape and scaled down to micIconSize (centred via the
                // default Item.Center scale origin) — fixes both oversize + offset.
                Item {
                    width: Tokens.micIconSize; height: Tokens.micIconSize
                    anchors.verticalCenter: parent.verticalCenter
                    Shape {
                        width: 24; height: 24
                        anchors.centerIn: parent
                        scale: Tokens.micIconSize / 24
                        antialiasing: true
                        preferredRendererType: Shape.CurveRenderer   // analytic AA — crisp when scaled
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

                // Waveform bars — mirror peak biased to its own centre.
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

                                // Mirror: distance from the centre drives a
                                // centre-bias hump (taller middle, shorter edges).
                                readonly property real dist:
                                    Math.abs(index - (Tokens.waveBarCount - 1) / 2)
                                readonly property real hump:
                                    1 - (dist / ((Tokens.waveBarCount - 1) / 2)) * Tokens.waveHump
                                // Organic per-bar motion: two detuned sines with a
                                // per-bar phase. Multiplied into the level below, so
                                // its depth is gated by loudness → silence stays
                                // flat, speech is lively and non-uniform.
                                readonly property real wob:
                                    Math.sin(recordingLayer.phase * Tokens.waveWob1 + dist * 0.9)
                                  + Math.sin(recordingLayer.phase * Tokens.waveWob2 + dist * 1.7)
                                readonly property real level:
                                    Math.min(1, Math.max(0,
                                        recordingLayer.env * hump
                                            * (1 + wob * 0.5 * Tokens.waveOrganic)))

                                height: Tokens.waveMinHeight
                                      + level * (Tokens.waveMaxHeight - Tokens.waveMinHeight)
                                opacity: 0.55 + Math.min(0.45, level)
                            }
                        }
                    }
                }

                // Optional label
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
}
