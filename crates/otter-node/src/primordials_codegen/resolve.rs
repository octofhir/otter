//! Compile-time primordial recipe selection from the current bootstrap AST.
//!
//! # Contents
//! - Ordered captured holder/special-name facts.
//! - Static name grammar matching over owned catalog strings.
//! - Complete ordered lookup stages, including prefix collisions.
//!
//! # Invariants
//! This module parses no JavaScript text. Recipes contain names only, never
//! runtime values. Every accepted name preserves the original lookup order;
//! missing members are still read live when its lazy cache entry is requested.
//!
//! # See also
//! - `emit` lowers these recipes into one compact static JS dispatcher.

use oxc_ast::ast::{
    BindingPattern, Expression, ObjectPropertyKind, Program, PropertyKey, Statement,
};
use oxc_span::GetSpan;

use super::{Result, reject};

pub(super) struct Facts {
    pub(super) constructors: Vec<String>,
    pub(super) namespaces: Vec<String>,
    explicit: Vec<String>,
    symbol_wells: Vec<String>,
    special: Vec<String>,
}

impl Facts {
    pub(super) fn read(program: &Program<'_>) -> Result<Self> {
        let mut objects = std::collections::BTreeMap::new();
        for statement in &program.body {
            let Statement::VariableDeclaration(declaration) = statement else {
                continue;
            };
            for binding in &declaration.declarations {
                let BindingPattern::BindingIdentifier(identifier) = &binding.id else {
                    continue;
                };
                if !matches!(
                    identifier.name.as_str(),
                    "constructors" | "namespaces" | "explicit" | "symbolWells"
                ) {
                    continue;
                }
                let Some(Expression::ObjectExpression(object)) = &binding.init else {
                    return Err(reject(
                        "bootstrap",
                        binding.span,
                        "holder catalog must be a current object literal",
                    ));
                };
                let mut keys = Vec::new();
                for entry in &object.properties {
                    let ObjectPropertyKind::ObjectProperty(property) = entry else {
                        return Err(reject(
                            "bootstrap",
                            entry.span(),
                            "holder spread has no static ordered key proof",
                        ));
                    };
                    let key = match &property.key {
                        PropertyKey::StaticIdentifier(key) if !property.computed => {
                            key.name.to_string()
                        }
                        PropertyKey::StringLiteral(key) => key.value.to_string(),
                        _ => {
                            return Err(reject(
                                "bootstrap",
                                property.key.span(),
                                "holder key is not static",
                            ));
                        }
                    };
                    if keys.contains(&key) {
                        return Err(reject("bootstrap", property.span, "duplicate holder key"));
                    }
                    keys.push(key);
                }
                if objects.insert(identifier.name.to_string(), keys).is_some() {
                    return Err(reject(
                        "bootstrap",
                        identifier.span,
                        "duplicate holder authority",
                    ));
                }
            }
        }
        let constructors = objects
            .remove("constructors")
            .ok_or("missing constructor authority")?;
        let namespaces = objects
            .remove("namespaces")
            .ok_or("missing namespace authority")?;
        let explicit = objects
            .remove("explicit")
            .ok_or("missing explicit authority")?;
        let symbol_wells = objects
            .remove("symbolWells")
            .ok_or("missing symbol authority")?;
        let mut special = explicit.clone();
        special.extend(symbol_wells.iter().cloned());
        special.extend(constructors.iter().cloned());
        special.extend(namespaces.iter().cloned());
        special.sort();
        special.dedup();
        Ok(Self {
            constructors,
            namespaces,
            explicit,
            symbol_wells,
            special,
        })
    }

    pub(super) fn special_names(&self) -> Vec<String> {
        self.special.clone()
    }

