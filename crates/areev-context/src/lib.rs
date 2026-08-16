//! areev-context — LLM-ready context assembly.
//!
//! Context module: takes recall results (`SearchHit`s)
//! and renders budget-aware, provider-optimal context strings — SML for
//! Claude, Markdown for GPT-class, TOON/JSON for machine consumers — with
//! progressive disclosure (Full/Summary/Omit) and grain-type sections.
//! Docs frame this as infrastructure for context engineering (§1 stance).

pub mod assembly;
pub mod budget;
pub mod policy;
pub mod presets;
pub mod render;

pub use assembly::{ContextAssembler, FormattedContext, RenderingHints};
pub use budget::Allocation;
pub use policy::{
    FormatPolicy, GrainTypeOverride, MetadataLevel, Ordering, OutputFormat, SectionConfig,
};
