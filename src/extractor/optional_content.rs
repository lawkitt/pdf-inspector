//! Optional content (layers) the document's default configuration turns
//! off: content no reader shows until someone turns its layer on.
//!
//! The default configuration is `/OCProperties /D` in the catalog. A group
//! (`/OCG`) is off when `/D /OFF` lists it, or when `/D /BaseState` is
//! `/OFF` and `/D /ON` does not list it. A membership dictionary (`/OCMD`)
//! is off when its visibility expression (`/VE`) is false, or, without a
//! readable one, when its visibility policy (`/P`, `/AnyOn` by default)
//! over its groups (`/OCGs`) is not met. Usage (`/AS`, print or view only)
//! is not read: the default configuration alone decides. A document
//! without `/OCProperties` has no optional content and turns nothing off.

use std::collections::HashSet;

use lopdf::{Dictionary, Document, Object, ObjectId};

/// Deepest nesting of a visibility expression evaluated; a deeper one is
/// not read and the membership dictionary falls back to its policy.
const MAX_EXPRESSION_DEPTH: usize = 16;

/// Most references followed to resolve one object.
const MAX_REFERENCE_CHAIN: usize = 8;

/// The groups the default configuration turns off.
#[derive(Clone, Debug, Default)]
pub(crate) enum OffLayers {
    /// No optional content: nothing is off.
    #[default]
    None,
    /// `/BaseState /ON` (or `/Unchanged`): the groups `/OFF` lists are off.
    Listed(HashSet<ObjectId>),
    /// `/BaseState /OFF`: every group is off but those `/ON` lists.
    AllBut(HashSet<ObjectId>),
}

impl OffLayers {
    /// The default configuration of `doc`.
    pub(crate) fn read(doc: &Document) -> OffLayers {
        let Some(config) = doc
            .catalog()
            .ok()
            .and_then(|catalog| catalog.get(b"OCProperties").ok())
            .and_then(|o| dict(doc, o))
            .and_then(|properties| properties.get(b"D").ok())
            .and_then(|o| dict(doc, o))
        else {
            return OffLayers::None;
        };
        let groups = |key: &[u8]| -> HashSet<ObjectId> {
            config
                .get(key)
                .ok()
                .and_then(|o| resolve(doc, o))
                .and_then(|o| o.as_array().ok())
                .map(|array| array.iter().filter_map(|o| o.as_reference().ok()).collect())
                .unwrap_or_default()
        };
        let base_off = config
            .get(b"BaseState")
            .ok()
            .and_then(|o| resolve(doc, o))
            .and_then(|o| o.as_name().ok())
            == Some(b"OFF");
        if base_off {
            OffLayers::AllBut(groups(b"ON"))
        } else {
            let off = groups(b"OFF");
            if off.is_empty() {
                OffLayers::None
            } else {
                OffLayers::Listed(off)
            }
        }
    }

