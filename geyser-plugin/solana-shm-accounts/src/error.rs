use thiserror::Error;

#[derive(Debug, Error)]
pub enum ShmError {
    #[error("Writer appears stale (heartbeat not updated)")]
    WriterStale,
    #[error("Writer process is dead (pid {0} not found)")]
    WriterDead(u32),
    #[error("Torn read detected")]
    TornRead,
    #[error("Account data ({actual} bytes) exceeds slot capacity ({capacity} bytes)")]
    SlotOverflow { actual: u32, capacity: u32 },
    #[error("Invalid magic number: expected {expected:#x}, got {actual:#x}")]
    InvalidMagic { expected: u64, actual: u64 },
    #[error("Version mismatch: expected {expected}, got {actual}")]
    VersionMismatch { expected: u64, actual: u64 },
    #[error("Pubkey not found in shm offset table")]
    PubkeyNotFound,
    #[error("Shm not ready (timeout waiting for writer)")]
    NotReady,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
