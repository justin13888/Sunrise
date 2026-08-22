//! Context entity per `docs/02-domain/contexts-and-tags.md`.
//!
//! A Context is a cross-cutting facet applied to Tasks across Streams
//! (`@errands`, `@deep-work`, `@waiting-on:carlos`). Membership lives on the
//! Task side as an OR-set (`Task::contexts`); this module owns the Context
//! entity itself plus the draft/patch shapes the core's `CreateContext` /
//! `UpdateContext` commands take.
//!
//! # Reserved prefixes
//!
//! The spec reserves `waiting-on:` and `energy:` but is explicit that these
//! are *conventions, not enforced types*: "A user can ignore them." So this
//! module does **not** reject or rewrite names carrying them. What it does
//! provide is [`ContextFacet`], one shared classifier so that every surface
//! (Today's energy filter, the weekly-review waiting-on roll-up) interprets a
//! name the same way instead of each re-implementing the prefix match.

use crate::common::Energy;
use crate::validation::{validate_title, ValidationError, MAX_CONTEXT_NAME_LEN};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Maximum `description` length, per the spec CDDL (`description?: text<280>`).
pub const MAX_CONTEXT_DESCRIPTION_LEN: usize = 280;

/// Reserved prefix: the task is blocked on a person or thing.
pub const WAITING_ON_PREFIX: &str = "waiting-on:";

/// Reserved prefix: the context names an energy level.
pub const ENERGY_PREFIX: &str = "energy:";

/// Persisted Context (cross-cutting tag).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    /// Context id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// Display name, without the leading `@` (e.g. `deep-work`).
    pub name: String,
    /// Optional description shown in the picker.
    #[serde(default)]
    pub description: Option<String>,
    /// Archived contexts stay on the Tasks that carry them but drop out of
    /// pickers and out of `@name` capture resolution.
    #[serde(default)]
    pub archived: bool,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
}

/// How a Context name reads under the spec's two reserved prefixes.
///
/// Purely advisory — see the module docs. A name that carries `energy:` but
/// does not name one of `low` / `med` / `high` is [`ContextFacet::Plain`],
/// because the spec defines the prefix's meaning only for those three values
/// and guessing at anything else would be worse than leaving it uninterpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextFacet<'a> {
    /// `waiting-on:<who>` — carries the non-empty remainder.
    WaitingOn(&'a str),
    /// `energy:<low|med|high>`.
    Energy(Energy),
    /// Anything else: an ordinary user-defined context.
    Plain,
}

impl Context {
    /// Trim and validate the name.
    pub fn validate_name(name: &str) -> Result<String, ValidationError> {
        validate_title(name, "context.name", MAX_CONTEXT_NAME_LEN)
    }

    /// Trim and validate an optional description.
    pub fn validate_description(desc: Option<&str>) -> Result<Option<String>, ValidationError> {
        let Some(raw) = desc else { return Ok(None) };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        if trimmed.chars().count() > MAX_CONTEXT_DESCRIPTION_LEN {
            return Err(ValidationError::Field {
                field: "context.description",
                constraint: "max_length",
            });
        }
        Ok(Some(trimmed.to_string()))
    }

    /// Case- and whitespace-insensitive key used to detect duplicate names.
    ///
    /// `@Errands` and `@errands` are the same context to a user typing a
    /// capture line, so they must be the same context to the store.
    #[must_use]
    pub fn normalize_name(name: &str) -> String {
        name.trim().to_lowercase()
    }

    /// Classify `name` against the spec's reserved prefixes.
    #[must_use]
    pub fn facet(name: &str) -> ContextFacet<'_> {
        let n = name.trim();
        if let Some(who) = n.strip_prefix(WAITING_ON_PREFIX) {
            let who = who.trim();
            if !who.is_empty() {
                return ContextFacet::WaitingOn(who);
            }
            return ContextFacet::Plain;
        }
        if let Some(level) = n.strip_prefix(ENERGY_PREFIX) {
            return match level.trim().to_lowercase().as_str() {
                "low" => ContextFacet::Energy(Energy::Low),
                "med" | "medium" => ContextFacet::Energy(Energy::Med),
                "high" => ContextFacet::Energy(Energy::High),
                _ => ContextFacet::Plain,
            };
        }
        ContextFacet::Plain
    }

    /// This context's facet.
    #[must_use]
    pub fn facet_of(&self) -> ContextFacet<'_> {
        Self::facet(&self.name)
    }
}

