// ABOUTME: Daemon state machine with explicit states and transition rules.
// ABOUTME: Prevents race conditions by enforcing valid state transitions only.

use std::fmt;

/// The daemon's operational states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Connecting,
    Recording,
    Streaming,
    Transcribing,
    Injecting,
    RecoveringSession,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            State::Idle => write!(f, "Idle"),
            State::Connecting => write!(f, "Connecting"),
            State::Recording => write!(f, "Recording"),
            State::Streaming => write!(f, "Streaming"),
            State::Transcribing => write!(f, "Transcribing"),
            State::Injecting => write!(f, "Injecting"),
            State::RecoveringSession => write!(f, "RecoveringSession"),
        }
    }
}

/// Events that trigger state transitions.
#[derive(Debug)]
#[allow(dead_code)]
pub enum Event {
    Activated,
    StreamingReady,
    Deactivated,
    TranscriptReady,
    InjectionDone,
    StreamingDone,
    BatchCaptureFailed {
        transcript_generation: u64,
        message: String,
    },
    Error,
    Recovered,
}

impl State {
    /// Attempt a state transition. Returns the new state if the transition
    /// is valid, or None if the event should be ignored in the current state.
    pub fn transition(self, event: &Event) -> Option<State> {
        match (self, event) {
            // Idle + Activated -> Recording
            (State::Idle, Event::Activated) => Some(State::Recording),

            (State::Connecting, Event::StreamingReady) => Some(State::Streaming),

            // Recording + Deactivated -> Transcribing
            (State::Recording, Event::Deactivated) => Some(State::Transcribing),

            // Streaming + Deactivated -> Transcribing (draining final results)
            (State::Streaming, Event::Deactivated) => Some(State::Transcribing),
            // A stop request can arrive while the provider handshake is still
            // in flight. The session will drain as soon as it is ready.
            (State::Connecting, Event::Deactivated) => Some(State::Transcribing),

            // Transcribing + TranscriptReady -> Injecting
            (State::Transcribing, Event::TranscriptReady) => Some(State::Injecting),

            // Injecting + InjectionDone -> Idle
            (State::Injecting, Event::InjectionDone) => Some(State::Idle),

            // A streaming session may finish while draining after key release.
            (State::Transcribing, Event::StreamingDone) => Some(State::Idle),

            // Or it may finish/error before the shortcut is pressed again.
            (State::Streaming, Event::StreamingDone) => Some(State::Idle),
            (State::Connecting, Event::StreamingDone) => Some(State::Idle),

            // Any + Error -> RecoveringSession
            (_, Event::Error) => Some(State::RecoveringSession),

            // RecoveringSession + Recovered -> Idle
            (State::RecoveringSession, Event::Recovered) => Some(State::Idle),

            // Ignore duplicate Activated while Recording or Streaming
            (State::Recording, Event::Activated) => None,
            (State::Streaming, Event::Activated) => None,
            (State::Connecting, Event::Activated) => None,

            // Ignore Deactivated when no capture can be stopped.
            (State::Idle, Event::Deactivated) => None,
            (State::Transcribing, Event::Deactivated) => None,
            (State::Injecting, Event::Deactivated) => None,
            (State::RecoveringSession, Event::Deactivated) => None,

            // Allow new recording while injection is ongoing (queue handles serialization)
            (State::Injecting, Event::Activated) => Some(State::Recording),

            // Ignore Activated during Transcribing or RecoveringSession
            (State::Transcribing, Event::Activated) => None,
            (State::RecoveringSession, Event::Activated) => None,

            // Ignore other combinations
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_activated_is_ignored() {
        assert_eq!(State::Streaming.transition(&Event::Activated), None);
    }

    #[test]
    fn streaming_deactivated_transitions_to_transcribing() {
        assert_eq!(
            State::Streaming.transition(&Event::Deactivated),
            Some(State::Transcribing)
        );
    }

    #[test]
    fn connecting_becomes_streaming_only_after_provider_setup() {
        assert_eq!(
            State::Connecting.transition(&Event::StreamingReady),
            Some(State::Streaming)
        );
    }

    #[test]
    fn connecting_can_be_stopped_while_provider_setup_is_in_flight() {
        assert_eq!(
            State::Connecting.transition(&Event::Deactivated),
            Some(State::Transcribing)
        );
    }

    #[test]
    fn transcribing_streaming_done_transitions_to_idle() {
        assert_eq!(
            State::Transcribing.transition(&Event::StreamingDone),
            Some(State::Idle)
        );
    }

    #[test]
    fn a_previous_injection_cannot_finish_the_current_transcription() {
        // InjectionDone belongs to an earlier queued transcript. The current
        // recording has already stopped and is still being transcribed.
        assert_eq!(State::Transcribing.transition(&Event::InjectionDone), None);
    }

    #[test]
    fn streaming_error_transitions_to_recovering() {
        assert_eq!(
            State::Streaming.transition(&Event::Error),
            Some(State::RecoveringSession)
        );
    }

    #[test]
    fn streaming_done_transitions_to_idle() {
        assert_eq!(
            State::Streaming.transition(&Event::StreamingDone),
            Some(State::Idle)
        );
    }
}
