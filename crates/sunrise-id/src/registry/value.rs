//! Value types: the spec types [`crate::describe_value_types!`] fills, and the
//! macro itself, for the types a record field or an op payload carries without
//! being a registered record.

/// One value type: a type a record field or an op payload carries that is not
/// itself a registered record, described by [`describe_value_types!`].
///
/// [`describe_value_types!`]: crate::describe_value_types
#[derive(Debug, Clone, Copy)]
pub struct ValueSpec {
    /// The Rust type name.
    pub name: &'static str,
    /// Its wire shape.
    pub shape: ValueShape,
}

/// How a value type is laid out on the wire.
#[derive(Debug, Clone, Copy)]
pub enum ValueShape {
    /// A map of named fields.
    Record {
        /// Its fields, in declaration order.
        fields: &'static [ValueField],
        /// Whether it keeps the fields this build does not know.
        keeps_unknowns: bool,
    },
    /// Encoded exactly as the type it names: a transparent newtype, or a type
    /// whose hand-written serde writes that type.
    Alias(&'static str),
    /// A fixed-length array of positional items, in order.
    Tuple(&'static [&'static str]),
    /// One of a closed set of variants.
    Variants {
        /// Where the variant's name goes.
        tagging: Tagging,
        /// The variants, in declaration order.
        variants: &'static [VariantSpec],
        /// Whether an unknown variant is kept rather than refused.
        keeps_unknowns: bool,
    },
}

/// Where a [`ValueShape::Variants`] value writes its variant's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tagging {
    /// serde's external tagging: a unit variant is its name as a string, any
    /// other is a one-entry map from its name to its content.
    External,
    /// One map whose named key holds the variant's name beside its fields.
    Internal(&'static str),
}

/// One variant of a [`ValueShape::Variants`] type.
#[derive(Debug, Clone, Copy)]
pub struct VariantSpec {
    /// The name on the wire.
    pub name: &'static str,
    /// The Rust variant name.
    pub rust: &'static str,
    /// What it carries.
    pub shape: VariantShape,
}

