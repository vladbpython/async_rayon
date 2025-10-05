#[derive(Debug, PartialEq, PartialOrd, Eq, Ord, Clone)]
pub enum SpawnError {
    JoinFailed(String),
    Panic(String),
    ChannelClosed,
    SemaphoreClosed,
    Timeout,
    Cancelled,
}