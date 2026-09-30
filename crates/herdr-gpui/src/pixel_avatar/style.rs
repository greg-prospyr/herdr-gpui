//! What an avatar looks like: one of a few friendly shapes in one of a fixed
//! palette of hues. Every agent has a default derived from a stable key, so
//! avatars differ from each other before anyone picks one.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Shape {
    Blob,
    Drop,
    Peak,
    Gem,
    Cloud,
    Block,
}

impl Shape {
    pub(crate) const ALL: [Self; 6] = [
        Self::Blob,
        Self::Drop,
        Self::Peak,
        Self::Gem,
        Self::Cloud,
        Self::Block,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Blob => "Blob",
            Self::Drop => "Drop",
            Self::Peak => "Peak",
            Self::Gem => "Gem",
            Self::Cloud => "Cloud",
            Self::Block => "Block",
        }
    }
}

/// Avatar colors are their own palette, never the terminal theme's ANSI
/// slots, so an agent keeps its look whichever theme is loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Hue {
    Tangerine,
    Grape,
    Berry,
    Slate,
    Cobalt,
    Lagoon,
    Sage,
    Honey,
    Cocoa,
    Rose,
}

impl Hue {
    pub(crate) const ALL: [Self; 10] = [
        Self::Tangerine,
        Self::Grape,
        Self::Berry,
        Self::Slate,
        Self::Cobalt,
        Self::Lagoon,
        Self::Sage,
        Self::Honey,
        Self::Cocoa,
        Self::Rose,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Tangerine => "Tangerine",
            Self::Grape => "Grape",
            Self::Berry => "Berry",
            Self::Slate => "Slate",
            Self::Cobalt => "Cobalt",
            Self::Lagoon => "Lagoon",
            Self::Sage => "Sage",
            Self::Honey => "Honey",
            Self::Cocoa => "Cocoa",
            Self::Rose => "Rose",
        }
    }

    /// The colour a moon's tint leans toward.
    pub(crate) fn base(self) -> u32 {
        match self {
            Self::Tangerine => 0xe8732c,
            Self::Grape => 0x7b4fd6,
            Self::Berry => 0xc23b7a,
            Self::Slate => 0x75777d,
            Self::Cobalt => 0x4a6ee0,
            Self::Lagoon => 0x5e9e8e,
            Self::Sage => 0x7fae6a,
            Self::Honey => 0xe0a030,
            Self::Cocoa => 0x7a5a3a,
            Self::Rose => 0xd9577a,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct AvatarStyle {
    pub shape: Shape,
    pub hue: Hue,
}

impl AvatarStyle {
    /// A stable default for `key`, such as a pane ID: the same agent looks
    /// the same on every run, and neighbours rarely match.
    pub(crate) fn default_for(key: &str) -> Self {
        // FNV-1a: stable across Rust releases, unlike `DefaultHasher`.
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in key.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        let shapes = Shape::ALL.len() as u64;
        let hues = Hue::ALL.len() as u64;
        Self {
            shape: Shape::ALL[(hash % shapes) as usize],
            hue: Hue::ALL[(hash / shapes % hues) as usize],
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn defaults_are_stable_and_spread_across_the_palette() {
        assert_eq!(
            AvatarStyle::default_for("w1:p1"),
            AvatarStyle::default_for("w1:p1")
        );
        let styles: std::collections::HashSet<_> = (0..60)
            .map(|index| AvatarStyle::default_for(&format!("w651db737963194:p{index}")))
            .collect();
        assert!(styles.len() > 25, "{}", styles.len());
        let shapes: std::collections::HashSet<_> = styles.iter().map(|s| s.shape).collect();
        let hues: std::collections::HashSet<_> = styles.iter().map(|s| s.hue).collect();
        assert_eq!(shapes.len(), Shape::ALL.len());
        assert_eq!(hues.len(), Hue::ALL.len());
    }

    #[test]
    fn styles_store_by_name() {
        let style = AvatarStyle {
            shape: Shape::Cloud,
            hue: Hue::Lagoon,
        };
        let text = serde_json::to_string(&style).unwrap();
        assert_eq!(text, r#"{"shape":"cloud","hue":"lagoon"}"#);
        assert_eq!(serde_json::from_str::<AvatarStyle>(&text).unwrap(), style);
    }
}
