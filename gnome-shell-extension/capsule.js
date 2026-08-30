// ABOUTME: Floating recording/transcribing capsule actor for Voxkey dictation.
// ABOUTME: Owns chrome placement, timers, positioning, and capsule controls.

import GObject from 'gi://GObject';
import GLib from 'gi://GLib';
import St from 'gi://St';
import Clutter from 'gi://Clutter';
import Pango from 'gi://Pango';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import {
    ACTIVE_STATES,
    AUDIO_HISTORY_LENGTH,
    ELAPSED_INTERVAL_MS,
    ICON_RECORDING,
    INDICATOR_BOTTOM_MARGIN,
    INDICATOR_DASH_GAP,
    INDICATOR_HEIGHT,
    INDICATOR_HIDE_DURATION_MS,
    INDICATOR_SHOW_DURATION_MS,
    INDICATOR_SIDE_MARGIN,
    INDICATOR_STATES,
    INDICATOR_WIDTH,
    PROCESSING_INTERVAL_MS,
    STATE_TRANSCRIBING,
} from './constants.js';

export const RecordingCapsule = GObject.registerClass(
class RecordingCapsule extends St.BoxLayout {
    _init({ cancellable, onCancel, onFinish }) {
        super._init({
            style_class: 'voxkey-recording-indicator',
            reactive: true,
            can_focus: true,
            track_hover: true,
            visible: false,
            vertical: true,
            width: INDICATOR_WIDTH,
            height: INDICATOR_HEIGHT,
            y_align: Clutter.ActorAlign.CENTER,
        });

        this._cancellable = cancellable;
        this._daemonState = '';
        this._audioLevel = 0;
        this._audioHistory = Array(AUDIO_HISTORY_LENGTH).fill(0);
        this._monitorsChangedId = 0;
        this._workareasChangedId = 0;
        this._focusWindowChangedId = 0;
        this._windowEnteredMonitorId = 0;
        this._processingTimerId = 0;
        this._audioTimerId = 0;
        this._elapsedTimerId = 0;
        this._processingFrame = 0;
        this._recordingStartedUs = 0;
        this._indicatorStateClass = '';
        this._recordingMonitorIndex = -1;
        this._overviewSignalIds = [];

        this._statusRow = new St.BoxLayout({
            style_class: 'voxkey-recording-status-row',
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._recordingBadge = new St.Bin({
            style_class: 'voxkey-recording-badge',
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._recordingIcon = new St.Icon({
            icon_name: ICON_RECORDING,
            icon_size: 12,
            style_class: 'voxkey-recording-indicator-icon',
        });
        this._recordingBadge.set_child(this._recordingIcon);
        this._recordingLabel = new St.Label({
            text: '',
            style_class: 'voxkey-recording-indicator-label',
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._recordingLabel.clutter_text.ellipsize = Pango.EllipsizeMode.END;
        this._recordingLabel.clutter_text.set_single_line_mode(true);
        this._timerLabel = new St.Label({
            text: '00:00',
            style_class: 'voxkey-recording-timer',
            y_align: Clutter.ActorAlign.CENTER,
        });

        this._activityBox = new St.BoxLayout({
            style_class: 'voxkey-recording-activity',
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._activityBars = [];
        for (let i = 0; i < AUDIO_HISTORY_LENGTH; i++) {
            const bar = new St.Widget({
                style_class: 'voxkey-recording-activity-bar',
                y_align: Clutter.ActorAlign.CENTER,
            });
            this._activityBox.add_child(bar);
            this._activityBars.push(bar);
        }

        this._contextLabel = new St.Label({
            text: '',
            style_class: 'voxkey-recording-context',
            x_align: Clutter.ActorAlign.FILL,
            x_expand: true,
        });
        this._contextLabel.clutter_text.ellipsize = Pango.EllipsizeMode.END;
        this._contextLabel.clutter_text.set_single_line_mode(true);
        this._transcriptLabel = new St.Label({
            text: '',
            style_class: 'voxkey-recording-transcript',
            x_align: Clutter.ActorAlign.FILL,
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
            height: 42,
        });
        this._transcriptLabel.clutter_text.ellipsize = Pango.EllipsizeMode.START;
        this._transcriptLabel.clutter_text.line_wrap = true;
        this._transcriptLabel.clutter_text.line_wrap_mode = Pango.WrapMode.WORD_CHAR;
        this._transcriptLabel.clutter_text.set_single_line_mode(false);

        this._actionRow = new St.BoxLayout({
            style_class: 'voxkey-recording-actions',
            x_align: Clutter.ActorAlign.CENTER,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._cancelButton = this._capsuleButton('Cancel', '', onCancel);
        this._transcribeButton = this._capsuleButton(
            'Finish', 'voxkey-capsule-button--transcribe', onFinish);
        for (const button of [
            this._cancelButton,
            this._transcribeButton,
        ])
            this._actionRow.add_child(button);

        this._statusRow.add_child(this._recordingBadge);
        this._statusRow.add_child(this._recordingLabel);
        this._statusRow.add_child(this._timerLabel);
        this._statusRow.add_child(this._activityBox);
        this.add_child(this._statusRow);
        this.add_child(this._contextLabel);
        this.add_child(this._transcriptLabel);
        this.add_child(this._actionRow);

        Main.layoutManager.addChrome(this, {
            affectsStruts: false,
            trackFullscreen: false,
        });
        this._monitorsChangedId = Main.layoutManager.connect(
            'monitors-changed', () => this._position());
        this._workareasChangedId = global.display.connect(
            'workareas-changed', () => this._position());
        this._focusWindowChangedId = global.display.connect(
            'notify::focus-window', () => this._position());
        this._windowEnteredMonitorId = global.display.connect(
            'window-entered-monitor', (_display, monitorIndex, window) => {
                if (window === global.display.focus_window) {
                    this._recordingMonitorIndex = monitorIndex;
                    this._position();
                }
            });
        for (const signal of ['showing', 'shown', 'hiding', 'hidden']) {
            this._overviewSignalIds.push(Main.overview.connect(
                signal, () => this._position()));
        }
    }

    _capsuleButton(label, modifier, callback) {
        const styleClass = ['voxkey-capsule-button', modifier]
            .filter(Boolean)
            .join(' ');
        const button = new St.Button({
            label,
            style_class: styleClass,
            reactive: true,
            can_focus: true,
            track_hover: true,
        });
        button.connect('clicked', callback);
        return button;
    }

    update({
        daemonState,
        subtitle,
        contextDescription,
        transcriptText,
        audioLevel,
        controlPending,
    }) {
        this._daemonState = daemonState;
        this._audioLevel = audioLevel;

        if (!INDICATOR_STATES.has(daemonState)) {
            this.hideIndicator();
            return;
        }

        const isRecording = ACTIVE_STATES.has(daemonState);
        this._recordingLabel.text = subtitle;
        this._contextLabel.text = contextDescription;
        // Freeze the capture duration while the final transcription runs.
        this._timerLabel.visible = true;
        this._activityBox.visible = true;
        this._transcriptLabel.text = transcriptText;
        this._updateAccessibleName(transcriptText);

        const stateClass = isRecording
            ? 'voxkey-recording-indicator--recording'
            : 'voxkey-recording-indicator--processing';
        this._recordingIcon.icon_name = isRecording
            ? ICON_RECORDING
            : 'view-refresh-symbolic';

        if (stateClass !== this._indicatorStateClass) {
            if (this._indicatorStateClass)
                this.remove_style_class_name(this._indicatorStateClass);
            this.add_style_class_name(stateClass);
            this._indicatorStateClass = stateClass;
        }

        this._transcribeButton.visible = isRecording;
        this._cancelButton.visible = true;
        this._actionRow.visible = true;
        this.height = INDICATOR_HEIGHT;
        if (this.visible)
            this._position();
        for (const button of [
            this._cancelButton,
            this._transcribeButton,
        ])
            button.reactive = !controlPending;

        if (isRecording) {
            this._stopProcessingAnimation();
            this._startAudioMeter();
        } else {
            this._stopAudioMeter();
            this._startProcessingAnimation();
        }
        this._show();
    }

    setTranscript(text) {
        if (!INDICATOR_STATES.has(this._daemonState))
            return;
        if (text !== this._transcriptLabel.text)
            this._transcriptLabel.text = text;
        this._updateAccessibleName(text);
    }

    pushAudioLevel(audioLevel) {
        this._audioLevel = audioLevel;
        if (!ACTIVE_STATES.has(this._daemonState))
            return;
        const level = Math.max(0, Math.min(1, Number.isFinite(this._audioLevel)
            ? this._audioLevel
            : 0));
        this._audioHistory.shift();
        this._audioHistory.push(level);
        this._paintAudioHistory();
    }

    syncElapsed(daemonState) {
        this._daemonState = daemonState;
        const active = ACTIVE_STATES.has(daemonState);
        if (active && !this._recordingStartedUs) {
            this._recordingStartedUs = GLib.get_monotonic_time();
            this._updateElapsedLabel();
        }
        if (active && !this._elapsedTimerId) {
            this._elapsedTimerId = GLib.timeout_add(
                GLib.PRIORITY_DEFAULT,
                ELAPSED_INTERVAL_MS,
                () => {
                    if (this._cancellable.is_cancelled() ||
                        !ACTIVE_STATES.has(this._daemonState)) {
                        this._elapsedTimerId = 0;
                        return GLib.SOURCE_REMOVE;
                    }
                    this._updateElapsedLabel();
                    return GLib.SOURCE_CONTINUE;
                });
        } else if (!active && daemonState === STATE_TRANSCRIBING) {
            // Capture has ended. Paint once at the transition boundary, then
            // retain the final duration until transcription completes.
            this._updateElapsedLabel();
            this._pauseElapsedTimer();
        } else if (!active) {
            // Keep the last value during the capsule's short exit animation.
            this._pauseElapsedTimer();
        }
    }

    resetElapsed() {
        this._pauseElapsedTimer();
        this._recordingStartedUs = 0;
        this._recordingMonitorIndex = -1;
        this._timerLabel.text = '00:00';
    }

    hideIndicator() {
        this._stopAudioMeter();
        this._stopProcessingAnimation();
        if (!this.visible) {
            this.resetElapsed();
            return;
        }
        this.remove_all_transitions();
        this.ease({
            opacity: 0,
            translation_y: 8,
            duration: INDICATOR_HIDE_DURATION_MS,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
            onComplete: () => {
                if (this._cancellable.is_cancelled())
                    return;
                if (!INDICATOR_STATES.has(this._daemonState)) {
                    this.hide();
                    this.translation_y = 0;
                    this.resetElapsed();
                }
            },
        });
    }

    _updateAccessibleName(text) {
        const state = this._recordingLabel.text;
        this.accessible_name = text
            ? `Voxkey: ${state}. ${text}`
            : `Voxkey: ${state}`;
    }

    _paintAudioHistory() {
        this._activityBars.forEach((bar, index) => {
            const shaped = Math.sqrt(this._audioHistory[index] ?? 0);
            bar.height = 3 + Math.round(shaped * 15);
            bar.opacity = 90 + Math.round(shaped * 165);
        });
    }

    _startAudioMeter() {
        if (this._cancellable.is_cancelled())
            return;
        if (this._audioTimerId) {
            GLib.source_remove(this._audioTimerId);
            this._audioTimerId = 0;
        }
        this._audioHistory.fill(0);
        this.pushAudioLevel(this._audioLevel);
        this._audioTimerId = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT,
            70,
            () => {
                if (!ACTIVE_STATES.has(this._daemonState)) {
                    this._audioTimerId = 0;
                    return GLib.SOURCE_REMOVE;
                }
                this.pushAudioLevel(this._audioLevel);
                return GLib.SOURCE_CONTINUE;
            });
    }

    _stopAudioMeter() {
        if (this._audioTimerId) {
            GLib.source_remove(this._audioTimerId);
            this._audioTimerId = 0;
        }
        this._audioHistory.fill(0);
    }

    _startProcessingAnimation() {
        if (this._cancellable.is_cancelled())
            return;
        if (this._processingTimerId) {
            GLib.source_remove(this._processingTimerId);
            this._processingTimerId = 0;
        }
        this._processingFrame = 0;
        this._paintProcessingFrame();
        this._processingTimerId = GLib.timeout_add(
            GLib.PRIORITY_DEFAULT,
            PROCESSING_INTERVAL_MS,
            () => {
                if (ACTIVE_STATES.has(this._daemonState) ||
                    this._daemonState !== STATE_TRANSCRIBING) {
                    this._processingTimerId = 0;
                    return GLib.SOURCE_REMOVE;
                }
                this._processingFrame++;
                this._paintProcessingFrame();
                return GLib.SOURCE_CONTINUE;
            });
    }

    _stopProcessingAnimation() {
        if (this._processingTimerId) {
            GLib.source_remove(this._processingTimerId);
            this._processingTimerId = 0;
        }
        if (!ACTIVE_STATES.has(this._daemonState)) {
            this._activityBars.forEach(bar => {
                bar.height = 3;
                bar.opacity = 100;
            });
        }
    }

    _paintProcessingFrame() {
        const count = this._activityBars.length;
        this._activityBars.forEach((bar, index) => {
            const distance = Math.abs(index - (this._processingFrame % count));
            const strength = Math.max(0, 1 - distance / 3);
            bar.height = 3 + Math.round(strength * 12);
            bar.opacity = 80 + Math.round(strength * 175);
        });
    }

    _updateElapsedLabel() {
        if (!this._recordingStartedUs)
            return;
        const seconds = Math.max(0, Math.floor(
            (GLib.get_monotonic_time() - this._recordingStartedUs) / 1_000_000));
        const minutes = Math.floor(seconds / 60);
        const remainder = seconds % 60;
        this._timerLabel.text = `${String(minutes).padStart(2, '0')}:${String(remainder).padStart(2, '0')}`;
    }

    _pauseElapsedTimer() {
        if (this._elapsedTimerId) {
            GLib.source_remove(this._elapsedTimerId);
            this._elapsedTimerId = 0;
        }
    }

    _position() {
        if (!this.visible)
            return;
        const monitors = Main.layoutManager.monitors;
        const focusWindow = global.display.focus_window;
        const focusedMonitorIndex = focusWindow
            ? focusWindow.get_monitor()
            : -1;
        if (focusedMonitorIndex >= 0 && focusedMonitorIndex < monitors.length)
            this._recordingMonitorIndex = focusedMonitorIndex;
        const primaryMonitor = Main.layoutManager.primaryMonitor;
        const primaryMonitorIndex = monitors.indexOf(primaryMonitor);
        const monitorIndex = this._recordingMonitorIndex >= 0 &&
            this._recordingMonitorIndex < monitors.length
            ? this._recordingMonitorIndex
            : primaryMonitorIndex;
        const monitor = monitorIndex >= 0 && monitorIndex < monitors.length
            ? monitors[monitorIndex]
            : primaryMonitor;
        if (!monitor)
            return;

        const workArea = monitorIndex >= 0
            ? Main.layoutManager.getWorkAreaForMonitor(monitorIndex)
            : monitor;
        const width = Math.min(
            INDICATOR_WIDTH,
            Math.max(1, workArea.width - INDICATOR_SIDE_MARGIN * 2));
        this.width = width;
        const height = this.height;
        let y = workArea.y + workArea.height - height - INDICATOR_BOTTOM_MARGIN;

        const dash = Main.overview.dash;
        if (monitor === primaryMonitor &&
            Main.overview.visible && dash && dash.visible) {
            const [, transformedDashY] = dash.get_transformed_position();
            const dashHeight = dash.height;
            const dashMargin = Number.isFinite(dash.margin_bottom)
                ? dash.margin_bottom
                : 0;
            if (Number.isFinite(dashHeight) && dashHeight > 0) {
                const restingDashY = workArea.y + workArea.height - dashHeight - dashMargin;
                const dashY = Number.isFinite(transformedDashY)
                    ? Math.min(transformedDashY, restingDashY)
                    : restingDashY;
                y = Math.min(y, Math.round(dashY) - height - INDICATOR_DASH_GAP);
            }
        }

        this.set_position(
            workArea.x + Math.round((workArea.width - width) / 2), y);
    }

    _show() {
        const wasVisible = this.visible;
        this.remove_all_transitions();
        if (!wasVisible) {
            this.opacity = 0;
            this.translation_y = 8;
            this.show();
            this._position();
        }
        this.ease({
            opacity: 255,
            translation_y: 0,
            duration: wasVisible ? 0 : INDICATOR_SHOW_DURATION_MS,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
        });
    }

    destroy() {
        this._stopAudioMeter();
        this._stopProcessingAnimation();
        this.resetElapsed();
        if (this._monitorsChangedId) {
            Main.layoutManager.disconnect(this._monitorsChangedId);
            this._monitorsChangedId = 0;
        }
        if (this._workareasChangedId) {
            global.display.disconnect(this._workareasChangedId);
            this._workareasChangedId = 0;
        }
        if (this._focusWindowChangedId) {
            global.display.disconnect(this._focusWindowChangedId);
            this._focusWindowChangedId = 0;
        }
        if (this._windowEnteredMonitorId) {
            global.display.disconnect(this._windowEnteredMonitorId);
            this._windowEnteredMonitorId = 0;
        }
        for (const id of this._overviewSignalIds)
            Main.overview.disconnect(id);
        this._overviewSignalIds = [];

        this.remove_all_transitions();
        Main.layoutManager.removeChrome(this);
        super.destroy();
    }
});
