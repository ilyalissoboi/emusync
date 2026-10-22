//! Container format adapters (DESIGN §5).
//!
//! Adapters are pure: they parse and build container images held in memory and never touch the
//! file system. The core reads the container, hands the bytes to an adapter, and writes the
//! result back with its own atomic-replace logic.
//!
//! - [`raw`]: a whole file as one opaque unit; the fallback for formats without an adapter.
//! - [`ps2`] (`ps2.card`): PS2 memory card images, one unit per save directory.
//! - [`ps2::folder`] (`ps2.card.folder`): PCSX2 folder memory cards, with the same units.
//! - [`gc::card`] (`gc.card`): GameCube memory card images (Dolphin's `.raw`), one unit per save.
//! - [`gc::gci`] (`gc.gci`): Dolphin's GCI folders, with the same units.
//! - [`psp`] (`psp.savedata`): a PSP memory stick's `SAVEDATA` folder, one unit per save folder.
//!
//! A container that is a directory on the host is handed to its adapter as a [`tree::Tree`]
//! image ([`ContainerKind::Directory`]), or, for a folder of save folders, as a tree image of
//! the save folders the core is working on ([`ContainerKind::UnitDirs`]).

use std::collections::BTreeMap;

pub mod gc;
pub mod ps2;
pub mod psp;
pub mod raw;
pub mod tree;

pub use gc::card::GcCard;
pub use gc::gci::GciFolder;
pub use ps2::folder::Ps2Folder;
pub use ps2::Ps2Card;
pub use psp::PspSavedata;
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

/// What a container is on the host, which decides how the core reads and writes its image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    /// A single file; the image is its bytes.
    File,
    /// A directory; the image is a [`tree::Tree`] of everything under it.
    Directory,
    /// A directory whose subdirectories are the units, each named as its unit (a PSP
    /// `SAVEDATA` folder). The core reads, checks and replaces each subdirectory on its own,
    /// and leaves everything else in the directory alone (DESIGN §27). The adapter's image is a
    /// [`tree::Tree`] of only the subdirectories at hand; in it, a unit is a root directory.
    UnitDirs,
}

/// A container format. Mirrors the trait sketched in DESIGN §5, working on in-memory images.
pub trait ContainerFormat: Send + Sync {
    /// Adapter id, e.g. `"raw"` or `"ps2.card"`.
    fn id(&self) -> &'static str;
    /// Whether containers of this format are files or directories.
    fn kind(&self) -> ContainerKind {
        ContainerKind::File
    }
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
    /// Extracts every unit, in container order. Adapters override this to parse only once.
    fn extract_all(&self, image: &[u8]) -> Result<Vec<Canonical>> {
        self.list(image)?.iter().map(|u| self.extract(image, u)).collect()
    }
    /// Title keys (`ps2:SLUS-20062`) a unit belongs to, from its name. Empty if unknown.
    fn titles(&self, _unit_name: &str) -> Vec<String> {
        Vec::new()
    }
    /// Builds a new image with `ops` applied. The input image is validated first.
    fn apply(&self, image: &[u8], ops: &[ApplyOp]) -> Result<Vec<u8>>;
    /// The image of a new, empty container named `file_name`, for formats whose emulator would
    /// create one itself on first use; the engine then creates a missing container when saves
    /// arrive for it (DESIGN §25.4). `None`: a missing container stays missing.
    fn empty(&self, _file_name: &str) -> Option<Vec<u8>> {
        None
    }
    /// [`ContainerKind::UnitDirs`] only: the size in bytes (all files in the subdirectory)
    /// above which a subdirectory is not a unit: never read, synced, replaced or deleted. `None`:
    /// no limit.
    fn max_unit_size(&self) -> Option<u64> {
        None
    }
}

/// Looks up an adapter by id.
pub fn by_id(id: &str) -> Option<&'static dyn ContainerFormat> {
    match id {
        "raw" => Some(&Raw),
        "ps2.card" => Some(&Ps2Card),
        "ps2.card.folder" => Some(&Ps2Folder),
        "gc.card" => Some(&GcCard),
        "gc.gci" => Some(&GciFolder),
        "psp.savedata" => Some(&PspSavedata),
        _ => None,
    }
}

/// Picks the adapter for a directory container from its [`tree::Tree`] image: a PCSX2 folder
/// card (it has a `_pcsx2_superblock`), a Dolphin GCI folder (it has a valid `.gci` file), a
/// PSP `SAVEDATA` folder (a folder in it has a `PARAM.SFO`), or nothing.
pub fn detect_dir(image: &[u8]) -> Option<&'static dyn ContainerFormat> {
    let dirs: [&'static dyn ContainerFormat; 3] = [&Ps2Folder, &GciFolder, &PspSavedata];
    dirs.into_iter().find(|f| f.probe(image) == Confidence::Certain)
}

/// Picks the adapter for a card file: one whose format is recognised in the image, or whose
/// platform's standard card size the file has (a card the emulator created but nobody has
/// formatted yet). Everything else is `raw`.
pub fn detect(file_name: &str, image: &[u8]) -> &'static dyn ContainerFormat {
    if Ps2Card.probe(image) == Confidence::Certain
        || (infer_platform(file_name) == "ps2" && Ps2Card.probe(image) == Confidence::Maybe)
    {
        return &Ps2Card;
    }
    // A GameCube card is recognised by its header's checksum and size field (it has no magic).
    if GcCard.probe(image) == Confidence::Certain {
        return &GcCard;
    }
    &Raw
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

    #[test]
    fn detection() {
        let card = ps2::blank_card(8192, true, [0; 8]);
        assert_eq!(detect("anything.bin", &card).id(), "ps2.card");
        assert_eq!(detect("Mcd001.ps2", &vec![0xFF; card.len()]).id(), "ps2.card");
        assert_eq!(detect("Mcd001.ps2", b"not a card").id(), "raw");
        assert_eq!(detect("save.srm", &vec![0xFF; card.len()]).id(), "raw");
        let gc = gc::card::blank_card(4, [1; 12], 0, false);
        assert_eq!(detect("MemoryCardA.USA.raw", &gc).id(), "gc.card");
        assert_eq!(detect("SRAM.raw", &[0; 68]).id(), "raw");
    }
}
