//! Cœur de distribution : diffuseur de trames (`FrameHub`), état de la capture
//! (`SourceStatus`) et superviseur de capture (`CaptureSupervisor`).
//!
//! Ne dépend que de `domain`, `tokio` (`sync`, `time`, `Handle`) et `tokio-util`
//! (`CancellationToken`) : ni V4L2, ni HTTP (ADR 0001).

pub mod hub;
pub mod status;
pub mod supervisor;

pub use hub::{DemandReceiver, FrameHub, FrameSubscriber, HubCapacity};
pub use status::{SourceStatus, StatusReceiver};
pub use supervisor::{
    BackoffPolicy, CaptureHandle, CaptureSupervisor, CaptureThreadError, SupervisorSettings,
    CAPTURE_THREAD_NAME,
};
