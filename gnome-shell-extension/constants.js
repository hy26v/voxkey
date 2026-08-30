// ABOUTME: Shared D-Bus names, icons, state strings, and timing constants for the Shell UI.
// ABOUTME: Kept in sync with daemon state strings in src/state.rs.

export const BUS_NAME = 'io.github.hy26v.Voxkey.Daemon';
export const OBJECT_PATH = '/io/github/hy26v/Voxkey/Daemon';
export const INTERFACE = 'io.github.hy26v.Voxkey.Daemon1';

export const PROP_STATE = 'State';
export const PROP_PORTAL_CONNECTED = 'PortalConnected';
export const PROP_SHORTCUT_TRIGGER = 'ShortcutTrigger';
export const PROP_SHORTCUT_DESCRIPTION = 'ShortcutDescription';
export const PROP_LIVE_TRANSCRIPT = 'LiveTranscript';
export const PROP_LAST_TRANSCRIPT = 'LastTranscript';
export const PROP_LAST_ERROR = 'LastError';
export const PROP_AUDIO_LEVEL = 'AudioLevel';
export const PROP_AUDIO_INPUT_DEVICE = 'AudioInputDevice';
export const PROP_TRANSCRIBER_CONFIG = 'TranscriberConfig';

export const METHOD_START = 'StartDictation';
export const METHOD_STOP = 'StopDictation';
export const METHOD_CANCEL = 'CancelDictation';
export const METHOD_INSERT_LAST = 'InsertLastTranscript';
export const METHOD_DISMISS_ERROR = 'DismissLastError';

export const ICON_IDLE = 'audio-input-microphone-symbolic';
export const ICON_RECORDING = 'media-record-symbolic';
export const ICON_OFFLINE = 'microphone-sensitivity-muted-symbolic';
export const ICON_ERROR = 'dialog-error-symbolic';

/// State strings exposed by the daemon, kept in sync with src/state.rs.
export const STATE_IDLE = 'Idle';
export const STATE_CONNECTING = 'Connecting';
export const STATE_RECORDING = 'Recording';
export const STATE_STREAMING = 'Streaming';
export const STATE_TRANSCRIBING = 'Transcribing';
export const STATE_INJECTING = 'Injecting';
export const STATE_RECOVERING = 'RecoveringSession';

export const INDICATOR_WIDTH = 560;
export const INDICATOR_HEIGHT = 142;
export const INDICATOR_SIDE_MARGIN = 16;
export const INDICATOR_BOTTOM_MARGIN = 24;
export const INDICATOR_DASH_GAP = 12;
export const TRANSCRIPT_MAX_CHARS = 420;
export const TRANSCRIPT_MENU_MAX_CHARS = 72;
export const MENU_TEXT_MAX_CHARS = 96;
export const INDICATOR_SHOW_DURATION_MS = 180;
export const INDICATOR_HIDE_DURATION_MS = 140;
export const PROCESSING_INTERVAL_MS = 150;
export const ELAPSED_INTERVAL_MS = 250;
export const AUDIO_HISTORY_LENGTH = 7;
// Quick Settings takes 400 ms to close on current GNOME releases. Waiting one
// frame beyond that gives focus back to the user's application before the
// daemon starts synthesizing keystrokes.
export const QUICK_SETTINGS_FOCUS_DELAY_MS = 450;
// The daemon gives acknowledged controls ten seconds. A slightly longer
// client deadline lets its actionable error arrive first, while ensuring a
// wedged service can never leave every Shell control disabled indefinitely.
export const CONTROL_CALL_TIMEOUT_MS = 12000;

export const ACTIVE_STATES = new Set([
    STATE_CONNECTING,
    STATE_RECORDING,
    STATE_STREAMING,
]);
// The floating control belongs only to the capture lifecycle. Insertion and
// errors remain visible through the focused application and Quick Settings,
// respectively, instead of keeping a post-dictation overlay on screen.
export const INDICATOR_STATES = new Set([
    STATE_CONNECTING,
    STATE_RECORDING,
    STATE_STREAMING,
    STATE_TRANSCRIBING,
]);
export const BUSY_STATES = new Set([
    STATE_CONNECTING,
    STATE_RECORDING,
    STATE_STREAMING,
    STATE_TRANSCRIBING,
    STATE_INJECTING,
]);
export const CANCELLABLE_STATES = new Set([
    STATE_CONNECTING,
    STATE_RECORDING,
    STATE_STREAMING,
    STATE_TRANSCRIBING,
]);