    /// The holder table a name is read from: the first, in `explicit`,
    /// `symbolWells`, `constructors`, `namespaces` order, whose static keys
    /// hold it.
    pub(super) fn holder_of(&self, name: &str) -> Option<&'static str> {
        [
            ("explicit", &self.explicit),
            ("symbolWells", &self.symbol_wells),
            ("constructors", &self.constructors),
            ("namespaces", &self.namespaces),
        ]
        .into_iter()
        .find(|(_, keys)| keys.iter().any(|key| key == name))
        .map(|(holder, _)| holder)
    }

    pub(super) fn typed_arrays(&self) -> Vec<String> {
        self.constructors
            .iter()
            .filter(|name| {
                name.ends_with("Array")
                    && (name.starts_with("Int")
                        || name.starts_with("Uint")
                        || name.starts_with("Float")
                        || name.starts_with("BigInt")
                        || name.starts_with("BigUint"))
            })
            .cloned()
            .collect()
    }

    pub(super) fn case(&self, name: &str) -> Case {
        let mut steps = Vec::new();
        if let Some(base) = name
            .strip_suffix("Prototype")
            .filter(|base| base_word(base))
        {
            steps.push(Step::PrototypeObject {
                base: base.to_owned(),
            });
        }
        if let Some((base, property, half)) = prototype_candidates(name).find_map(|(base, rest)| {
            rest.strip_prefix("Get")
                .map(|property| ("get", property))
                .or_else(|| rest.strip_prefix("Set").map(|property| ("set", property)))
                .filter(|(_, property)| member_word(property))
                .map(|(half, property)| (base, property, half))
        }) {
            steps.push(Step::PrototypeHalf {
                base: base.to_owned(),
                property: property.to_owned(),
                half,
            });
        }
        if let Some((base, property)) = prototype_candidates(name).find_map(|(base, rest)| {
            rest.strip_suffix("Apply")
                .filter(|property| member_word(property))
                .map(|property| (base, property))
        }) {
            steps.push(Step::PrototypeApply {
                base: base.to_owned(),
                property: property.to_owned(),
            });
        }
        if let Some(stem) = name.strip_suffix("Apply") {
            for (authority, base) in self.holders() {
                if let Some(rest) = stem
                    .strip_prefix(base)
                    .filter(|rest| member_word_or_underscore(rest))
                {
                    steps.push(Step::StaticApply {
                        authority,
                        base: base.to_owned(),
                        property: rest.to_owned(),
                    });
                }
            }
        }
        if let Some((base, property)) = prototype_candidates(name).next() {
            steps.push(Step::PrototypeMethod {
                base: base.to_owned(),
                property: property.to_owned(),
            });
        }
        if base_and_member(name) {
            for (authority, base) in self.holders() {
                if let Some(rest) = name
                    .strip_prefix(base)
                    .filter(|rest| member_word_or_underscore(rest))
                {
                    steps.push(Step::Static {
                        authority,
                        base: base.to_owned(),
                        property: rest.to_owned(),
                    });
                }
            }
        }
        Case {
            name: name.to_owned(),
            steps,
        }
    }

    fn holders(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.namespaces
            .iter()
            .map(|name| ("namespaces", name.as_str()))
            .chain(
                self.constructors
                    .iter()
                    .map(|name| ("constructors", name.as_str())),
            )
    }
}

fn base_word(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_uppercase()) && bytes.all(|b| b.is_ascii_alphanumeric())
}

fn member_word(value: &str) -> bool {
    base_word(value)
}

fn member_word_or_underscore(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_uppercase())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn prototype_candidates(name: &str) -> impl Iterator<Item = (&str, &str)> {
    // Match the original lazy BasePrototype boundary, not a greedy split.
    name.match_indices("Prototype")
        .filter_map(move |(position, _)| {
            let (base, rest) = (&name[..position], &name[position + "Prototype".len()..]);
            (base_word(base) && member_word(rest)).then_some((base, rest))
        })
}

fn base_and_member(name: &str) -> bool {
    (1..name.len())
        .filter(|&i| name.is_char_boundary(i))
        .any(|i| base_word(&name[..i]) && member_word_or_underscore(&name[i..]))
}

pub(super) struct Case {
    pub(super) name: String,
    pub(super) steps: Vec<Step>,
}

pub(super) enum Step {
    PrototypeObject {
        base: String,
    },
    PrototypeHalf {
        base: String,
        property: String,
        half: &'static str,
    },
    PrototypeApply {
        base: String,
        property: String,
    },
    StaticApply {
        authority: &'static str,
        base: String,
        property: String,
    },
    PrototypeMethod {
        base: String,
        property: String,
    },
    Static {
        authority: &'static str,
        base: String,
        property: String,
    },
}

impl Case {
    pub(super) fn record(&self) -> serde_json::Value {
        serde_json::json!({"name":self.name,"orderedSteps":self.steps.iter().map(|step| match step {
            Step::PrototypeObject{base} => serde_json::json!({"kind":"prototypeObject","base":base}),
            Step::PrototypeHalf{base,property,half} => serde_json::json!({"kind":"prototypeHalf","base":base,"property":property,"half":half}),
            Step::PrototypeApply{base,property} => serde_json::json!({"kind":"prototypeApply","base":base,"property":property}),
            Step::StaticApply{authority,base,property} => serde_json::json!({"kind":"staticApply","authority":authority,"base":base,"property":property}),
            Step::PrototypeMethod{base,property} => serde_json::json!({"kind":"prototypeMethod","base":base,"property":property}),
            Step::Static{authority,base,property} => serde_json::json!({"kind":"static","authority":authority,"base":base,"property":property}),
        }).collect::<Vec<_>>()})
    }
}
