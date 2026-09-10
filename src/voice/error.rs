#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    #[error("not connected to a voice channel")]
    NotConnected,
    #[error("nothing is playing")]
    NothingPlaying,
    #[error("not enough upcoming tracks to shuffle")]
    NothingToShuffle,
    #[error("the queue is already empty")]
    QueueEmpty,
    #[error("invalid queue position")]
    InvalidQueueIndex,
    #[error("failed to join voice channel: {0}")]
    Join(String),
    #[error("playback error: {0}")]
    Playback(String),
    #[error("failed to save setting: {0}")]
    Storage(String),
}