/// Draft used by the UI when creating a Context.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextDraft {
    /// Display name, without the leading `@`.
    pub name: String,
    /// Optional description.
    #[serde(default)]
    pub description: Option<String>,
}

impl ContextDraft {
    /// Validate name and description.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let _ = Context::validate_name(&self.name)?;
        let _ = Context::validate_description(self.description.as_deref())?;
        Ok(())
    }
}

/// Patch applied via `Command::UpdateContext`. A `None` field is "leave alone";
/// `description: Some(None)` clears the description.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextPatch {
    /// New name.
    pub name: Option<String>,
    /// New description (`Some(None)` clears it).
    pub description: Option<Option<String>>,
    /// Archive / unarchive.
    pub archived: Option<bool>,
}

impl ContextPatch {
    /// Validate whichever fields are present.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(n) = &self.name {
            let _ = Context::validate_name(n)?;
        }
        if let Some(d) = &self.description {
            let _ = Context::validate_description(d.as_deref())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_trims_and_accepts_a_normal_name() {
        let d = ContextDraft {
            name: "  errands ".into(),
            description: None,
        };
        d.validate().unwrap();
        assert_eq!(Context::validate_name(&d.name).unwrap(), "errands");
    }

    #[test]
    fn draft_rejects_blank_name() {
        let d = ContextDraft {
            name: "   ".into(),
            description: None,
        };
        assert_eq!(d.validate(), Err(ValidationError::InvalidTitle));
    }

    #[test]
    fn draft_rejects_overlong_name() {
        let d = ContextDraft {
            name: "x".repeat(MAX_CONTEXT_NAME_LEN + 1),
            description: None,
        };
        assert!(matches!(
            d.validate(),
            Err(ValidationError::Field {
                field: "context.name",
                constraint: "max_length"
            })
        ));
    }

    #[test]
    fn draft_rejects_overlong_description() {
        let d = ContextDraft {
            name: "errands".into(),
            description: Some("d".repeat(MAX_CONTEXT_DESCRIPTION_LEN + 1)),
        };
        assert!(matches!(
            d.validate(),
            Err(ValidationError::Field {
                field: "context.description",
                ..
            })
        ));
    }

    #[test]
    fn blank_description_reads_as_absent() {
        assert_eq!(Context::validate_description(Some("   ")).unwrap(), None);
    }

    #[test]
    fn patch_validates_only_present_fields() {
        ContextPatch::default().validate().unwrap();
        assert_eq!(
            ContextPatch {
                name: Some(" ".into()),
                ..Default::default()
            }
            .validate(),
            Err(ValidationError::InvalidTitle)
        );
    }

    #[test]
    fn duplicate_names_normalize_to_one_key() {
        assert_eq!(
            Context::normalize_name(" Errands "),
            Context::normalize_name("errands")
        );
    }

    #[test]
    fn reserved_prefixes_classify_but_never_reject() {
        assert_eq!(
            Context::facet("waiting-on:carlos"),
            ContextFacet::WaitingOn("carlos")
        );
        assert_eq!(
            Context::facet("energy:high"),
            ContextFacet::Energy(Energy::High)
        );
        assert_eq!(
            Context::facet("energy:med"),
            ContextFacet::Energy(Energy::Med)
        );
        assert_eq!(Context::facet("errands"), ContextFacet::Plain);
        // Undefined energy levels stay uninterpreted rather than being guessed.
        assert_eq!(Context::facet("energy:nuclear"), ContextFacet::Plain);
        // A bare prefix names nobody.
        assert_eq!(Context::facet("waiting-on:"), ContextFacet::Plain);
        // ...and none of them are rejected: they are conventions, not types.
        for n in ["waiting-on:carlos", "energy:high", "energy:nuclear"] {
            ContextDraft {
                name: n.into(),
                description: None,
            }
            .validate()
            .unwrap();
        }
    }
}
