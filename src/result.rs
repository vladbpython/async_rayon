use super::errors::SpawnError;
pub type SpawnResult<T> = Result<T, SpawnError>;