use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::library::track::Track;
use crate::library::track::TrackId;
use crate::library::track::TrackPath;
use crate::player::queue::Queue;

pub mod queue;

#[cfg(feature = "ui")]
pub mod rodio;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PlayerError {
    #[error("volume must be between 0.0 and 1.0, got {0}")]
    VolumeOutOfRange(f32),
}

/// Playback volume. Always in [0.0, 1.0].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Volume(f32);

impl Volume {
    /// Returns `Err(VolumeOutOfRange)` if `v` is outside [0.0, 1.0].
    pub fn new(v: f32) -> Result<Self, PlayerError> {
        if !(0.0..=1.0).contains(&v) {
            return Err(PlayerError::VolumeOutOfRange(v));
        }
        Ok(Self(v))
    }

    pub fn value(self) -> f32 {
        self.0
    }

    pub fn silent() -> Self {
        Self(0.0)
    }

    pub fn full() -> Self {
        Self(1.0)
    }
}

/// Playback position within a track. Always non-negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SeekPosition(std::time::Duration);

impl SeekPosition {
    pub fn from_secs(secs: u64) -> Self {
        Self(std::time::Duration::from_secs(secs))
    }

    pub fn from_millis(millis: u64) -> Self {
        Self(std::time::Duration::from_millis(millis))
    }

    pub fn as_duration(self) -> std::time::Duration {
        self.0
    }

    pub fn as_secs(self) -> u64 {
        self.0.as_secs()
    }

    pub fn zero() -> Self {
        Self(std::time::Duration::ZERO)
    }
}

/// Current state of the audio engine.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackState {
    Stopped,
    Playing {
        track: TrackId,
        position: SeekPosition,
    },
    Paused {
        track: TrackId,
        position: SeekPosition,
    },
    /// The backend failed to open or decode the track (e.g. missing file, corrupt data).
    Failed {
        track: TrackId,
        error: String,
    },
}

impl PlaybackState {
    pub fn is_stopped(&self) -> bool {
        matches!(self, Self::Stopped)
    }

    pub fn is_playing(&self) -> bool {
        matches!(self, Self::Playing { .. })
    }

    pub fn current_track(&self) -> Option<TrackId> {
        match self {
            Self::Stopped => None,
            Self::Playing { track, .. } => Some(*track),
            Self::Paused { track, .. } => Some(*track),
            Self::Failed { track, .. } => Some(*track),
        }
    }

    /// The playback position, or `None` when stopped or failed.
    pub fn position(&self) -> Option<SeekPosition> {
        match self {
            Self::Stopped => None,
            Self::Playing { position, .. } => Some(*position),
            Self::Paused { position, .. } => Some(*position),
            Self::Failed { .. } => None,
        }
    }
}

/// Commands sent to the audio engine thread.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerCommand {
    // Boxed: `Track` is large, and an unboxed variant would bloat every command.
    Play(Box<Track>),
    /// Replaces the queue with `tracks` positioned at `start` and plays it. This
    /// is what `Next`/`Previous` and auto-advance then navigate through.
    PlayQueue {
        tracks: Vec<Track>,
        start: usize,
    },
    /// Appends `tracks` to the end of the current queue without disturbing
    /// what's already playing. If the queue was empty, playback starts from
    /// the first appended track.
    Enqueue(Vec<Track>),
    /// Restores `tracks` at `start`, loaded paused at `position` — used on
    /// startup to reopen where the previous session left off, without resuming.
    RestorePaused {
        tracks: Vec<Track>,
        start: usize,
        position: SeekPosition,
    },
    /// Replaces the queue's track list without disturbing playback: if the
    /// current track is present in `tracks`, its position resumes unchanged;
    /// otherwise playback stops. Used to reconcile the queue after a library
    /// change removes some of its tracks — the player is the sole owner of
    /// the queue, so the UI cannot just drop entries from its own copy.
    SetQueue(Vec<Track>),
    Pause,
    Resume,
    Stop,
    Seek(SeekPosition),
    SetVolume(Volume),
    Next,
    Previous,
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("failed to open audio output: {0}")]
    Device(String),
    #[error("failed to decode {0}: {1}")]
    Decode(String, String),
}

/// What the audio hardware is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    Playing,
    Paused,
    /// No source loaded, or the loaded source finished on its own. Also the
    /// state of a backend on which `resume()` was called with nothing to
    /// resume — checked before reporting `Playing`, so a resume after a
    /// decode failure never claims audio that isn't there.
    Idle,
}

/// Drives the underlying audio hardware. Implemented by `RodioAudioBackend`.
///
/// Intentionally not `Send` — implementations may hold OS audio handles tied
/// to the thread that created them. The player thread creates its own instance.
pub trait AudioBackend {
    fn play(&mut self, path: &TrackPath) -> Result<(), AudioError>;
    /// Loads `path` and holds it paused at `position`, with no audible playback,
    /// so a restored session reopens where it left off without resuming.
    fn play_paused(&mut self, path: &TrackPath, position: Duration) -> Result<(), AudioError>;
    /// Moves the play head of the current track to `position`.
    fn seek(&mut self, position: Duration);
    fn pause(&mut self);
    fn resume(&mut self);
    fn stop(&mut self);
    fn set_volume(&mut self, volume: Volume);
    fn state(&self) -> BackendState;
    fn position(&self) -> Duration;
}

