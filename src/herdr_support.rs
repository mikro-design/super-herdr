//! What this build needs from Herdr, named rather than numbered.
//!
//! Herdr's protocol moves faster than this project can qualify it — 0.8.2 was
//! the tested combination while 0.9.0 was already out — so the question a
//! feature asks has to survive numbers it has never seen. A scattered
//! comparison against protocol 20 cannot: each one is a separate decision about
//! a version nobody was thinking about when it was written, and each has to be
//! found again when the answer changes.
//!
//! So a feature names what it needs, and this module answers. Two sources, in
//! order: what the host says it can do, and, for hosts that say nothing, the
//! protocol that first carried it. The first is the reason a newer Herdr works
//! without a release here; the second is the reason an older one still does.

use crate::state::NormalizedSnapshot;

/// The newest protocol this project has actually driven end to end.
///
/// Not a ceiling. Nothing is refused for being newer — a build that rejected
/// what it had not been told about would break on every Herdr release, which is
/// the opposite of the point. It is what `doctor` says it has confidence in.
pub const TESTED_PROTOCOL: u64 = 20;

/// The newest protocol this build knows exists, for reporting the distance.
pub const LATEST_KNOWN_PROTOCOL: u64 = 22;

/// Something a target can do that not every target can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// Plugin actions: listing a target's plugin actions and invoking one.
    PluginActions,
}

impl Feature {
    /// The name Herdr reports in its snapshot capabilities, for hosts that
    /// report any. Absence proves nothing — Herdr 0.8 reports no capability
    /// object at all — so this is only ever evidence in favour.
    const fn capability(self) -> &'static str {
        match self {
            Self::PluginActions => "plugins",
        }
    }

    /// The protocol that first carried it, which is the answer for a host that
    /// describes itself only by number.
    const fn first_protocol(self) -> u64 {
        match self {
            Self::PluginActions => 20,
        }
    }

    /// What to call it where somebody has to read it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::PluginActions => "plugin actions",
        }
    }
}

/// Every feature this build gates, so a report can list them without knowing
/// which ones exist.
pub const FEATURES: &[Feature] = &[Feature::PluginActions];

/// Whether a target can do this.
pub fn supports(snapshot: Option<&NormalizedSnapshot>, feature: Feature) -> bool {
    let Some(snapshot) = snapshot else {
        return false;
    };
    if snapshot.capabilities.contains(feature.capability()) {
        return true;
    }
    snapshot
        .protocol
        .is_some_and(|protocol| protocol >= feature.first_protocol())
}

/// Whether a protocol alone carries this, for callers holding a number and no
/// snapshot — a probe report, most of all.
pub fn supports_protocol(protocol: u64, feature: Feature) -> bool {
    protocol >= feature.first_protocol()
}

/// Where a target's protocol sits relative to what this build knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// The host did not say, which is every Herdr too old to report one.
    Unknown,
    /// Older than something this build gates on, so a feature is hidden.
    Limited,
    /// Inside what this project has driven.
    Tested,
    /// Newer than this project has driven. Accepted: Herdr adds to its
    /// protocol rather than reshaping it, and refusing would strand anyone who
    /// updates Herdr before this catches up.
    Newer,
}

/// The lowest protocol any gated feature asks for.
fn lowest_gate() -> u64 {
    FEATURES
        .iter()
        .map(|feature| feature.first_protocol())
        .min()
        .unwrap_or(0)
}

pub fn standing(protocol: Option<u64>) -> Standing {
    match protocol {
        None => Standing::Unknown,
        Some(protocol) if protocol < lowest_gate() => Standing::Limited,
        Some(protocol) if protocol > TESTED_PROTOCOL => Standing::Newer,
        Some(_) => Standing::Tested,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn snapshot(protocol: Option<u64>, capabilities: &[&str]) -> NormalizedSnapshot {
        NormalizedSnapshot {
            protocol,
            capabilities: capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect::<BTreeSet<_>>(),
            ..NormalizedSnapshot::default()
        }
    }

    /// A host that says what it can do is believed, whatever its number.
    ///
    /// This is the case that stops a Herdr release from needing one here: a
    /// protocol nobody has seen still answers the only question being asked.
    #[test]
    fn a_reported_capability_settles_it_without_a_number() {
        assert!(supports(
            Some(&snapshot(None, &["plugins"])),
            Feature::PluginActions
        ));
        assert!(supports(
            Some(&snapshot(Some(99), &["plugins"])),
            Feature::PluginActions
        ));
    }

    /// Herdr 0.8 reports no capabilities at all, so their absence is not an
    /// answer and the protocol has to be one.
    #[test]
    fn a_host_that_describes_itself_only_by_number_is_read_that_way() {
        assert!(!supports(
            Some(&snapshot(Some(19), &[])),
            Feature::PluginActions
        ));
        assert!(supports(
            Some(&snapshot(Some(20), &[])),
            Feature::PluginActions
        ));
        // Newer than anything this build was written against, and still yes:
        // a feature that arrived at 20 does not leave at 22.
        assert!(supports(
            Some(&snapshot(Some(22), &[])),
            Feature::PluginActions
        ));
        assert!(!supports(None, Feature::PluginActions));
    }

    #[test]
    fn a_protocol_newer_than_tested_is_reported_rather_than_refused() {
        assert_eq!(standing(None), Standing::Unknown);
        assert_eq!(standing(Some(19)), Standing::Limited);
        assert_eq!(standing(Some(TESTED_PROTOCOL)), Standing::Tested);
        assert_eq!(standing(Some(LATEST_KNOWN_PROTOCOL)), Standing::Newer);
        assert_eq!(standing(Some(LATEST_KNOWN_PROTOCOL + 5)), Standing::Newer);
    }
}