    /// Whether nothing is off.
    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, OffLayers::None)
    }

    /// Whether content whose optional content entry (`/OC`, or the
    /// properties of a `/OC` marked-content sequence) is `oc` is off: `oc`
    /// is a group or a membership dictionary, direct or by reference.
    /// Anything unreadable is on.
    pub(crate) fn hides(&self, doc: &Document, oc: &Object) -> bool {
        if self.is_empty() {
            return false;
        }
        let id = oc.as_reference().ok();
        let Some(entry) = dict(doc, oc) else {
            return false;
        };
        let kind = entry
            .get(b"Type")
            .ok()
            .and_then(|o| resolve(doc, o))
            .and_then(|o| o.as_name().ok());
        let membership = match kind {
            Some(b"OCMD") => true,
            Some(b"OCG") => false,
            _ => entry.has(b"OCGs") || entry.has(b"VE"),
        };
        if membership {
            !self.membership_visible(doc, entry)
        } else {
            id.is_some_and(|id| self.group_off(id))
        }
    }

    fn group_off(&self, id: ObjectId) -> bool {
        match self {
            OffLayers::None => false,
            OffLayers::Listed(off) => off.contains(&id),
            OffLayers::AllBut(on) => !on.contains(&id),
        }
    }

    /// Whether a membership dictionary leaves its content visible.
    fn membership_visible(&self, doc: &Document, membership: &Dictionary) -> bool {
        if let Some(visible) = membership
            .get(b"VE")
            .ok()
            .and_then(|expression| self.expression(doc, expression, 0))
        {
            return visible;
        }
        let groups: Vec<ObjectId> = match membership.get(b"OCGs").ok() {
            Some(Object::Reference(id)) => match doc.get_object(*id) {
                Ok(Object::Array(array)) => {
                    array.iter().filter_map(|o| o.as_reference().ok()).collect()
                }
                Ok(_) => vec![*id],
                Err(_) => Vec::new(),
            },
            Some(Object::Array(array)) => {
                array.iter().filter_map(|o| o.as_reference().ok()).collect()
            }
            _ => Vec::new(),
        };
        // No groups: the dictionary has no effect.
        if groups.is_empty() {
            return true;
        }
        let on = |id: &ObjectId| !self.group_off(*id);
        match membership
            .get(b"P")
            .ok()
            .and_then(|o| resolve(doc, o))
            .and_then(|o| o.as_name().ok())
        {
            Some(b"AllOn") => groups.iter().all(on),
            Some(b"AnyOff") => !groups.iter().all(on),
            Some(b"AllOff") => !groups.iter().any(on),
            _ => groups.iter().any(on),
        }
    }

    /// The value of a visibility expression: a group, or an array of
    /// `/And`, `/Or` or `/Not` and its operands. `None` when it cannot be
    /// read.
    fn expression(&self, doc: &Document, expression: &Object, depth: usize) -> Option<bool> {
        if depth > MAX_EXPRESSION_DEPTH {
            return None;
        }
        if let Object::Reference(id) = expression {
            match doc.get_object(*id).ok()? {
                Object::Dictionary(_) => return Some(!self.group_off(*id)),
                Object::Array(_) => {}
                _ => return None,
            }
        }
        let array = resolve(doc, expression)?.as_array().ok()?;
        let (operator, operands) = array.split_first()?;
        let mut values = operands
            .iter()
            .map(|operand| self.expression(doc, operand, depth + 1));
        match resolve(doc, operator)?.as_name().ok()? {
            b"Not" if operands.len() == 1 => values.next()?.map(|v| !v),
            b"And" if !operands.is_empty() => {
                values.try_fold(true, |all, value| value.map(|v| all && v))
            }
            b"Or" if !operands.is_empty() => {
                values.try_fold(false, |any, value| value.map(|v| any || v))
            }
            _ => None,
        }
    }
}

fn resolve<'a>(doc: &'a Document, mut obj: &'a Object) -> Option<&'a Object> {
    for _ in 0..MAX_REFERENCE_CHAIN {
        match obj {
            Object::Reference(id) => obj = doc.get_object(*id).ok()?,
            _ => return Some(obj),
        }
    }
    None
}

fn dict<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Dictionary> {
    resolve(doc, obj).and_then(|o| o.as_dict().ok())
}

