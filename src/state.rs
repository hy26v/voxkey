// ABOUTME: Daemon state machine with explicit states and transition rules.
// ABOUTME: Prevents race conditions by enforcing valid state transitions only.

use std::fmt;

/// The daemon's operational states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
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
    Deactivated,
    TranscriptReady,
    InjectionDone,
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

            // Recording + Deactivated -> Transcribing
            (State::Recording, Event::Deactivated) => Some(State::Transcribing),

            // Streaming + Deactivated -> Transcribing (draining final results)
            (State::Streaming, Event::Deactivated) => Some(State::Transcribing),

            // Transcribing + TranscriptReady -> Injecting
            (State::Transcribing, Event::TranscriptReady) => Some(State::Injecting),

            // Injecting + InjectionDone -> Idle
            (State::Injecting, Event::InjectionDone) => Some(State::Idle),

            // Transcribing + InjectionDone -> Idle (streaming session signals completion)
            (State::Transcribing, Event::InjectionDone) => Some(State::Idle),

            // Streaming + InjectionDone -> Idle (streaming error before key release)
            (State::Streaming, Event::InjectionDone) => Some(State::Idle),

            // Any + Error -> RecoveringSession
            (_, Event::Error) => Some(State::RecoveringSession),

            // RecoveringSession + Recovered -> Idle
            (State::RecoveringSession, Event::Recovered) => Some(State::Idle),

            // Ignore duplicate Activated while Recording or Streaming
            (State::Recording, Event::Activated) => None,
            (State::Streaming, Event::Activated) => None,

            // Ignore Deactivated while not Recording or Streaming
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
    fn transcribing_injection_done_transitions_to_idle() {
        assert_eq!(
            State::Transcribing.transition(&Event::InjectionDone),
            Some(State::Idle)
        );
    }

    #[test]
    fn streaming_error_transitions_to_recovering() {
        assert_eq!(
            State::Streaming.transition(&Event::Error),
            Some(State::RecoveringSession)
        );
    }

    #[test]
    fn streaming_injection_done_transitions_to_idle() {
        assert_eq!(
            State::Streaming.transition(&Event::InjectionDone),
            Some(State::Idle)
        );
    }
}
