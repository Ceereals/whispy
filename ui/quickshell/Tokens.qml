pragma Singleton
import QtQuick

// Whispy / Tokens.qml — single source of truth for design tokens.
// Mirrors the HTML design reference. Edit values here; every component reads
// from this singleton.
QtObject {
    // ── Pill geometry ──────────────────────────────────────────────────────
    readonly property int    pillHeight:           48
    readonly property int    pillRadius:           24      // pillHeight / 2
    readonly property int    marginBottom:         24      // distance from screen bottom

    // Computed widths — pill hugs content (override via showLabel)
    readonly property int    widthRecording:       134
    readonly property int    widthRecordingLbl:    240
    readonly property int    widthTranscribing:    60
    readonly property int    widthTranscribingLbl: 156
    readonly property int    widthSuccess:         56
    readonly property int    widthSuccessLbl:      96
    readonly property int    widthErrorMin:        180
    readonly property int    widthErrorMax:        360

    // ── Surface ────────────────────────────────────────────────────────────
    readonly property color  pillBg:               Qt.rgba(0.078, 0.086, 0.110, 0.80)  // rgba(20,22,28,0.80)
    readonly property color  pillBorder:           Qt.rgba(1, 1, 1, 0.08)
    readonly property color  pillBorderStrong:     Qt.rgba(1, 1, 1, 0.14)
    readonly property color  pillShadowColor:      Qt.rgba(0, 0, 0, 0.55)
    readonly property int    pillShadowOffsetY:    24
    readonly property real   pillShadowBlur:       1.0     // MultiEffect.shadowBlur is 0..1

    // ── Text ───────────────────────────────────────────────────────────────
    readonly property color  textPrimary:          Qt.rgba(1, 1, 1, 0.96)
    readonly property color  textSecondary:        Qt.rgba(1, 1, 1, 0.62)
    readonly property int    textSize:             13
    readonly property int    textSizeError:        12
    readonly property int    textWeight:           Font.Medium

    // ── Accent palette per state ───────────────────────────────────────────
    readonly property color  accentRecording:      "#ff6b7a"
    readonly property color  accentRecordingGlow:  Qt.rgba(1, 107/255, 122/255, 0.35)
    readonly property color  accentTranscribing:   "#a8b4ff"
    readonly property color  accentTranscribingDim: Qt.rgba(168/255, 180/255, 1, 0.18)
    readonly property color  accentSuccess:        "#34d399"
    readonly property color  accentSuccessDim:     Qt.rgba(52/255, 211/255, 153/255, 0.28)
    readonly property color  accentError:          "#ffb454"
    readonly property color  accentErrorBg:        Qt.rgba(1, 180/255, 84/255, 0.14)

    // ── Motion (handoff §5) ────────────────────────────────────────────────
    readonly property int    durEnter:             200     // idle → visible
    readonly property int    durMorph:             250     // state morph
    readonly property int    durLeave:             250
    readonly property int    durLeaveError:        300
    readonly property int    durFadeContent:       180
    readonly property int    durShake:             250
    readonly property int    durCheckDraw:         220
    readonly property int    durSpinner:           900
    readonly property int    durRecPulse:          2000

    // ── Waveform ───────────────────────────────────────────────────────────
    readonly property int    waveBarCount:         16
    readonly property int    waveBarWidth:         3
    readonly property int    waveBarGap:           2
    readonly property int    waveMaxHeight:        24
    readonly property int    waveMinHeight:        2       // bar floor in px
    // Meter mapping. Measured speech RMS is tiny (median ~0.0024, peak ~0.015,
    // silence ~0). Normalize to 0..1 by sensitivity, then a perceptual curve
    // (<1) lifts quiet speech. Rendered every vsync frame: a time-constant
    // follower (fast attack / slow release, VU ballistics) chases the level,
    // and a per-bar organic wobble — gated by the level so silence stays flat —
    // gives lively, non-uniform motion. Centre-biased (mirror) via waveHump.
    readonly property real   waveSensitivity:      0.015   // rms that maps to full height
    readonly property real   wavePerceptual:       0.6     // power curve (<1 = expand quiet)
    readonly property real   waveAttackTau:        0.05    // follower rise time-const (s, fast)
    readonly property real   waveReleaseTau:       0.22    // follower fall time-const (s, slow)
    readonly property real   waveHump:             0.55    // centre bias: edge height falloff
    readonly property real   waveOrganic:          0.6     // per-bar wobble depth (level-gated)
    readonly property real   waveWob1:             5.0     // organic sine 1 speed (rad/s)
    readonly property real   waveWob2:             8.3     // organic sine 2 speed (rad/s)
    readonly property int    waveTickMs:           16      // follower/render tick (~60 fps)
    readonly property int    micIconSize:          18      // recording mic visual box (24-space path scaled to fit)
    readonly property int    micWaveGap:           8       // mic → waveform gap (recording Row spacing)

    // Recording glow pulse (opacity-animated, fixed border width)
    readonly property int    glowBorderWidth:      3
    readonly property real   glowPulseMin:         0.25    // opacity floor of the pulse

    // ── Lifecycle (auto-hide after success/error) ─────────────────────────
    readonly property int    holdSuccessMs:        600
    readonly property int    holdErrorMs:          1200
    readonly property int    staleThresholdMs:     5000

    // ── Font ───────────────────────────────────────────────────────────────
    readonly property string fontFamily:           "Inter"
}
