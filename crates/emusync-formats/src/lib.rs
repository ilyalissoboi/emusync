//! Container format adapters (DESIGN §5).
//!
//! Adapters are pure: they parse and build container images held in memory and never touch the
//! file system. The core reads the container, hands the bytes to an adapter, and writes the
//! result back with its own atomic-replace logic.
//!
//! M0 ships only the [`raw`] adapter, which treats a whole file as one opaque unit and
//! implements whole-file mode for memory cards (DESIGN §2.1).

use std::collections::BTreeMap;

pub mod raw;

pub use raw::Raw;

/// Name of the single unit of a container in whole-file mode (`ps2:Mcd001:@card`).
pub const WHOLE_FILE_UNIT: &str = "@card";

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    /// The container failed validation. It must not be modified or published.
    #[error("invalid container: {0}")]
    Invalid(String),
    /// The unit does not exist in the container.
    #[error("unit not found: {0}")]
    NotFound(String),
    /// The container has no room for the requested change.
    #[error("not enough space in container")]
    NoSpace,
    /// The operation is not supported by this adapter.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, FormatError>;

/// How sure an adapter is that it understands a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    No,
    Maybe,
    Certain,
}

/// A unit as listed in a container: its native name and a cheap hash, if the format has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRef {
    pub name: String,
}

/// Canonical form of a unit (DESIGN §2.5). `bytes` are hashed; `meta` is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canonical {
    pub name: String,
    pub bytes: Vec<u8>,
    pub meta: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub enum ApplyOp {
    Put(Canonical),
    Delete(String),
}

/// A container format. Mirrors the trait sketched in DESIGN §5, working on in-memory images.
pub trait ContainerFormat: Send + Sync {
    /// Adapter id, e.g. `"raw"` or `"ps2.card"`.
    fn id(&self) -> &'static str;
    /// Whether this adapter can supply units for a card (unit mode). `raw` cannot.
    fn supports_units(&self) -> bool;
    /// Auto-detection.
    fn probe(&self, image: &[u8]) -> Confidence;
    /// Full validation. A container that fails is never synced or modified.
    fn validate(&self, image: &[u8]) -> Result<()>;
    /// Lists the units in the container.
    fn list(&self, image: &[u8]) -> Result<Vec<UnitRef>>;
    /// Extracts one unit in canonical form.
    fn extract(&self, image: &[u8], unit: &UnitRef) -> Result<Canonical>;
    /// Builds a new image with `ops` applied. The input image is validated first.
    fn apply(&self, image: &[u8], ops: &[ApplyOp]) -> Result<Vec<u8>>;
}

/// Looks up an adapter by id.
pub fn by_id(id: &str) -> Option<&'static dyn ContainerFormat> {
    match id {
        "raw" => Some(&Raw),
        _ => None,
    }
}

/// Infers the platform from a card file's extension, for logical card ids (`ps2:Mcd001`).
pub fn infer_platform(file_name: &str) -> &'static str {
    let ext = file_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "ps2" => "ps2",
        "mcd" | "mcr" | "gme" | "vgs" | "vmp" => "ps1",
        "gcp" | "raw" => "gc",
        "srm" | "sav" | "eep" | "fla" => "sram",
        _ => "file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_inference() {
        assert_eq!(infer_platform("Mcd001.ps2"), "ps2");
        assert_eq!(infer_platform("epsxe000.MCR"), "ps1");
        assert_eq!(infer_platform("noext"), "file");
    }
}
