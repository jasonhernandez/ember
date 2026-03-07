use std::fmt;
use std::str::FromStr;

use serde::de;

/// A byte size parsed from a human-readable string like `512M` or `16G`.
///
/// Stores the value internally in bytes. Supports MiB (`M`) and GiB (`G`) suffixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize {
    bytes: u64,
}

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;

impl ByteSize {
    /// Create a `ByteSize` from a number of GiB (const-compatible).
    pub const fn from_gib(gib: u64) -> Self {
        Self { bytes: gib * GIB }
    }

    /// Convert to whole MiB, returning an error if not evenly divisible.
    pub fn to_mib(self) -> Result<u32, String> {
        if self.bytes % MIB != 0 {
            return Err(format!(
                "{} bytes is not evenly divisible into MiB",
                self.bytes
            ));
        }
        let mib = self.bytes / MIB;
        u32::try_from(mib).map_err(|_| format!("{mib} MiB exceeds u32 range"))
    }

    /// Convert to whole GiB, returning an error if not evenly divisible.
    pub fn to_gib(self) -> Result<u32, String> {
        if self.bytes % GIB != 0 {
            return Err(format!(
                "{} bytes is not evenly divisible into GiB",
                self.bytes
            ));
        }
        let gib = self.bytes / GIB;
        u32::try_from(gib).map_err(|_| format!("{gib} GiB exceeds u32 range"))
    }
}

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty size string".to_string());
        }

        let (num_str, suffix) = if s.ends_with('G') || s.ends_with('g') {
            (&s[..s.len() - 1], "G")
        } else if s.ends_with('M') || s.ends_with('m') {
            (&s[..s.len() - 1], "M")
        } else {
            return Err(format!(
                "'{s}' requires a unit suffix (M for MiB, G for GiB)"
            ));
        };

        let num: u64 = num_str
            .trim()
            .parse()
            .map_err(|_| format!("invalid number: '{num_str}'"))?;

        let bytes = match suffix {
            "M" => num.checked_mul(MIB),
            "G" => num.checked_mul(GIB),
            _ => unreachable!(),
        };

        let bytes = bytes.ok_or_else(|| format!("{num}{suffix} overflows u64"))?;
        Ok(Self { bytes })
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.bytes % GIB == 0 {
            write!(f, "{}G", self.bytes / GIB)
        } else if self.bytes % MIB == 0 {
            write!(f, "{}M", self.bytes / MIB)
        } else {
            write!(f, "{}B", self.bytes)
        }
    }
}

impl<'de> de::Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ByteSize::from_str(&s).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_megabytes() {
        let size: ByteSize = "512M".parse().unwrap();
        assert_eq!(size.to_mib().unwrap(), 512);
    }

    #[test]
    fn parse_gigabytes() {
        let size: ByteSize = "4G".parse().unwrap();
        assert_eq!(size.to_gib().unwrap(), 4);
    }

    #[test]
    fn from_gib_const() {
        let size = ByteSize::from_gib(16);
        assert_eq!(size.to_gib().unwrap(), 16);
        assert_eq!(size.to_mib().unwrap(), 16 * 1024);
    }

    #[test]
    fn reject_bare_integer() {
        let err = "512".parse::<ByteSize>().unwrap_err();
        assert!(err.contains("requires a unit suffix"), "got: {err}");
    }

    #[test]
    fn display() {
        assert_eq!(ByteSize::from_gib(8).to_string(), "8G");
        assert_eq!("512M".parse::<ByteSize>().unwrap().to_string(), "512M");
    }
}