/// Cloneable handle to the background player thread.
///
/// Each clone shares the same command channel — any clone can send commands.
#[derive(Clone)]
pub struct PlayerHandle {
    command_tx: Sender<PlayerCommand>,
}

impl PlayerHandle {
    /// Spawns a background thread, creates the backend with `make_backend`,
    /// and runs the player loop. `on_state` is called from that thread on
    /// every state transition and on each 250 ms position tick while playing.
    /// `on_queue_changed` is called whenever the queue's track list itself
    /// changes (not on cursor-only moves like `Next`/`Previous`) — the player
    /// owns the queue, so this is the only way a caller learns its contents.
    pub fn launch<B, F, G>(
        make_backend: impl FnOnce() -> Result<B, AudioError> + Send + 'static,
        on_state: F,
        on_queue_changed: G,
    ) -> Self
    where
        B: AudioBackend + 'static,
        F: Fn(PlaybackState) + Send + 'static,
        G: Fn(Vec<Track>) + Send + 'static,
    {
        let (command_tx, command_rx) = mpsc::channel();
        std::thread::spawn(move || match make_backend() {
            Ok(mut backend) => player_loop(&mut backend, command_rx, on_state, on_queue_changed),
            Err(e) => tracing::error!("audio backend init failed: {e}"),
        });
        Self { command_tx }
    }

    pub fn send(&self, cmd: PlayerCommand) {
        let _ = self.command_tx.send(cmd);
    }
}

/// What the player loop believes it has asked the backend to do.
///
/// This is the loop's own record, deliberately *not* re-read from
/// `backend.state()` after issuing a command. Sampling the backend right after
/// `play()` returns races with the track ending in that same window: the sample
/// comes back `Idle`, the loop concludes it has nothing to tick for, and then
/// blocks in `recv()` forever — never noticing the track ended, so auto-advance
/// never happens. Trusting the command we just issued has no such window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopState {
    /// Nothing is loaded. The loop blocks until a command arrives.
    Empty,
    /// A source is loaded and playing. The loop runs its 250 ms position tick.
    Playing,
    /// A source is loaded but paused. No tick, but `Resume` has something to
    /// resume and `Seek` has a position to report.
    Paused,
}

/// Starts `track` on the backend. Emits `Playing` on success or `Failed` on error.
fn play_track<B: AudioBackend, F: Fn(PlaybackState)>(
    backend: &mut B,
    track: &Track,
    on_state: &F,
) -> LoopState {
    match backend.play(&track.path) {
        Ok(()) => {
            on_state(PlaybackState::Playing {
                track: track.id,
                position: SeekPosition::zero(),
            });
            LoopState::Playing
        }
        Err(e) => {
            on_state(PlaybackState::Failed {
                track: track.id,
                error: e.to_string(),
            });
            LoopState::Empty
        }
    }
}

/// Plays the queue's current track. Returns `Empty` when the queue has nothing
/// to play or the backend failed to open the track.
fn play_current<B: AudioBackend, F: Fn(PlaybackState)>(
    backend: &mut B,
    queue: &Queue,
    on_state: &F,
) -> LoopState {
    match queue.current() {
        Some(track) => play_track(backend, track, on_state),
        None => LoopState::Empty,
    }
}

/// Loads `track` paused at `position`. Emits `Paused` on success or `Failed` on error.
fn restore_paused_track<B: AudioBackend, F: Fn(PlaybackState)>(
    backend: &mut B,
    track: &Track,
    position: SeekPosition,
    on_state: &F,
) -> LoopState {
    match backend.play_paused(&track.path, position.as_duration()) {
        Ok(()) => {
            on_state(PlaybackState::Paused {
                track: track.id,
                position,
            });
            LoopState::Paused
        }
        Err(e) => {
            on_state(PlaybackState::Failed {
                track: track.id,
                error: e.to_string(),
            });
            LoopState::Empty
        }
    }
}

/// Silences the backend, clears the queue, and reports both. Shared by `Stop`,
/// by reaching the end of the queue, and by `SetQueue` pruning the playing track.
fn stop_playback<B: AudioBackend, F: Fn(PlaybackState), G: Fn(Vec<Track>)>(
    backend: &mut B,
    queue: &mut Queue,
    on_state: &F,
    on_queue_changed: &G,
) -> LoopState {
    backend.stop();
    *queue = Queue::empty();
    on_queue_changed(Vec::new());
    on_state(PlaybackState::Stopped);
    LoopState::Empty
}

/// Handles one 250 ms tick while playing: report the current position, or
/// notice the track ended on its own and advance to the next one.
///
/// Unlike the play paths, querying `backend.state()` here is the whole point —
/// the tick exists to observe what the hardware did on its own, and a stale
/// reading merely defers the decision to the next tick 250 ms later.
fn tick<B: AudioBackend, F: Fn(PlaybackState), G: Fn(Vec<Track>)>(
    backend: &mut B,
    queue: &mut Queue,
    on_state: &F,
    on_queue_changed: &G,
) -> LoopState {
    let Some(current_id) = queue.current().map(|t| t.id) else {
        return LoopState::Empty;
    };
    match backend.state() {
        BackendState::Playing => {
            on_state(PlaybackState::Playing {
                track: current_id,
                position: SeekPosition::from_millis(backend.position().as_millis() as u64),
            });
            LoopState::Playing
        }
        // Still paused since the last tick — nothing to report.
        BackendState::Paused => LoopState::Paused,
        BackendState::Idle => {
            if queue.advance().is_some() {
                play_current(backend, queue, on_state)
            } else {
                stop_playback(backend, queue, on_state, on_queue_changed)
            }
        }
    }
}