/// What one variant carries.
#[derive(Debug, Clone, Copy)]
pub enum VariantShape {
    /// Nothing.
    Unit,
    /// One value of the named type.
    Newtype(&'static str),
    /// Named fields.
    Fields(&'static [ValueField]),
}

/// One field of a value type. It has no CRDT type: a value type is written as
/// a unit inside the field that holds it.
#[derive(Debug, Clone, Copy)]
pub struct ValueField {
    /// The CBOR map key.
    pub name: &'static str,
    /// The Rust field name.
    pub rust: &'static str,
    /// The value type, as written in the declaration.
    pub value_type: &'static str,
}

/// Describe the value types a crate puts on the wire, and check every
/// description against the type it names.
///
/// Expands to a `static` slice of [`ValueSpec`] and, beside it, one function
/// per type that the compiler checks and nothing calls:
///
/// | Section | Wire shape | Fails the build when |
/// |---|---|---|
/// | `records` | a map of named fields | a field is added, removed, or retyped (an exhaustive destructuring with no `..`) |
/// | `newtypes` | its one field, transparently | the field's type changes (the constructor coerces to `fn(T) -> Self`) |
/// | `aliases` | the type named | never: its serde is hand-written, so a wire test pins it |
/// | `tuples` | an array of its fields, in order | an item is added, removed, or retyped (the constructor coerces) |
/// | `variants` | one of its variants | a variant is added or removed (an exhaustive `match`), or a variant's fields or payload change |
///
/// ```text
/// describe_value_types! {
///     pub static NAME;
///     records: [ Rec { field: Type; renamed as "wire": Type; ..unknown } ],
///     newtypes: [ Body(Vec<u8>); ],
///     aliases: [ Set = Vec<Day>; ],
///     tuples: [ Entry([u8; 16], u64); ],
///     variants: [
///         Time "kind" {                      // internally tagged; or `external`
///             Instant as "instant" = { at: Timestamp; };
///             ..Unknown                      // an arm that keeps an unknown variant
///         }
///         Who external { Device = ([u8; 16]); Nobody = unit; }
///     ],
/// }
/// ```
///
/// Every type named must be in scope at the call site, and so must
/// `Unknowns` where a record keeps unknowns. Wire names are checked against
/// what serde writes by each caller's tests, as the entity registry's are.
#[macro_export]
macro_rules! describe_value_types {
    (
        $(#[$meta:meta])*
        $vis:vis static $name:ident;
        records: [
            $(
                $record:ident {
                    $( $field:ident $(as $wire:literal)?: $field_ty:ty; )*
                    $(..$unknown:ident)?
                }
            )*
        ],
        newtypes: [ $( $newtype:ident($newtype_inner:ty); )* ],
        aliases: [ $( $alias:ident = $alias_ty:ty; )* ],
        tuples: [ $( $tuple:ident( $($item:ty),* $(,)? ); )* ],
        variants: [
            $(
                $enum_ty:ident $tagging:tt {
                    $( $variant:ident $(as $variant_wire:literal)? = $variant_body:tt; )*
                    $(..$unknown_variant:ident)?
                }
            )*
        ],
    ) => {
        $(#[$meta])*
        $vis static $name: &[$crate::registry::ValueSpec] = &[
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($record),
                    shape: $crate::registry::ValueShape::Record {
                        fields: &[
                            $(
                                $crate::registry::ValueField {
                                    name: $crate::__registry_wire_name!($field $($wire)?),
                                    rust: ::core::stringify!($field),
                                    value_type: ::core::stringify!($field_ty),
                                },
                            )*
                        ],
                        keeps_unknowns: $crate::__registry_present!($($unknown)?),
                    },
                },
            )*
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($newtype),
                    shape: $crate::registry::ValueShape::Alias(::core::stringify!($newtype_inner)),
                },
            )*
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($alias),
                    shape: $crate::registry::ValueShape::Alias(::core::stringify!($alias_ty)),
                },
            )*
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($tuple),
                    shape: $crate::registry::ValueShape::Tuple(&[$(::core::stringify!($item)),*]),
                },
            )*
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($enum_ty),
                    shape: $crate::registry::ValueShape::Variants {
                        tagging: $crate::__value_tagging!($tagging),
                        variants: &[
                            $(
                                $crate::registry::VariantSpec {
                                    name: $crate::__registry_wire_name!($variant $($variant_wire)?),
                                    rust: ::core::stringify!($variant),
                                    shape: $crate::__value_variant_shape!($variant_body),
                                },
                            )*
                        ],
                        keeps_unknowns: $crate::__registry_present!($($unknown_variant)?),
                    },
                },
            )*
        ];

        $(
            const _: () = {
                #[allow(dead_code, clippy::no_effect_underscore_binding)]
                fn described_fields_are_the_type_fields(value: &$record) {
                    let $record { $($field,)* $($unknown,)? } = value;
                    $( let _: &$field_ty = $field; )*
                    $( let _: &Unknowns = $unknown; )?
                }
            };
        )*
        $(
            const _: () = {
                #[allow(dead_code)]
                fn described_newtype_is_the_type() {
                    let _: fn($newtype_inner) -> $newtype = $newtype;
                }
            };
        )*
        $(
            const _: () = {
                #[allow(dead_code)]
                fn described_items_are_the_type_items() {
                    let _: fn($($item),*) -> $tuple = $tuple;
                }
            };
        )*
        $(
            const _: () = {
                #[allow(dead_code)]
                fn described_variants_are_the_type_variants(value: &$enum_ty) {
                    match value {
                        $( $enum_ty::$variant { .. } => {} )*
                        $( $enum_ty::$unknown_variant { .. } => {} )?
                    }
                    $( $crate::__value_variant_check!(value, $enum_ty, $variant, $variant_body); )*
                }
            };
        )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __registry_present {
    () => {
        false
    };
    ($name:ident) => {
        true
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __value_tagging {
    (external) => {
        $crate::registry::Tagging::External
    };
    ($tag:literal) => {
        $crate::registry::Tagging::Internal($tag)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __value_variant_shape {
    (unit) => {
        $crate::registry::VariantShape::Unit
    };
    (($ty:ty)) => {
        $crate::registry::VariantShape::Newtype(::core::stringify!($ty))
    };
    ({ $( $field:ident $(as $wire:literal)?: $field_ty:ty; )* }) => {
        $crate::registry::VariantShape::Fields(&[
            $(
                $crate::registry::ValueField {
                    name: $crate::__registry_wire_name!($field $($wire)?),
                    rust: ::core::stringify!($field),
                    value_type: ::core::stringify!($field_ty),
                },
            )*
        ])
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __value_variant_check {
    ($value:ident, $enum_ty:ident, $variant:ident, unit) => {
        let _: $enum_ty = $enum_ty::$variant;
    };
    ($value:ident, $enum_ty:ident, $variant:ident, ($ty:ty)) => {
        let _: fn($ty) -> $enum_ty = $enum_ty::$variant;
    };
    ($value:ident, $enum_ty:ident, $variant:ident, { $( $field:ident $(as $wire:literal)?: $field_ty:ty; )* }) => {
        if let $enum_ty::$variant { $($field,)* } = $value {
            $( let _: &$field_ty = $field; )*
        }
    };
}
