//! The registry's expansion in this crate: `define_entities!` turns
//! [`crate::for_each_entity!`] into [`EntityKind`] and [`ENTITIES`], with the
//! helper macros that read a field's storage, wire name, conflict class and
//! default.

use super::spec::{Crdt, EntitySpec, FieldSpec, Merge, OpClass, OpSpec, Owner, RecordSpec};

#[doc(hidden)]
#[macro_export]
macro_rules! __registry_storage {
    (none) => {
        ::core::option::Option::None
    };
    (($table:literal, $key:literal, $unknowns:ident)) => {
        ::core::option::Option::Some($crate::registry::Storage {
            table: $table,
            key: $key,
            unknowns: $crate::registry::Unknowns::$unknowns,
        })
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __registry_wire_name {
    ($field:ident) => {
        ::core::stringify!($field)
    };
    ($field:ident $wire:literal) => {
        $wire
    };
}

/// A field's [`Conflict`](crate::registry::Conflict), from its CRDT type and
/// the class it declares. Only a register declares one, and it must: see the
/// module docs §Grammar.
#[doc(hidden)]
#[macro_export]
macro_rules! __registry_conflict {
    ($field:ident; Register) => {
        ::core::compile_error!(::core::concat!(
            "register `",
            ::core::stringify!($field),
            "` declares no conflict class: write `=> Register(Quiet)` or another class (ADR-0065 §3)"
        ))
    };
    ($field:ident; Register Merge) => {
        ::core::compile_error!(::core::concat!(
            "register `",
            ::core::stringify!($field),
            "` cannot be Merge: a register keeps one value (ADR-0065 §3)"
        ))
    };
    ($field:ident; Register $class:ident) => {
        $crate::registry::Conflict::$class
    };
    ($field:ident; $crdt:ident) => {
        $crate::registry::Conflict::Merge
    };
    ($field:ident; $crdt:ident $class:ident) => {
        ::core::compile_error!(::core::concat!(
            "`",
            ::core::stringify!($field),
            "` is not a register and declares no conflict class: every write to it survives (ADR-0065 §3)"
        ))
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __registry_option_name {
    () => {
        ::core::option::Option::None
    };
    ($name:ident) => {
        ::core::option::Option::Some(::core::stringify!($name))
    };
}

/// A field's declared default, or `None`: see the module docs §Defaults.
#[doc(hidden)]
#[macro_export]
macro_rules! __registry_default {
    () => {
        ::core::option::Option::None
    };
    ($default:literal) => {
        ::core::option::Option::Some($default)
    };
}

/// Expands the registry into [`EntityKind`] and [`ENTITIES`].
macro_rules! define_entities {
    (
        $(
            $(#[$kind_meta:meta])*
            $kind:ident {
                prefix: $prefix:literal,
                tag: $tag:literal,
                merge: $merge:ident,
                owner: $owner:ident $(($owner_field:literal))?,
                features: [$($feature:literal),* $(,)?],
                ops: [
                    $(
                        $(#[$op_meta:meta])*
                        $op:ident($payload:ty) = $inner_kind:literal, $class:ident, $target:ident;
                    )*
                ],
                records: [
                    $(
                        $record:ident @ $storage:tt {
                            $(
                                $field:ident $(as $wire:literal)?: $field_ty:ty => $crdt:ident $(($conflict:ident))? $(= $default:literal)?;
                            )*
                            $(..$unknown:ident)?
                        }
                    )*
                ],
            }
        )*
    ) => {
        /// Domain entity kinds, generated from [`for_each_entity!`](crate::for_each_entity).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "lowercase")]
        #[non_exhaustive]
        pub enum EntityKind {
            $( $(#[$kind_meta])* $kind, )*
        }

        impl EntityKind {
            /// How many kinds the registry declares.
            pub const COUNT: usize = [$(::core::stringify!($kind)),*].len();

            /// Canonical 4-char prefix (3 letters + `_`).
            #[must_use]
            pub const fn prefix(self) -> &'static str {
                match self {
                    $( Self::$kind => $prefix, )*
                }
            }

            /// Look up an entity kind from its 4-char prefix (including the `_`).
            /// Returns `None` for unknown prefixes.
            #[must_use]
            pub fn from_prefix(prefix: &str) -> Option<Self> {
                match prefix {
                    $( $prefix => Some(Self::$kind), )*
                    _ => None,
                }
            }

            /// The op-log `target_kind` and schema name, e.g. `"focus_session"`.
            #[must_use]
            pub const fn tag(self) -> &'static str {
                match self {
                    $( Self::$kind => $tag, )*
                }
            }

            /// All kinds, in declaration order.
            #[must_use]
            pub const fn all() -> [Self; Self::COUNT] {
                [ $( Self::$kind, )* ]
            }

            /// This kind's registry entry.
            #[must_use]
            pub fn spec(self) -> &'static $crate::registry::EntitySpec {
                &$crate::registry::ENTITIES[self as usize]
            }
        }

        /// Every registered entity, in [`EntityKind`] declaration order.
        pub static ENTITIES: [EntitySpec; EntityKind::COUNT] = [
            $(
                EntitySpec {
                    kind: EntityKind::$kind,
                    prefix: $prefix,
                    tag: $tag,
                    merge: Merge::$merge,
                    owner: Owner::$owner $(($owner_field))?,
                    features: &[$($feature),*],
                    ops: &[
                        $(
                            OpSpec {
                                variant: ::core::stringify!($op),
                                payload: ::core::stringify!($payload),
                                inner_kind: $inner_kind,
                                class: OpClass::$class,
                                target: ::core::stringify!($target),
                            },
                        )*
                    ],
                    records: &[
                        $(
                            RecordSpec {
                                name: ::core::stringify!($record),
                                storage: $crate::__registry_storage!($storage),
                                fields: &[
                                    $(
                                        FieldSpec {
                                            name: $crate::__registry_wire_name!($field $($wire)?),
                                            rust: ::core::stringify!($field),
                                            value_type: ::core::stringify!($field_ty),
                                            crdt: Crdt::$crdt,
                                            conflict: $crate::__registry_conflict!($field; $crdt $($conflict)?),
                                            default: $crate::__registry_default!($($default)?),
                                        },
                                    )*
                                ],
                                unknowns: $crate::__registry_option_name!($($unknown)?),
                            },
                        )*
                    ],
                },
            )*
        ];
    };
}

crate::for_each_entity!(define_entities);