fn player_loop<B: AudioBackend, F: Fn(PlaybackState), G: Fn(Vec<Track>)>(
    backend: &mut B,
    command_rx: Receiver<PlayerCommand>,
    on_state: F,
    on_queue_changed: G,
) {
    let mut queue = Queue::empty();
    let mut loop_state = LoopState::Empty;

    loop {
        let cmd_opt = match loop_state {
            // Playing: wake every 250 ms to report the position and to notice a
            // track that ended on its own.
            LoopState::Playing => match command_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(cmd) => Some(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            // Nothing is playing, so nothing can end on its own: block until
            // the next command rather than spinning.
            LoopState::Empty | LoopState::Paused => match command_rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            },
        };

        match cmd_opt {
            None => {
                loop_state = tick(backend, &mut queue, &on_state, &on_queue_changed);
            }
            Some(PlayerCommand::Play(track)) => {
                queue = Queue::single(*track);
                on_queue_changed(queue.tracks().to_vec());
                loop_state = play_current(backend, &queue, &on_state);
            }
            Some(PlayerCommand::PlayQueue { tracks, start }) => {
                queue = Queue::new(tracks, start);
                on_queue_changed(queue.tracks().to_vec());
                loop_state = play_current(backend, &queue, &on_state);
            }
            Some(PlayerCommand::Enqueue(tracks)) => {
                let was_empty = queue.is_empty();
                queue.append(tracks);
                on_queue_changed(queue.tracks().to_vec());
                if was_empty {
                    loop_state = play_current(backend, &queue, &on_state);
                }
            }
            Some(PlayerCommand::RestorePaused {
                tracks,
                start,
                position,
            }) => {
                queue = Queue::new(tracks, start);
                on_queue_changed(queue.tracks().to_vec());
                loop_state = match queue.current() {
                    Some(track) => restore_paused_track(backend, track, position, &on_state),
                    None => LoopState::Empty,
                };
            }
            Some(PlayerCommand::SetQueue(tracks)) => {
                let current_id = queue.current().map(|t| t.id);
                let survives = current_id.is_some_and(|id| tracks.iter().any(|t| t.id == id));
                if current_id.is_some() && !survives {
                    // The playing/paused track was pruned out from under us —
                    // the safe, unsurprising behaviour is to stop rather than
                    // silently continue on (or jump to) a track the user
                    // didn't choose.
                    loop_state = stop_playback(backend, &mut queue, &on_state, &on_queue_changed);
                } else {
                    let start = current_id
                        .and_then(|id| tracks.iter().position(|t| t.id == id))
                        .unwrap_or(0);
                    queue = Queue::new(tracks, start);
                    on_queue_changed(queue.tracks().to_vec());
                }
            }
            Some(PlayerCommand::Next) => {
                if queue.advance().is_some() {
                    loop_state = play_current(backend, &queue, &on_state);
                }
            }
            Some(PlayerCommand::Previous) => {
                if queue.rewind().is_some() {
                    loop_state = play_current(backend, &queue, &on_state);
                }
            }
            Some(PlayerCommand::Pause) => {
                backend.pause();
                loop_state = match loop_state {
                    // Nothing is loaded — after a decode failure, say — so
                    // there is nothing to pause and nothing to report.
                    LoopState::Empty => LoopState::Empty,
                    LoopState::Playing | LoopState::Paused => LoopState::Paused,
                };
                if loop_state == LoopState::Paused
                    && let Some(t) = queue.current()
                {
                    on_state(PlaybackState::Paused {
                        track: t.id,
                        position: SeekPosition::from_millis(backend.position().as_millis() as u64),
                    });
                }
            }
            Some(PlayerCommand::Resume) => match loop_state {
                // Resuming with no loaded source (right after a decode failure,
                // say) must not claim audio that isn't there.
                LoopState::Empty => {}
                LoopState::Playing | LoopState::Paused => {
                    backend.resume();
                    loop_state = LoopState::Playing;
                    if let Some(t) = queue.current() {
                        on_state(PlaybackState::Playing {
                            track: t.id,
                            position: SeekPosition::from_millis(
                                backend.position().as_millis() as u64
                            ),
                        });
                    }
                }
            },
            Some(PlayerCommand::Stop) => {
                loop_state = stop_playback(backend, &mut queue, &on_state, &on_queue_changed);
            }
            Some(PlayerCommand::SetVolume(v)) => {
                backend.set_volume(v);
            }
            Some(PlayerCommand::Seek(position)) => {
                backend.seek(position.as_duration());
                // Report the new position immediately, keeping the play/pause
                // state, rather than waiting for the next tick. With nothing
                // loaded there is no position to report.
                let state = match loop_state {
                    LoopState::Empty => None,
                    LoopState::Playing => queue.current().map(|t| PlaybackState::Playing {
                        track: t.id,
                        position,
                    }),
                    LoopState::Paused => queue.current().map(|t| PlaybackState::Paused {
                        track: t.id,
                        position,
                    }),
                };
                if let Some(state) = state {
                    on_state(state);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    use super::*;
    use crate::library::track::AlbumTitle;
    use crate::library::track::Artist;
    use crate::library::track::Composer;
    use crate::library::track::DiscNumber;
    use crate::library::track::Genre;
    use crate::library::track::Title;
    use crate::library::track::Track;
    use crate::library::track::TrackDuration;
    use crate::library::track::TrackId;
    use crate::library::track::TrackNumber;
    use crate::library::track::TrackPath;
    use crate::library::track::Year;

    #[test]
    fn volume_new_accepts_zero() {
        assert_eq!(Volume::new(0.0).unwrap().value(), 0.0);
    }

    #[test]
    fn volume_new_accepts_one() {
        assert_eq!(Volume::new(1.0).unwrap().value(), 1.0);
    }

    #[test]
    fn volume_new_accepts_midpoint() {
        assert_eq!(Volume::new(0.5).unwrap().value(), 0.5);
    }

    #[test]
    fn volume_new_rejects_value_above_one() {
        assert!(matches!(
            Volume::new(1.1),
            Err(PlayerError::VolumeOutOfRange(_))
        ));
    }

    #[test]
    fn volume_new_rejects_negative_value() {
        assert!(matches!(
            Volume::new(-0.1),
            Err(PlayerError::VolumeOutOfRange(_))
        ));
    }

    #[test]
    fn volume_silent_is_zero() {
        assert_eq!(Volume::silent().value(), 0.0);
    }

    #[test]
    fn volume_full_is_one() {
        assert_eq!(Volume::full().value(), 1.0);
    }

    #[test]
    fn seek_position_from_secs_round_trips() {
        assert_eq!(SeekPosition::from_secs(90).as_secs(), 90);
    }

    #[test]
    fn seek_position_ordering_reflects_time() {
        assert!(SeekPosition::from_secs(10) < SeekPosition::from_secs(60));
    }

    #[test]
    fn playback_state_stopped_is_stopped() {
        assert!(PlaybackState::Stopped.is_stopped());
    }

    #[test]
    fn playback_state_playing_is_not_stopped() {
        let state = PlaybackState::Playing {
            track: TrackId::new(1),
            position: SeekPosition::zero(),
        };
        assert!(!state.is_stopped());
        assert!(state.is_playing());
    }

    #[test]
    fn playback_state_stopped_has_no_current_track() {
        assert_eq!(PlaybackState::Stopped.current_track(), None);
    }

    #[test]
    fn playback_state_playing_exposes_current_track() {
        let id = TrackId::new(7);
        let state = PlaybackState::Playing {
            track: id,
            position: SeekPosition::zero(),
        };
        assert_eq!(state.current_track(), Some(id));
    }

    #[test]
    fn playback_state_paused_exposes_current_track() {
        let id = TrackId::new(3);
        let state = PlaybackState::Paused {
            track: id,
            position: SeekPosition::from_secs(42),
        };
        assert_eq!(state.current_track(), Some(id));
    }

    struct MockAudioBackend {
        state: BackendState,
        volume: f32,
        position: Duration,
    }

    impl MockAudioBackend {
        fn new() -> Self {
            Self {
                state: BackendState::Idle,
                volume: 1.0,
                position: Duration::ZERO,
            }
        }
    }

    impl AudioBackend for MockAudioBackend {
        fn play(&mut self, _path: &TrackPath) -> Result<(), AudioError> {
            self.state = BackendState::Playing;
            self.position = Duration::ZERO;
            Ok(())
        }
        fn play_paused(
            &mut self,
            _path: &TrackPath,
            _position: Duration,
        ) -> Result<(), AudioError> {
            self.state = BackendState::Paused;
            Ok(())
        }
        fn pause(&mut self) {
            self.state = BackendState::Paused;
        }
        fn resume(&mut self) {
            self.state = BackendState::Playing;
        }
        fn stop(&mut self) {
            self.state = BackendState::Idle;
        }
        fn set_volume(&mut self, v: Volume) {
            self.volume = v.value();
        }
        fn state(&self) -> BackendState {
            self.state
        }
        fn seek(&mut self, position: Duration) {
            self.position = position;
        }
        fn position(&self) -> Duration {
            self.position
        }
    }

    fn julie_and_candy() -> Track {
        Track {
            id: TrackId::new(1),
            path: TrackPath::new("/music/geogaddi/julie_and_candy.flac").unwrap(),
            title: Title::new("Julie and Candy"),
            artist: Artist::new("Boards of Canada"),
            album_artist: Artist::new("Boards of Canada"),
            album: AlbumTitle::new("Geogaddi"),
            genre: Genre::new("Electronic"),
            composer: Composer::new(""),
            duration: TrackDuration::from_secs(232),
            track_number: TrackNumber::new(2),
            disc_number: DiscNumber::new(1),
            year: Year::new(2002),
        }
    }

    fn launch_with_channel() -> (PlayerHandle, mpsc::Receiver<PlaybackState>) {
        let (handle, rx, _queue_rx) = launch_with_channels();
        (handle, rx)
    }

    /// Like `launch_with_channel`, but also returns the queue-snapshot receiver
    /// for tests that care about `on_queue_changed`.
    fn launch_with_channels() -> (
        PlayerHandle,
        mpsc::Receiver<PlaybackState>,
        mpsc::Receiver<Vec<Track>>,
    ) {
        let (tx, rx) = mpsc::channel();
        let (queue_tx, queue_rx) = mpsc::channel();
        let handle = PlayerHandle::launch(
            || Ok(MockAudioBackend::new()),
            move |s| {
                let _ = tx.send(s);
            },
            move |tracks| {
                let _ = queue_tx.send(tracks);
            },
        );
        (handle, rx, queue_rx)
    }

    /// Drains `rx` until a message matching `pred` arrives (or 3 s elapses).
    /// Intermediate gaps are tolerated: the player emits only every 250 ms while
    /// playing, and auto-advance lands a full tick after a track ends.
    fn recv_matching(
        rx: &mpsc::Receiver<PlaybackState>,
        pred: impl Fn(&PlaybackState) -> bool,
    ) -> PlaybackState {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(s) if pred(&s) => return s,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Without this arm a dead player thread would spin this loop at
                // full tilt until the deadline, stealing a core from the rest
                // of the suite on exactly the contended machines that need it.
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("player thread ended before the expected playback state arrived")
                }
            }
            if Instant::now() > deadline {
                panic!("deadline exceeded waiting for expected playback state");
            }
        }
    }

    #[test]
    fn player_play_command_transitions_to_playing_state() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        let s = recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        assert!(matches!(s, PlaybackState::Playing { .. }));
    }

    #[test]
    fn player_play_command_reports_correct_track_id() {
        let track = julie_and_candy();
        let expected = track.id;
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Play(Box::new(track)));
        let s = recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        assert_eq!(s.current_track(), Some(expected));
    }

    #[test]
    fn player_pause_after_play_transitions_to_paused_state() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        handle.send(PlayerCommand::Pause);
        let s = recv_matching(&rx, |s| matches!(s, PlaybackState::Paused { .. }));
        assert!(matches!(s, PlaybackState::Paused { .. }));
    }

    #[test]
    fn player_resume_after_pause_transitions_to_playing_state() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        handle.send(PlayerCommand::Pause);
        recv_matching(&rx, |s| matches!(s, PlaybackState::Paused { .. }));
        handle.send(PlayerCommand::Resume);
        let s = recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        assert!(matches!(s, PlaybackState::Playing { .. }));
    }

    #[test]
    fn player_stop_after_play_transitions_to_stopped_state() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        handle.send(PlayerCommand::Stop);
        let s = recv_matching(&rx, |s| s == &PlaybackState::Stopped);
        assert_eq!(s, PlaybackState::Stopped);
    }

    #[test]
    fn player_stop_clears_current_track() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        handle.send(PlayerCommand::Stop);
        let s = recv_matching(&rx, |s| s == &PlaybackState::Stopped);
        assert_eq!(s.current_track(), None);
    }

    fn geogaddi(id: i64, title: &str) -> Track {
        Track {
            id: TrackId::new(id),
            path: TrackPath::new(format!("/music/geogaddi/{id:02}.flac")).unwrap(),
            title: Title::new(title),
            artist: Artist::new("Boards of Canada"),
            album_artist: Artist::new("Boards of Canada"),
            album: AlbumTitle::new("Geogaddi"),
            genre: Genre::new("Electronic"),
            composer: Composer::new(""),
            duration: TrackDuration::from_secs(200),
            track_number: TrackNumber::new(id as u32),
            disc_number: DiscNumber::new(1),
            year: Year::new(2002),
        }
    }

    fn geogaddi_pair() -> Vec<Track> {
        vec![geogaddi(10, "Dawn Chorus"), geogaddi(20, "1969")]
    }

    #[test]
    fn player_play_queue_plays_the_starting_track() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::PlayQueue {
            tracks: geogaddi_pair(),
            start: 1,
        });
        let s = recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(20)));
        assert_eq!(s.current_track(), Some(TrackId::new(20)));
    }

    #[test]
    fn player_next_plays_the_following_track() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::PlayQueue {
            tracks: geogaddi_pair(),
            start: 0,
        });
        recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(10)));
        handle.send(PlayerCommand::Next);
        let s = recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(20)));
        assert_eq!(s.current_track(), Some(TrackId::new(20)));
    }

    #[test]
    fn player_previous_plays_the_prior_track() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::PlayQueue {
            tracks: geogaddi_pair(),
            start: 1,
        });
        recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(20)));
        handle.send(PlayerCommand::Previous);
        let s = recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(10)));
        assert_eq!(s.current_track(), Some(TrackId::new(10)));
    }

    #[test]
    fn player_enqueue_on_empty_queue_starts_playing_the_first_track() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Enqueue(geogaddi_pair()));
        let s = recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(10)));
        assert_eq!(s.current_track(), Some(TrackId::new(10)));
    }

    #[test]
    fn player_enqueue_while_playing_does_not_disturb_the_current_track() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        handle.send(PlayerCommand::Enqueue(geogaddi_pair()));
        let s = recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        assert_eq!(s.current_track(), Some(julie_and_candy().id));
    }

    #[test]
    fn player_next_reaches_a_track_appended_via_enqueue() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        handle.send(PlayerCommand::Enqueue(geogaddi_pair()));
        handle.send(PlayerCommand::Next);
        let s = recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(10)));
        assert_eq!(s.current_track(), Some(TrackId::new(10)));
    }

    /// A backend whose playing state a test can flip to simulate a track ending.
    struct FlaggedBackend {
        playing: Arc<AtomicBool>,
    }

    impl AudioBackend for FlaggedBackend {
        fn play(&mut self, _path: &TrackPath) -> Result<(), AudioError> {
            self.playing.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn play_paused(
            &mut self,
            _path: &TrackPath,
            _position: Duration,
        ) -> Result<(), AudioError> {
            self.playing.store(false, Ordering::SeqCst);
            Ok(())
        }
        fn pause(&mut self) {
            self.playing.store(false, Ordering::SeqCst);
        }
        fn resume(&mut self) {
            self.playing.store(true, Ordering::SeqCst);
        }
        fn stop(&mut self) {
            self.playing.store(false, Ordering::SeqCst);
        }
        fn set_volume(&mut self, _v: Volume) {}
        fn seek(&mut self, _position: Duration) {}
        fn state(&self) -> BackendState {
            if self.playing.load(Ordering::SeqCst) {
                BackendState::Playing
            } else {
                BackendState::Idle
            }
        }
        fn position(&self) -> Duration {
            Duration::ZERO
        }
    }

    /// Launches a player over a `FlaggedBackend`; the returned flag lets the test
    /// simulate the current track finishing by storing `false`.
    fn launch_flagged() -> (PlayerHandle, mpsc::Receiver<PlaybackState>, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        let backend_flag = Arc::clone(&flag);
        let (tx, rx) = mpsc::channel();
        let handle = PlayerHandle::launch(
            move || {
                Ok(FlaggedBackend {
                    playing: backend_flag,
                })
            },
            move |s| {
                let _ = tx.send(s);
            },
            |_tracks| {},
        );
        (handle, rx, flag)
    }

    #[test]
    fn player_auto_advances_when_the_track_ends() {
        let (handle, rx, flag) = launch_flagged();
        handle.send(PlayerCommand::PlayQueue {
            tracks: geogaddi_pair(),
            start: 0,
        });
        recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(10)));

        // The first track finishes on its own.
        flag.store(false, Ordering::SeqCst);

        let s = recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(20)));
        assert_eq!(s.current_track(), Some(TrackId::new(20)));
    }

    /// A backend that accepts `play()` but never reports `Playing`, modelling a
    /// track that finishes in the window between `play()` returning and the
    /// player loop next looking at the backend.
    ///
    /// This is the deterministic form of the scheduling race that made
    /// `player_auto_advances_when_the_track_ends` flaky under load: a loop that
    /// decides whether to keep ticking by re-sampling `backend.state()` right
    /// after `play()` sees `Idle`, stops ticking, and blocks forever.
    struct InstantlyIdleBackend;

    impl AudioBackend for InstantlyIdleBackend {
        fn play(&mut self, _path: &TrackPath) -> Result<(), AudioError> {
            Ok(())
        }
        fn play_paused(
            &mut self,
            _path: &TrackPath,
            _position: Duration,
        ) -> Result<(), AudioError> {
            Ok(())
        }
        fn pause(&mut self) {}
        fn resume(&mut self) {}
        fn stop(&mut self) {}
        fn set_volume(&mut self, _v: Volume) {}
        fn seek(&mut self, _position: Duration) {}
        fn state(&self) -> BackendState {
            BackendState::Idle
        }
        fn position(&self) -> Duration {
            Duration::ZERO
        }
    }

    #[test]
    fn player_auto_advances_when_the_track_ends_before_the_backend_is_polled() {
        let (tx, rx) = mpsc::channel();
        let handle = PlayerHandle::launch(
            || Ok(InstantlyIdleBackend),
            move |s| {
                let _ = tx.send(s);
            },
            |_tracks| {},
        );
        handle.send(PlayerCommand::PlayQueue {
            tracks: geogaddi_pair(),
            start: 0,
        });
        recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(10)));

        // The loop must keep ticking on the strength of the play command it
        // issued, not on a backend reading taken after the track already ended.
        let s = recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(20)));
        assert_eq!(s.current_track(), Some(TrackId::new(20)));
    }

    #[test]
    fn player_stops_after_the_last_track_ends() {
        let (handle, rx, flag) = launch_flagged();
        handle.send(PlayerCommand::PlayQueue {
            tracks: vec![geogaddi(10, "Dawn Chorus")],
            start: 0,
        });
        recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(10)));

        // The only track finishes: no next, so playback stops.
        flag.store(false, Ordering::SeqCst);

        let s = recv_matching(&rx, |s| s == &PlaybackState::Stopped);
        assert_eq!(s, PlaybackState::Stopped);
    }

    #[test]
    fn player_restore_paused_reports_paused_at_position() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::RestorePaused {
            tracks: geogaddi_pair(),
            start: 1,
            position: SeekPosition::from_secs(87),
        });
        let s = recv_matching(&rx, |s| matches!(s, PlaybackState::Paused { .. }));
        assert_eq!(s.current_track(), Some(TrackId::new(20)));
        assert_eq!(s.position(), Some(SeekPosition::from_secs(87)));
    }

    #[test]
    fn player_seek_reports_the_new_position_while_playing() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));

        handle.send(PlayerCommand::Seek(SeekPosition::from_secs(90)));

        let s = recv_matching(&rx, |s| s.position() == Some(SeekPosition::from_secs(90)));
        assert!(matches!(s, PlaybackState::Playing { .. }));
    }

    #[test]
    fn playback_state_stopped_has_no_position() {
        assert_eq!(PlaybackState::Stopped.position(), None);
    }

    #[test]
    fn playback_state_playing_exposes_position() {
        let state = PlaybackState::Playing {
            track: TrackId::new(1),
            position: SeekPosition::from_secs(12),
        };
        assert_eq!(state.position(), Some(SeekPosition::from_secs(12)));
    }

    #[test]
    fn playback_state_paused_exposes_position() {
        let state = PlaybackState::Paused {
            track: TrackId::new(3),
            position: SeekPosition::from_secs(30),
        };
        assert_eq!(state.position(), Some(SeekPosition::from_secs(30)));
    }

    #[test]
    fn playback_state_failed_is_not_stopped() {
        let state = PlaybackState::Failed {
            track: TrackId::new(1),
            error: "corrupt file".into(),
        };
        assert!(!state.is_stopped());
    }

    #[test]
    fn playback_state_failed_is_not_playing() {
        let state = PlaybackState::Failed {
            track: TrackId::new(1),
            error: "corrupt file".into(),
        };
        assert!(!state.is_playing());
    }

    #[test]
    fn playback_state_failed_exposes_current_track() {
        let id = TrackId::new(5);
        let state = PlaybackState::Failed {
            track: id,
            error: "missing file".into(),
        };
        assert_eq!(state.current_track(), Some(id));
    }

    #[test]
    fn playback_state_failed_has_no_position() {
        let state = PlaybackState::Failed {
            track: TrackId::new(1),
            error: "missing file".into(),
        };
        assert_eq!(state.position(), None);
    }

    /// A backend whose `play` always fails, so the player emits `Failed`.
    struct FailingBackend;

    impl AudioBackend for FailingBackend {
        fn play(&mut self, _path: &TrackPath) -> Result<(), AudioError> {
            Err(AudioError::Decode(
                "track.flac".into(),
                "corrupt file".into(),
            ))
        }
        fn play_paused(
            &mut self,
            _path: &TrackPath,
            _position: Duration,
        ) -> Result<(), AudioError> {
            Err(AudioError::Decode(
                "track.flac".into(),
                "corrupt file".into(),
            ))
        }
        fn pause(&mut self) {}
        // Never has a loaded source, so resume() must not make it Playing —
        // the case the phantom-playing bug hinged on.
        fn resume(&mut self) {}
        fn stop(&mut self) {}
        fn seek(&mut self, _position: Duration) {}
        fn set_volume(&mut self, _v: Volume) {}
        fn state(&self) -> BackendState {
            BackendState::Idle
        }
        fn position(&self) -> Duration {
            Duration::ZERO
        }
    }

    #[test]
    fn player_transitions_to_failed_state_on_decode_error() {
        let (tx, rx) = mpsc::channel();
        let handle = PlayerHandle::launch(
            || Ok(FailingBackend),
            move |s| {
                let _ = tx.send(s);
            },
            |_tracks| {},
        );
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        let s = recv_matching(&rx, |s| matches!(s, PlaybackState::Failed { .. }));
        assert!(matches!(s, PlaybackState::Failed { .. }));
    }

    #[test]
    fn player_failed_state_includes_the_track_id() {
        let track = julie_and_candy();
        let expected = track.id;
        let (tx, rx) = mpsc::channel();
        let handle = PlayerHandle::launch(
            || Ok(FailingBackend),
            move |s| {
                let _ = tx.send(s);
            },
            |_tracks| {},
        );
        handle.send(PlayerCommand::Play(Box::new(track)));
        let s = recv_matching(&rx, |s| matches!(s, PlaybackState::Failed { .. }));
        assert_eq!(s.current_track(), Some(expected));
    }

    #[test]
    fn player_resume_after_failure_does_not_report_playing() {
        let (tx, rx) = mpsc::channel();
        let handle = PlayerHandle::launch(
            || Ok(FailingBackend),
            move |s| {
                let _ = tx.send(s);
            },
            |_tracks| {},
        );
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Failed { .. }));

        handle.send(PlayerCommand::Resume);

        // Absence check: drain every state emitted within a bounded window and
        // assert none of them is Playing. FailingBackend never has a loaded
        // source, so resume() must never be reported as Playing.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Ok(s) = rx.recv_timeout(Duration::from_millis(50)) {
                assert!(
                    !matches!(s, PlaybackState::Playing { .. }),
                    "resume after a failed track must not report Playing, got {s:?}"
                );
            }
        }
    }

    #[test]
    fn player_pause_after_failure_does_not_report_paused() {
        let (tx, rx) = mpsc::channel();
        let handle = PlayerHandle::launch(
            || Ok(FailingBackend),
            move |s| {
                let _ = tx.send(s);
            },
            |_tracks| {},
        );
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Failed { .. }));

        handle.send(PlayerCommand::Pause);

        // Absence check: nothing was ever loaded, so there is nothing to pause.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Ok(s) = rx.recv_timeout(Duration::from_millis(50)) {
                assert!(
                    !matches!(s, PlaybackState::Paused { .. }),
                    "pause after a failed track must not report Paused, got {s:?}"
                );
            }
        }
    }

    #[test]
    fn player_seek_after_failure_reports_no_position() {
        let (tx, rx) = mpsc::channel();
        let handle = PlayerHandle::launch(
            || Ok(FailingBackend),
            move |s| {
                let _ = tx.send(s);
            },
            |_tracks| {},
        );
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Failed { .. }));

        handle.send(PlayerCommand::Seek(SeekPosition::from_secs(90)));

        // Absence check: seeking into a track that never loaded must not claim
        // a playing or paused position.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Ok(s) = rx.recv_timeout(Duration::from_millis(50)) {
                assert_eq!(
                    s.position(),
                    None,
                    "seek after a failed track must not report a position"
                );
            }
        }
    }

    #[test]
    fn player_seek_reports_the_new_position_while_paused() {
        let (handle, rx) = launch_with_channel();
        handle.send(PlayerCommand::RestorePaused {
            tracks: geogaddi_pair(),
            start: 0,
            position: SeekPosition::from_secs(12),
        });
        recv_matching(&rx, |s| matches!(s, PlaybackState::Paused { .. }));

        handle.send(PlayerCommand::Seek(SeekPosition::from_secs(90)));

        let s = recv_matching(&rx, |s| s.position() == Some(SeekPosition::from_secs(90)));
        assert!(matches!(s, PlaybackState::Paused { .. }));
    }

    /// Drains `rx` until a track list matching `pred` arrives (or 3 s elapses),
    /// mirroring `recv_matching` for the queue-snapshot channel.
    fn recv_queue_matching(
        rx: &mpsc::Receiver<Vec<Track>>,
        pred: impl Fn(&[Track]) -> bool,
    ) -> Vec<Track> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(tracks) if pred(&tracks) => return tracks,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("player thread ended before the expected queue snapshot arrived")
                }
            }
            if Instant::now() > deadline {
                panic!("deadline exceeded waiting for expected queue snapshot");
            }
        }
    }

    #[test]
    fn queue_changed_reports_full_track_list_on_play_queue() {
        let (handle, _rx, queue_rx) = launch_with_channels();
        handle.send(PlayerCommand::PlayQueue {
            tracks: geogaddi_pair(),
            start: 0,
        });
        let tracks = recv_queue_matching(&queue_rx, |t| t.len() == 2);
        assert_eq!(tracks[0].id, TrackId::new(10));
        assert_eq!(tracks[1].id, TrackId::new(20));
    }

    #[test]
    fn queue_changed_reports_appended_tracks_on_enqueue() {
        let (handle, _rx, queue_rx) = launch_with_channels();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_queue_matching(&queue_rx, |t| t.len() == 1);

        handle.send(PlayerCommand::Enqueue(geogaddi_pair()));

        let tracks = recv_queue_matching(&queue_rx, |t| t.len() == 3);
        assert_eq!(tracks[0].id, julie_and_candy().id);
        assert_eq!(tracks[1].id, TrackId::new(10));
        assert_eq!(tracks[2].id, TrackId::new(20));
    }

    #[test]
    fn queue_changed_reports_empty_list_on_stop() {
        let (handle, rx, queue_rx) = launch_with_channels();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        recv_queue_matching(&queue_rx, |t| t.len() == 1);

        handle.send(PlayerCommand::Stop);

        let tracks = recv_queue_matching(&queue_rx, |t| t.is_empty());
        assert!(tracks.is_empty());
    }

    #[test]
    fn queue_changed_not_emitted_on_next_or_previous() {
        let (handle, rx, queue_rx) = launch_with_channels();
        handle.send(PlayerCommand::PlayQueue {
            tracks: geogaddi_pair(),
            start: 0,
        });
        recv_queue_matching(&queue_rx, |t| t.len() == 2);
        recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(10)));

        handle.send(PlayerCommand::Next);
        recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(20)));

        // Absence check: the track list itself didn't change, only the
        // cursor, so no further queue snapshot should follow.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Ok(tracks) = queue_rx.recv_timeout(Duration::from_millis(50)) {
                panic!("Next must not emit a queue snapshot, got {tracks:?}");
            }
        }
    }

    #[test]
    fn set_queue_preserves_current_track_position_when_it_survives() {
        let (handle, rx, queue_rx) = launch_with_channels();
        handle.send(PlayerCommand::PlayQueue {
            tracks: geogaddi_pair(),
            start: 1,
        });
        recv_matching(&rx, |s| s.current_track() == Some(TrackId::new(20)));
        recv_queue_matching(&queue_rx, |t| t.len() == 2);

        // Prune the first track; the playing track (id 20) survives.
        handle.send(PlayerCommand::SetQueue(vec![geogaddi(20, "1969")]));

        let tracks = recv_queue_matching(&queue_rx, |t| t.len() == 1);
        assert_eq!(tracks[0].id, TrackId::new(20));

        // Playback must not have been disturbed: no Stopped state follows.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if let Ok(s) = rx.recv_timeout(Duration::from_millis(50)) {
                assert_ne!(
                    s,
                    PlaybackState::Stopped,
                    "the surviving current track must keep playing"
                );
            }
        }
    }

    #[test]
    fn set_queue_stops_playback_when_current_track_is_pruned() {
        let (handle, rx, queue_rx) = launch_with_channels();
        handle.send(PlayerCommand::Play(Box::new(julie_and_candy())));
        recv_matching(&rx, |s| matches!(s, PlaybackState::Playing { .. }));
        recv_queue_matching(&queue_rx, |t| t.len() == 1);

        // The playing track (julie_and_candy) is not in the new list.
        handle.send(PlayerCommand::SetQueue(geogaddi_pair()));

        let s = recv_matching(&rx, |s| s == &PlaybackState::Stopped);
        assert_eq!(s, PlaybackState::Stopped);
        let tracks = recv_queue_matching(&queue_rx, |t| t.is_empty());
        assert!(tracks.is_empty());
    }
}
