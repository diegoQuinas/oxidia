//! XML item-to-script registry for the game actor.
//!
//! Parses `actions.xml` / `movements.xml` at runtime into an `HashMap<u16, String>`
//! mapping `item_id → "module.function"`. Designed as a pure-data module with no
//! Lua dependency so it is independently testable.

use std::collections::HashMap;

use formats::FormatError;

/// Maps `item_id` → `"module.function"` parsed from `actions.xml`.
#[derive(Debug, Clone, Default)]
pub struct XmlRegistry(HashMap<u16, String>);

/// Production entry points wired in PR 2 (Phase 3); suppressed until then.
#[allow(dead_code)]
impl XmlRegistry {
    /// Parse an `<actions>` document and build the item-to-script mapping.
    ///
    /// Each `<action itemid="N" script="module.fn"/>` entry registers a callback
    /// for that item id. Malformed XML or entries missing required attributes
    /// return `Err(FormatError)`.
    pub fn from_actions_xml(xml: &str) -> Result<Self, FormatError> {
        let doc = roxmltree::Document::parse(xml).map_err(|_| FormatError::InvalidNode {
            what: "actions.xml is not well-formed",
        })?;
        let mut map: HashMap<u16, String> = HashMap::new();
        for action in doc.descendants().filter(|n| n.has_tag_name("action")) {
            let item_id: u16 = action
                .attribute("itemid")
                .and_then(|s| s.parse().ok())
                .ok_or(FormatError::InvalidNode {
                    what: "action missing valid itemid attribute",
                })?;
            let script = action
                .attribute("script")
                .ok_or(FormatError::InvalidNode {
                    what: "action missing script attribute",
                })?;
            map.insert(item_id, script.to_string());
        }
        Ok(XmlRegistry(map))
    }

    /// Look up the script name for a given item id.
    ///
    /// Returns `None` when the item is not registered.
    pub fn lookup(&self, item_id: u16) -> Option<&str> {
        self.0.get(&item_id).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::XmlRegistry;
    // ------------------------------------------------------------------
    // Helper: reference actions XML
    // ------------------------------------------------------------------

    fn sample_actions_xml() -> &'static str {
        r#"<actions>
          <action itemid="1386" script="teleport.onUse"/>
          <action itemid="1280" script="teleport.onUse"/>
          <action itemid="8592" script="teleport.onUse"/>
        </actions>"#
    }

    // ------------------------------------------------------------------
    // GREEN 1: valid XML → lookup returns the script for a registered item
    // ------------------------------------------------------------------

    #[test]
    fn lookup_returns_script_for_registered_item() {
        let registry = XmlRegistry::from_actions_xml(sample_actions_xml())
            .expect("valid XML must parse successfully");
        assert_eq!(
            registry.lookup(1386),
            Some("teleport.onUse"),
            "item 1386 (ladder) must map to teleport.onUse"
        );
        assert_eq!(
            registry.lookup(1280),
            Some("teleport.onUse"),
            "item 1280 (hole) must map to teleport.onUse"
        );
    }

    // ------------------------------------------------------------------
    // GREEN 2: unregistered item → lookup returns None
    // ------------------------------------------------------------------

    #[test]
    fn lookup_returns_none_for_unregistered_item() {
        let registry = XmlRegistry::from_actions_xml(sample_actions_xml())
            .expect("valid XML must parse successfully");
        assert_eq!(
            registry.lookup(9999),
            None,
            "unregistered item must return None"
        );
    }

    // ------------------------------------------------------------------
    // GREEN 3: malformed XML → from_actions_xml returns Err
    // ------------------------------------------------------------------

    #[test]
    fn from_actions_xml_returns_err_on_malformed_xml() {
        let result = XmlRegistry::from_actions_xml("this is not valid xml <<<>>>");
        assert!(
            result.is_err(),
            "malformed XML must return Err(FormatError); got {:?}",
            result
        );
    }

    // ------------------------------------------------------------------
    // TRIANGULATE 4: empty document (<actions/>) → Ok(empty registry)
    // ------------------------------------------------------------------

    #[test]
    fn empty_actions_xml_produces_empty_registry() {
        let registry = XmlRegistry::from_actions_xml("<actions/>")
            .expect("empty <actions/> must parse successfully");
        assert_eq!(
            registry.lookup(1386),
            None,
            "empty registry must return None for any lookup"
        );
    }

    // ------------------------------------------------------------------
    // TRIANGULATE 5: action without itemid → Err
    // ------------------------------------------------------------------

    #[test]
    fn action_without_itemid_is_rejected() {
        let result = XmlRegistry::from_actions_xml(
            r#"<actions>
              <action script="noop.onUse"/>
            </actions>"#,
        );
        assert!(
            result.is_err(),
            "action without itemid must return Err; got {:?}",
            result
        );
    }

    // ------------------------------------------------------------------
    // TRIANGULATE 6: action without script → Err
    // ------------------------------------------------------------------

    #[test]
    fn action_without_script_is_rejected() {
        let result = XmlRegistry::from_actions_xml(
            r#"<actions>
              <action itemid="1386"/>
            </actions>"#,
        );
        assert!(
            result.is_err(),
            "action without script attribute must return Err; got {:?}",
            result
        );
    }
}
