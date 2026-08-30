// ABOUTME: Interactive GNOME Quick Settings control surface for Voxkey dictation.
// ABOUTME: D-Bus proxy, menu actions, and ownership of the floating RecordingCapsule.

import GObject from 'gi://GObject';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import St from 'gi://St';

import { QuickMenuToggle, SystemIndicator } from 'resource:///org/gnome/shell/ui/quickSettings.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import { RecordingCapsule } from './capsule.js';
import {
    ACTIVE_STATES,
    BUS_NAME,
    BUSY_STATES,
    CANCELLABLE_STATES,
    CONTROL_CALL_TIMEOUT_MS,
    ICON_ERROR,
    ICON_IDLE,
    ICON_OFFLINE,
    ICON_RECORDING,
    INDICATOR_STATES,
    INTERFACE,
    MENU_TEXT_MAX_CHARS,
    METHOD_CANCEL,
    METHOD_DISMISS_ERROR,
    METHOD_INSERT_LAST,
    METHOD_START,
    METHOD_STOP,
    OBJECT_PATH,
    PROP_AUDIO_INPUT_DEVICE,
    PROP_AUDIO_LEVEL,
    PROP_LAST_ERROR,
    PROP_LAST_TRANSCRIPT,
    PROP_LIVE_TRANSCRIPT,
    PROP_PORTAL_CONNECTED,
    PROP_SHORTCUT_DESCRIPTION,
    PROP_SHORTCUT_TRIGGER,
    PROP_STATE,
    PROP_TRANSCRIBER_CONFIG,
    QUICK_SETTINGS_FOCUS_DELAY_MS,
    STATE_IDLE,
    STATE_CONNECTING,
    STATE_RECORDING,
    STATE_STREAMING,
    STATE_TRANSCRIBING,
    STATE_INJECTING,
    STATE_RECOVERING,
    TRANSCRIPT_MAX_CHARS,
    TRANSCRIPT_MENU_MAX_CHARS,
} from './constants.js';
import { menuPreview, readableShortcut, truncateText } from './util.js';