/// Whether the dictionary `owner` (an XObject, an annotation) carries an
/// `/OC` entry the default configuration turns off.
pub(crate) fn owner_off(doc: &Document, layers: &OffLayers, owner: &Dictionary) -> bool {
    owner
        .get(b"OC")
        .ok()
        .is_some_and(|oc| layers.hides(doc, oc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    /// A document with groups `a`, `b` and `c` and the default
    /// configuration `config`, built from them.
    fn doc_with(config: impl FnOnce([ObjectId; 3]) -> Dictionary) -> (Document, [ObjectId; 3]) {
        let mut doc = Document::with_version("1.7");
        let groups = [b"A", b"B", b"C"].map(|name| {
            doc.add_object(dictionary! {
                "Type" => "OCG",
                "Name" => Object::string_literal(name.to_vec()),
            })
        });
        let config = config(groups);
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "OCProperties" => dictionary! {
                "OCGs" => groups.iter().map(|&id| Object::Reference(id)).collect::<Vec<_>>(),
                "D" => config,
            },
        });
        doc.trailer.set("Root", catalog);
        (doc, groups)
    }

    fn refs(ids: &[ObjectId]) -> Object {
        Object::Array(ids.iter().map(|&id| Object::Reference(id)).collect())
    }

    #[test]
    fn a_group_listed_off_is_off_and_the_others_are_on() {
        let (doc, [a, b, c]) =
            doc_with(|[a, b, _]| dictionary! { "ON" => refs(&[b]), "OFF" => refs(&[a]) });
        let layers = OffLayers::read(&doc);
        assert!(layers.hides(&doc, &Object::Reference(a)));
        assert!(!layers.hides(&doc, &Object::Reference(b)));
        assert!(!layers.hides(&doc, &Object::Reference(c)));
    }

    #[test]
    fn base_state_off_turns_off_every_group_not_listed_on() {
        let (doc, [a, b, c]) =
            doc_with(|[_, b, _]| dictionary! { "BaseState" => "OFF", "ON" => refs(&[b]) });
        let layers = OffLayers::read(&doc);
        assert!(layers.hides(&doc, &Object::Reference(a)));
        assert!(!layers.hides(&doc, &Object::Reference(b)));
        assert!(layers.hides(&doc, &Object::Reference(c)));
    }

    #[test]
    fn without_optional_content_properties_nothing_is_off() {
        let mut doc = Document::with_version("1.7");
        let group = doc.add_object(dictionary! { "Type" => "OCG" });
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog" });
        doc.trailer.set("Root", catalog);
        let layers = OffLayers::read(&doc);
        assert!(layers.is_empty());
        assert!(!layers.hides(&doc, &Object::Reference(group)));
    }

    #[test]
    fn a_membership_dictionary_follows_its_policy() {
        let (doc, [a, b, _]) = doc_with(|[a, _, _]| dictionary! { "OFF" => refs(&[a]) });
        let layers = OffLayers::read(&doc);
        let membership = |policy: Option<&str>| {
            let mut d = dictionary! { "Type" => "OCMD", "OCGs" => refs(&[a, b]) };
            if let Some(policy) = policy {
                d.set("P", Object::Name(policy.as_bytes().to_vec()));
            }
            Object::Dictionary(d)
        };
        // A is off, B is on.
        assert!(!layers.hides(&doc, &membership(None)));
        assert!(!layers.hides(&doc, &membership(Some("AnyOn"))));
        assert!(layers.hides(&doc, &membership(Some("AllOn"))));
        assert!(!layers.hides(&doc, &membership(Some("AnyOff"))));
        assert!(layers.hides(&doc, &membership(Some("AllOff"))));
        // A single group, not in an array.
        let single =
            Object::Dictionary(dictionary! { "Type" => "OCMD", "OCGs" => Object::Reference(a) });
        assert!(layers.hides(&doc, &single));
        // No groups: no effect.
        let empty =
            Object::Dictionary(dictionary! { "Type" => "OCMD", "OCGs" => Object::Array(vec![]) });
        assert!(!layers.hides(&doc, &empty));
    }

    #[test]
    fn a_visibility_expression_overrides_the_policy() {
        let (doc, [a, b, c]) = doc_with(|[a, _, _]| dictionary! { "OFF" => refs(&[a]) });
        let layers = OffLayers::read(&doc);
        let with_expression = |expression: Object| {
            Object::Dictionary(dictionary! {
                "Type" => "OCMD",
                "OCGs" => refs(&[b]),
                "VE" => expression,
            })
        };
        let r = Object::Reference;
        // A is off; B and C are on.
        let and = Object::Array(vec!["And".into(), r(a), r(b)]);
        assert!(layers.hides(&doc, &with_expression(and)));
        let or = Object::Array(vec!["Or".into(), r(a), r(b)]);
        assert!(!layers.hides(&doc, &with_expression(or)));
        let not = Object::Array(vec!["Not".into(), r(c)]);
        assert!(layers.hides(&doc, &with_expression(not)));
        let nested = Object::Array(vec![
            "And".into(),
            r(b),
            Object::Array(vec!["Not".into(), r(a)]),
        ]);
        assert!(!layers.hides(&doc, &with_expression(nested)));
        // An expression that cannot be read falls back to the policy over
        // the groups: B is on.
        let unreadable = Object::Array(vec!["Xor".into(), r(a)]);
        assert!(!layers.hides(&doc, &with_expression(unreadable)));
    }

    #[test]
    fn a_runaway_expression_is_not_read() {
        let (doc, [a, _, _]) = doc_with(|[a, _, _]| dictionary! { "OFF" => refs(&[a]) });
        let layers = OffLayers::read(&doc);
        let mut expression = Object::Reference(a);
        for _ in 0..=MAX_EXPRESSION_DEPTH + 1 {
            expression = Object::Array(vec![
                "Not".into(),
                Object::Array(vec!["Not".into(), expression]),
            ]);
        }
        // Too deep to read: the policy over no groups decides, which has no
        // effect.
        let membership = Object::Dictionary(dictionary! { "Type" => "OCMD", "VE" => expression });
        assert!(!layers.hides(&doc, &membership));
    }
}
