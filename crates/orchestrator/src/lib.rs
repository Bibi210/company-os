pub mod db;
pub mod embedding;
pub mod engine;
pub mod error;
pub mod fusion;
pub mod lock;
pub mod query;
pub mod roadmap_summary;
pub mod types;

pub use db::OrchestratorDb;
pub use embedding::{EMBEDDING_DIM, Embedder, build_embedding_view, model_version};
pub use engine::{OrchestratorEngine, RfcUpdateResult, compute_consensus};
pub use error::OrchestratorError;
pub use fusion::{DEFAULT_RRF_K, FusedResult, RankedResult, rrf_fuse};
pub use lock::{DbLockGuard, acquire_exclusive_blocking, try_acquire_exclusive};
pub use query::{QueryMode, sanitize_fts_query};
pub use roadmap_summary::{
    RoadmapCounters, RoadmapHeader, RoadmapItem, RoadmapItemRef, RoadmapListEntry, RoadmapSelector,
    RoadmapSummary,
};
pub use types::{
    ArtifactPath, ArtifactSummary, ConsensusResult, Finding, IndexedArtifact, PathPattern,
    PermitStatus, RelationDirection, RelationLink, ReviewRound, ReviewVerdict, ReviewVote,
    RoundStatus, WritePermit,
};