const VoxkeyToggle = GObject.registerClass(
class VoxkeyToggle extends QuickMenuToggle {
    _init() {
        super._init({
            title: 'Voxkey',
            iconName: ICON_OFFLINE,
            toggleMode: true,
        });

        this._daemonState = STATE_IDLE;
        this._portalConnected = false;
        this._shortcutTrigger = '';
        this._shortcutDescription = '';
        this._liveTranscript = '';
        this._lastTranscript = '';
        this._lastError = '';
        this._audioLevel = 0;
        this._audioInputDevice = '';
        this._transcriberConfig = '';
        this._proxy = null;
        this._cancellable = new Gio.Cancellable();
        this._propertyHandlerId = 0;
        this._focusDelayId = 0;
        this._focusDelayResolve = null;
        this._reconnectId = 0;
        this._ownerHandlerId = 0;
        this._controlPending = false;
        this._capsule = null;

        this._buildMenu();
        this._capsule = new RecordingCapsule({
            cancellable: this._cancellable,
            onCancel: () => this._callControl(METHOD_CANCEL, 'Could not cancel dictation'),
            onFinish: () => this._callControl(METHOD_STOP, 'Could not finish dictation'),
        });

        // The tile itself is the fastest path: start when idle, stop while
        // capturing. The arrow still opens the full Quick Settings menu.
        this.connect('clicked', () => this._runPrimaryAction());
        this._connectDaemon();
    }

    _buildMenu() {
        this.menu.setHeader(ICON_OFFLINE, 'Voxkey', 'Connecting to Voxkey…');

        this._primaryActionItem = new PopupMenu.PopupMenuItem('Start dictation');
        this._primaryActionItem.connect('activate', () => this._runPrimaryAction());
        this.menu.addMenuItem(this._primaryActionItem);

        this._cancelItem = new PopupMenu.PopupMenuItem('Cancel dictation');
        this._cancelItem.connect('activate', () => this._callControl(
            METHOD_CANCEL, 'Could not cancel dictation', true));
        this.menu.addMenuItem(this._cancelItem);

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._lastTranscriptItem = new PopupMenu.PopupMenuItem('No recent transcript yet');
        this._lastTranscriptItem.setSensitive(false);
        this._lastTranscriptItem.connect('activate', () => this._openSettings('history'));
        this.menu.addMenuItem(this._lastTranscriptItem);

        this._copyLastItem = new PopupMenu.PopupMenuItem('Copy last transcript');
        this._copyLastItem.connect('activate', () => this._copyText(
            this._lastTranscript, 'Last transcript copied'));
        this.menu.addMenuItem(this._copyLastItem);

        this._insertLastItem = new PopupMenu.PopupMenuItem('Type last transcript');
        this._insertLastItem.connect('activate', () => this._callControl(
            METHOD_INSERT_LAST, 'Could not type the last transcript', true));
        this.menu.addMenuItem(this._insertLastItem);

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._providerItem = new PopupMenu.PopupMenuItem('Transcription: …');
        this._providerItem.connect('activate', () => this._openSettings('transcription'));
        this.menu.addMenuItem(this._providerItem);

        this._microphoneItem = new PopupMenu.PopupMenuItem('Microphone: …');
        this._microphoneItem.connect('activate', () => this._openSettings('audio'));
        this.menu.addMenuItem(this._microphoneItem);

        this._shortcutItem = new PopupMenu.PopupMenuItem('Shortcut: …');
        this._shortcutItem.connect('activate', () => this._openSettings('general'));
        this.menu.addMenuItem(this._shortcutItem);

        this._errorSeparator = new PopupMenu.PopupSeparatorMenuItem();
        this.menu.addMenuItem(this._errorSeparator);

        this._errorItem = new PopupMenu.PopupMenuItem('');
        this._errorItem.setSensitive(false);
        this._errorItem.connect('activate', () => this._openSettings('general'));
        this.menu.addMenuItem(this._errorItem);

        this._copyErrorItem = new PopupMenu.PopupMenuItem('Copy error details');
        this._copyErrorItem.connect('activate', () => this._copyText(
            this._lastError, 'Error details copied'));
        this.menu.addMenuItem(this._copyErrorItem);

        this._dismissErrorItem = new PopupMenu.PopupMenuItem('Dismiss error');
        this._dismissErrorItem.connect('activate', () => {
            const expected = this._lastError;
            if (!expected)
                return;
            this._callControl(
                METHOD_DISMISS_ERROR,
                'Could not dismiss the error',
                false,
                new GLib.Variant('(s)', [expected]));
        });
        this.menu.addMenuItem(this._dismissErrorItem);

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        this._openSettingsItem = new PopupMenu.PopupMenuItem('Open settings');
        this._openSettingsItem.connect('activate', () => this._openSettings());
        this.menu.addMenuItem(this._openSettingsItem);
    }

    async _connectDaemon() {
        try {
            const proxy = await this._buildProxy();
            if (this._cancellable.is_cancelled())
                return;
            this._proxy = proxy;
            this._propertyHandlerId = this._proxy.connect(
                'g-properties-changed',
                (_proxy, changed) => this._onPropertiesChanged(changed));
            this._ownerHandlerId = this._proxy.connect(
                'notify::g-name-owner', () => this._onNameOwnerChanged());
            this._onNameOwnerChanged();
        } catch (error) {
            if (this._cancellable.is_cancelled())
                return;
            console.error(`Voxkey connection failed: ${this._cleanError(error)}`);
            this._setOffline('Not running');
            if (!this._cancellable.is_cancelled() && !this._reconnectId) {
                this._reconnectId = GLib.timeout_add_seconds(
                    GLib.PRIORITY_DEFAULT, 5, () => {
                        this._reconnectId = 0;
                        this._connectDaemon();
                        return GLib.SOURCE_REMOVE;
                    });
            }
        }
    }

    _onNameOwnerChanged() {
        if (this._cancellable.is_cancelled())
            return;
        if (!this._proxy || !this._proxy.get_name_owner()) {
            this._setOffline('Not running');
            return;
        }
        this._refreshAll();
    }

    _buildProxy() {
        return new Promise((resolve, reject) => {
            Gio.DBusProxy.new_for_bus(
                Gio.BusType.SESSION,
                Gio.DBusProxyFlags.NONE,
                null,
                BUS_NAME,
                OBJECT_PATH,
                INTERFACE,
                this._cancellable,
                (_source, result) => {
                    try {
                        resolve(Gio.DBusProxy.new_for_bus_finish(result));
                    } catch (error) {
                        reject(error);
                    }
                });
        });
    }

    _cached(name, fallback) {
        if (!this._proxy)
            return fallback;
        const value = this._proxy.get_cached_property(name);
        if (value === null)
            return fallback;
        return value.deepUnpack();
    }

    _daemonAvailable() {
        return Boolean(this._proxy && this._proxy.get_name_owner());
    }

    _refreshAll() {
        if (this._cancellable.is_cancelled())
            return;
        this._daemonState = this._cached(PROP_STATE, STATE_IDLE);
        this._portalConnected = this._cached(PROP_PORTAL_CONNECTED, false);
        this._shortcutTrigger = this._cached(PROP_SHORTCUT_TRIGGER, '');
        this._shortcutDescription = this._cached(PROP_SHORTCUT_DESCRIPTION, '');
        this._liveTranscript = this._cached(PROP_LIVE_TRANSCRIPT, '');
        this._lastTranscript = this._cached(PROP_LAST_TRANSCRIPT, '');
        this._lastError = this._cached(PROP_LAST_ERROR, '');
        this._audioLevel = Number(this._cached(PROP_AUDIO_LEVEL, 0));
        this._audioInputDevice = this._cached(PROP_AUDIO_INPUT_DEVICE, '');
        this._transcriberConfig = this._cached(PROP_TRANSCRIBER_CONFIG, '');
        this._render();
    }

    _onPropertiesChanged(changed) {
        if (this._cancellable.is_cancelled())
            return;
        const values = changed.recursiveUnpack();
        const keys = Object.keys(values);
        if (PROP_STATE in values)
            this._daemonState = values[PROP_STATE];
        if (PROP_PORTAL_CONNECTED in values)
            this._portalConnected = values[PROP_PORTAL_CONNECTED];
        if (PROP_SHORTCUT_TRIGGER in values)
            this._shortcutTrigger = values[PROP_SHORTCUT_TRIGGER];
        if (PROP_SHORTCUT_DESCRIPTION in values)
            this._shortcutDescription = values[PROP_SHORTCUT_DESCRIPTION];
        if (PROP_LIVE_TRANSCRIPT in values)
            this._liveTranscript = values[PROP_LIVE_TRANSCRIPT];
        if (PROP_LAST_TRANSCRIPT in values)
            this._lastTranscript = values[PROP_LAST_TRANSCRIPT];
        if (PROP_LAST_ERROR in values)
            this._lastError = values[PROP_LAST_ERROR];
        if (PROP_AUDIO_LEVEL in values)
            this._audioLevel = Number(values[PROP_AUDIO_LEVEL]);
        if (PROP_AUDIO_INPUT_DEVICE in values)
            this._audioInputDevice = values[PROP_AUDIO_INPUT_DEVICE];
        if (PROP_TRANSCRIBER_CONFIG in values)
            this._transcriberConfig = values[PROP_TRANSCRIBER_CONFIG];

        // These two properties can update many times per second. Avoid
        // rebuilding menu labels and state classes when only their actors move.
        if (keys.length === 1 && keys[0] === PROP_LIVE_TRANSCRIPT) {
            this._updateTranscriptLabel();
            return;
        }
        if (keys.length === 1 && keys[0] === PROP_AUDIO_LEVEL) {
            if (this._capsule)
                this._capsule.pushAudioLevel(this._audioLevel);
            return;
        }
        this._render();
    }

    _render() {
        if (this._cancellable.is_cancelled())
            return;
        const isActive = ACTIVE_STATES.has(this._daemonState);
        const subtitle = this._humanState();
        const icon = this._lastError && !BUSY_STATES.has(this._daemonState)
            ? ICON_ERROR
            : isActive
                ? ICON_RECORDING
                : this._portalConnected ? ICON_IDLE : ICON_OFFLINE;

        this.subtitle = subtitle;
        this.checked = isActive;
        this.iconName = icon;
        this.menu.setHeader(icon, 'Voxkey', subtitle);
        this._renderMenu();
        if (this._capsule)
            this._capsule.syncElapsed(this._daemonState);
        this._updateRecordingIndicator(subtitle);
    }

    _renderMenu() {
        const idle = this._daemonState === STATE_IDLE;
        const active = ACTIVE_STATES.has(this._daemonState);
        const canControl = this._portalConnected && !this._controlPending;
        if (!this._daemonAvailable()) {
            this._primaryActionItem.label.text = 'Open settings to start Voxkey…';
            this._primaryActionItem.setSensitive(!this._controlPending);
        } else if (this._daemonState === STATE_RECOVERING) {
            this._primaryActionItem.label.text = 'Restoring desktop access…';
            this._primaryActionItem.setSensitive(false);
        } else if (!this._portalConnected) {
            this._primaryActionItem.label.text = 'Set up desktop access…';
            this._primaryActionItem.setSensitive(!this._controlPending);
        } else if (idle) {
            this._primaryActionItem.label.text = 'Start dictation';
            this._primaryActionItem.setSensitive(canControl);
        } else if (active) {
            this._primaryActionItem.label.text = 'Finish dictation';
            this._primaryActionItem.setSensitive(canControl);
        } else {
            this._primaryActionItem.label.text = this._humanState();
            this._primaryActionItem.setSensitive(false);
        }

        this._cancelItem.visible = this._daemonAvailable() &&
            CANCELLABLE_STATES.has(this._daemonState);
        this._cancelItem.setSensitive(canControl);
        const hasTranscript = Boolean(this._lastTranscript);
        this._lastTranscriptItem.label.text = hasTranscript
            ? `Last: ${menuPreview(this._lastTranscript, TRANSCRIPT_MENU_MAX_CHARS)}`
            : 'No recent transcript yet';
        this._lastTranscriptItem.setSensitive(hasTranscript);
        this._copyLastItem.visible = hasTranscript;
        this._insertLastItem.visible = hasTranscript;
        this._copyLastItem.setSensitive(hasTranscript);
        this._insertLastItem.setSensitive(hasTranscript && idle && canControl);

        this._providerItem.label.text = `Transcription: ${this._providerDescription()}`;
        this._microphoneItem.label.text = `Microphone: ${menuPreview(
            this._microphoneDescription(), TRANSCRIPT_MENU_MAX_CHARS)}`;
        const shortcut = readableShortcut(
            this._shortcutDescription || this._shortcutTrigger);
        this._shortcutItem.label.text = shortcut
            ? `Shortcut: ${shortcut}`
            : 'Shortcut: not configured';

        const hasError = Boolean(this._lastError);
        this._errorSeparator.visible = hasError;
        this._errorItem.visible = hasError;
        this._copyErrorItem.visible = hasError;
        this._dismissErrorItem.visible = hasError;
        this._errorItem.label.text = hasError
            ? `Error: ${menuPreview(this._lastError, MENU_TEXT_MAX_CHARS)}`
            : '';
        this._errorItem.setSensitive(hasError);
        this._copyErrorItem.setSensitive(hasError);
        this._dismissErrorItem.setSensitive(hasError && this._daemonAvailable() && !this._controlPending);
    }

    _runPrimaryAction() {
        if (this._controlPending || this._daemonState === STATE_RECOVERING)
            return;
        if (!this._daemonAvailable()) {
            this._openSettings('general');
            return;
        }
        if (!this._portalConnected) {
            this._openSettings('permissions');
            return;
        }
        if (this._daemonState === STATE_IDLE) {
            this._callControl(METHOD_START, 'Could not start dictation', true);
        } else if (ACTIVE_STATES.has(this._daemonState)) {
            this._callControl(METHOD_STOP, 'Could not finish dictation', true);
        }
    }

    async _callControl(method, failureTitle, dismissQuickSettings = false, parameters = null) {
        const proxy = this._proxy;
        const cancellable = this._cancellable;
        if (!proxy || cancellable.is_cancelled() || this._controlPending)
            return;

        this._controlPending = true;
        this._render();
        try {
            if (dismissQuickSettings) {
                Main.panel.closeQuickSettings();
                await new Promise(resolve => {
                    if (this._focusDelayId) {
                        GLib.source_remove(this._focusDelayId);
                        this._focusDelayId = 0;
                    }
                    this._focusDelayResolve = resolve;
                    this._focusDelayId = GLib.timeout_add(
                        GLib.PRIORITY_DEFAULT,
                        QUICK_SETTINGS_FOCUS_DELAY_MS,
                        () => {
                            this._focusDelayId = 0;
                            const settle = this._focusDelayResolve;
                            this._focusDelayResolve = null;
                            if (settle)
                                settle();
                            return GLib.SOURCE_REMOVE;
                        });
                });
                if (cancellable.is_cancelled())
                    return;
            }
            await new Promise((resolve, reject) => {
                proxy.call(
                    method,
                    parameters,
                    Gio.DBusCallFlags.NONE,
                    CONTROL_CALL_TIMEOUT_MS,
                    cancellable,
                    (_source, result) => {
                        try {
                            proxy.call_finish(result);
                            resolve();
                        } catch (error) {
                            if (cancellable.is_cancelled())
                                resolve();
                            else
                                reject(error);
                        }
                    });
            });
        } catch (error) {
            if (!cancellable.is_cancelled())
                Main.notifyError('Voxkey', `${failureTitle}: ${this._cleanError(error)}`);
        } finally {
            if (!cancellable.is_cancelled()) {
                this._controlPending = false;
                this._render();
            }
        }
    }

    _cleanError(error) {
        const message = error && error.message ? error.message : error;
        return String(message)
            .replace(/^GDBus\.Error:[^:]+:\s*/, '')
            .trim();
    }

    _copyText(text, notification) {
        if (!text)
            return;
        Main.panel.closeQuickSettings();
        St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, text);
        Main.notify('Voxkey', notification);
    }

    _transcriptDisplayText() {
        if (!this._liveTranscript)
            return this._emptyPreviewText();
        return truncateText(this._liveTranscript, TRANSCRIPT_MAX_CHARS, true);
    }

    _updateTranscriptLabel() {
        if (!this._capsule)
            return;
        if (!INDICATOR_STATES.has(this._daemonState))
            return;
        this._capsule.setTranscript(this._transcriptDisplayText());
    }

    _providerDescription() {
        try {
            const config = JSON.parse(this._transcriberConfig || '{}');
            switch (config.provider) {
                case 'mistral': return 'Mistral';
                case 'mistral-realtime': return 'Mistral Realtime';
                case 'parakeet': {
                    const parakeet = config.parakeet || {};
                    const model = parakeet.model;
                    const name = model === 'parakeet-tdt-0.6b-v2'
                        ? 'Parakeet v2'
                        : model === 'parakeet-tdt-0.6b-v3'
                            ? 'Parakeet v3'
                            : 'Parakeet (custom model)';
                    return parakeet.backend === 'http'
                        ? `${name} Server`
                        : name;
                }
                case 'whisper-cpp': return 'Whisper.cpp';
                default: return 'Not configured';
            }
        } catch (_error) {
            return 'Not configured';
        }
    }

    _microphoneDescription() {
        return this._audioInputDevice || 'System default';
    }

    _contextDescription() {
        return `${this._providerDescription()} · ${this._microphoneDescription()}`;
    }

    _updateRecordingIndicator(subtitle) {
        if (!this._capsule)
            return;
        if (!INDICATOR_STATES.has(this._daemonState)) {
            this._liveTranscript = '';
            this._capsule.hideIndicator();
            return;
        }

        this._capsule.update({
            daemonState: this._daemonState,
            subtitle,
            contextDescription: this._contextDescription(),
            transcriptText: this._transcriptDisplayText(),
            audioLevel: this._audioLevel,
            controlPending: this._controlPending,
        });
    }

    _humanState() {
        if (this._daemonState === STATE_RECOVERING)
            return 'Restoring desktop access';
        if (!this._portalConnected)
            return this._lastError ? 'Needs attention' : 'Desktop access needed';
        switch (this._daemonState) {
            case STATE_IDLE: return this._lastError ? 'Needs attention' : 'Ready';
            case STATE_CONNECTING: return 'Connecting';
            case STATE_RECORDING: return 'Listening';
            case STATE_STREAMING: return 'Listening and transcribing';
            case STATE_TRANSCRIBING: return 'Transcribing';
            case STATE_INJECTING: return 'Typing transcription';
            default: return this._daemonState;
        }
    }

    _emptyPreviewText() {
        if (ACTIVE_STATES.has(this._daemonState))
            return 'Listening…';
        if (this._daemonState === STATE_TRANSCRIBING)
            return 'Finalizing transcription…';
        return '';
    }

    _setOffline(reason) {
        this._portalConnected = false;
        this._liveTranscript = '';
        this._audioLevel = 0;
        const showsError = Boolean(this._lastError);
        this.subtitle = showsError ? 'Needs attention' : reason;
        this.checked = false;
        this.iconName = showsError ? ICON_ERROR : ICON_OFFLINE;
        this.menu.setHeader(this.iconName, 'Voxkey', this.subtitle);
        this._primaryActionItem.setSensitive(false);
        this._cancelItem.visible = false;
        this._renderMenu();
        if (this._capsule) {
            this._capsule.resetElapsed();
            this._capsule.hideIndicator();
        }
    }

    _openSettings(page = '') {
        try {
            Main.panel.closeQuickSettings();
            const desktop = Gio.DesktopAppInfo.new('io.github.hy26v.Voxkey.desktop');
            if (!desktop)
                throw new Error('Voxkey settings is not installed');
            if (page) {
                const executable = desktop.get_executable() || 'voxkey-settings';
                const launcher = Gio.AppInfo.create_from_commandline(
                    `${executable} --page=${page}`,
                    'Voxkey',
                    Gio.AppInfoCreateFlags.NONE);
                launcher.launch([], null);
            } else {
                desktop.launch([], null);
            }
            Main.overview.hide();
        } catch (error) {
            Main.notifyError('Voxkey', `Could not launch settings: ${this._cleanError(error)}`);
        }
    }

    destroy() {
        if (this._focusDelayId) {
            GLib.source_remove(this._focusDelayId);
            this._focusDelayId = 0;
        }
        if (this._focusDelayResolve) {
            const settle = this._focusDelayResolve;
            this._focusDelayResolve = null;
            settle();
        }
        if (this._reconnectId) {
            GLib.source_remove(this._reconnectId);
            this._reconnectId = 0;
        }
        this._cancellable.cancel();
        if (this._propertyHandlerId && this._proxy) {
            this._proxy.disconnect(this._propertyHandlerId);
            this._propertyHandlerId = 0;
        }
        if (this._ownerHandlerId && this._proxy) {
            this._proxy.disconnect(this._ownerHandlerId);
            this._ownerHandlerId = 0;
        }
        this._proxy = null;
        if (this._capsule) {
            this._capsule.destroy();
            this._capsule = null;
        }
        super.destroy();
    }
});

export const VoxkeyIndicator = GObject.registerClass(
class VoxkeyIndicator extends SystemIndicator {
    _init() {
        super._init();
        this._toggle = new VoxkeyToggle();
        this.quickSettingsItems.push(this._toggle);
    }

    destroy() {
        this.quickSettingsItems.forEach(item => item.destroy());
        this.quickSettingsItems = [];
        this._toggle = null;
        super.destroy();
    }
});
