//! Directional in-process event feed.
//!
//! Managers emit [`AppEvent`]s instead of reaching directly into every
//! surface; the 100 ms UI tick drains the feed (`MusicApp::drain_events`)
//! and reacts. This is a *one-way, single-consumer* feed, not a broadcast
//! bus: single-owner architecture keeps ordering deterministic and events
//! are always drained before the sync phase, so nothing is lost.

/// Discrete application state changes worth propagating to surfaces (UI, tray).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppEvent {
    /// The current track changed (`None` = stop playing anything).
    TrackChanged(Option<usize>),
    /// Playback started or resumed.
    PlaybackStarted,
    /// Playback paused.
    PlaybackPaused,
    /// Playback stopped or a track finished/queue exhausted.
    PlaybackStopped,
    /// The track set changed: added/removed/cleared/sorted/loaded.
    QueueChanged,
    /// Output volume changed (UI slider or tray wheel).
    VolumeChanged(f32),
    /// Cover art was refreshed for the current track.
    CoverChanged,
    /// Active output device changed.
    DeviceChanged,
}