//! `raw`: any single file as one opaque unit (DESIGN §5.3). Implements whole-file mode.

use crate::{ApplyOp, Canonical, Confidence, ContainerFormat, FormatError, Result, UnitRef, WHOLE_FILE_UNIT};
use std::collections::BTreeMap;

pub struct Raw;

impl ContainerFormat for Raw {
    fn id(&self) -> &'static str {
        "raw"
    }

    fn supports_units(&self) -> bool {
        false
    }

    fn probe(&self, _image: &[u8]) -> Confidence {
        Confidence::Maybe
    }

    fn validate(&self, _image: &[u8]) -> Result<()> {
        Ok(())
    }

    fn list(&self, _image: &[u8]) -> Result<Vec<UnitRef>> {
        Ok(vec![UnitRef { name: WHOLE_FILE_UNIT.to_string() }])
    }

    fn extract(&self, image: &[u8], unit: &UnitRef) -> Result<Canonical> {
        if unit.name != WHOLE_FILE_UNIT {
            return Err(FormatError::NotFound(unit.name.clone()));
        }
        Ok(Canonical { name: WHOLE_FILE_UNIT.to_string(), bytes: image.to_vec(), meta: BTreeMap::new() })
    }

    fn apply(&self, image: &[u8], ops: &[ApplyOp]) -> Result<Vec<u8>> {
        self.validate(image)?;
        let mut out = image.to_vec();
        for op in ops {
            match op {
                ApplyOp::Put(c) if c.name == WHOLE_FILE_UNIT => out = c.bytes.clone(),
                ApplyOp::Put(c) => return Err(FormatError::NotFound(c.name.clone())),
                ApplyOp::Delete(_) => {
                    return Err(FormatError::Unsupported("raw containers cannot delete their only unit".into()))
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let unit = UnitRef { name: WHOLE_FILE_UNIT.into() };
        let c = Raw.extract(b"old", &unit).unwrap();
        assert_eq!(c.bytes, b"old");
        let new = Canonical { name: WHOLE_FILE_UNIT.into(), bytes: b"new".to_vec(), meta: BTreeMap::new() };
        let img = Raw.apply(b"old", &[ApplyOp::Put(new)]).unwrap();
        assert_eq!(Raw.extract(&img, &unit).unwrap().bytes, b"new");
        assert!(Raw.apply(b"old", &[ApplyOp::Delete(WHOLE_FILE_UNIT.into())]).is_err());
    }
}
