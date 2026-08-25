pub mod budget;
pub mod layout;
pub mod media;
pub mod queue;
pub mod sources;

pub use budget::{Admission, Codec, DecodeBudget, StreamSpec};
pub use layout::{solve_grid, Rect};
pub use media::{MediaItem, SourceKind};
pub use sources::{JellyfinSource, PlexSource, ServerConfig, SourceConfig};
pub use queue::{PlaybackQueue, QueueOrder};
