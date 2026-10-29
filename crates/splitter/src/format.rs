//! What the output is: its PDF version, and whether its streams are compressed.
//!
//! Both are the person's choice, and both have a default that follows the cut: a single rewritten
//! file is worth compressing and worth the 1.5 that object streams need, while cutting into many
//! parts is fastest with the streams copied as they are.

use std::fmt;

/// A PDF version, as the header spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u8,
    minor: u8,
}

impl Version {
    pub const PDF_1_3: Self = Self { major: 1, minor: 3 };
    pub const PDF_1_4: Self = Self { major: 1, minor: 4 };
    pub const PDF_1_5: Self = Self { major: 1, minor: 5 };
    pub const PDF_1_6: Self = Self { major: 1, minor: 6 };
    pub const PDF_1_7: Self = Self { major: 1, minor: 7 };
    pub const PDF_2_0: Self = Self { major: 2, minor: 0 };

    /// Every version the interface offers, oldest first.
    pub const OFFERED: [Self; 4] = [Self::PDF_1_5, Self::PDF_1_6, Self::PDF_1_7, Self::PDF_2_0];

    /// Read `1.7`, `2.0` and the like. `None` when the text is not a version.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (major, minor) = text.trim().split_once('.')?;
        Some(Self { major: major.parse().ok()?, minor: minor.parse().ok()? })
    }

    /// The version the source file declares, when the header can be read.
    #[must_use]
    pub fn of_source(text: &str) -> Option<Self> {
        Self::parse(text)
    }

    #[must_use]
    pub fn major(self) -> u8 {
        self.major
    }

    #[must_use]
    pub fn minor(self) -> u8 {
        self.minor
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// What to do with the streams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    /// Every stream is copied byte for byte, exactly as the source holds it: the fastest way to
    /// write a part, and the one that never changes what a stream says.
    #[default]
    Neutral,
    /// Streams the source left uncompressed are Flate-compressed, and the part's own objects go
    /// into an object stream behind a cross-reference stream. Images are never touched: a stream
    /// that already carries a filter is copied as it is, so nothing lossy can happen here.
    Compress,
}

impl Compression {
    /// Every compression the interface offers.
    pub const OFFERED: [Self; 2] = [Self::Neutral, Self::Compress];

    /// True when the object stream and the cross-reference stream are wanted: both need PDF 1.5.
    #[must_use]
    pub fn structures_objects(self) -> bool {
        self == Self::Compress
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Neutral => "neutral",
            Self::Compress => "compress",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_read_and_order_as_they_are_written() {
        assert_eq!(Version::parse("1.7"), Some(Version::PDF_1_7));
        assert_eq!(Version::parse("2.0"), Some(Version::PDF_2_0));
        assert_eq!(Version::parse("1"), None);
        assert_eq!(Version::parse("abc"), None);
        assert!(Version::PDF_1_5 < Version::PDF_1_7);
        assert!(Version::PDF_1_7 < Version::PDF_2_0);
        assert_eq!(Version::PDF_1_5.to_string(), "1.5");
    }

    #[test]
    fn the_source_header_names_its_version() {
        assert_eq!(Version::of_source("1.4"), Some(Version::PDF_1_4));
        // A header that says nothing usable falls back to what the caller asked for.
        assert_eq!(Version::of_source(""), None);
    }

    #[test]
    fn only_compression_asks_for_object_streams() {
        assert!(Compression::Compress.structures_objects());
        assert!(!Compression::Neutral.structures_objects());
        assert_eq!(Compression::default(), Compression::Neutral);
    }
}
