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
            let script = action.attribute("script").ok_or(FormatError::InvalidNode {
                what: "action missing script attribute",
            })?;

            // Support both single-itemid and fromid/toid range syntax.
            if let Some(item_id_str) = action.attribute("itemid") {
                // Single item: <action itemid="N" script="X"/>
                let item_id: u16 = item_id_str.parse().map_err(|_| FormatError::InvalidNode {
                    what: "action has invalid itemid attribute",
                })?;
                map.insert(item_id, script.to_string());
            } else if let (Some(from_str), Some(to_str)) =
                (action.attribute("fromid"), action.attribute("toid"))
            {
                // Range: <action fromid="A" toid="B" script="X"/>
                let from_id: u16 = from_str.parse().map_err(|_| FormatError::InvalidNode {
                    what: "action has invalid fromid attribute",
                })?;
                let to_id: u16 = to_str.parse().map_err(|_| FormatError::InvalidNode {
                    what: "action has invalid toid attribute",
                })?;
                if from_id <= to_id {
                    for id in from_id..=to_id {
                        map.insert(id, script.to_string());
                    }
                } else {
                    // Inverted range — log warning and skip.
                    tracing::warn!(
                        from = from_id,
                        to = to_id,
                        script = script,
                        "fromid > toid in action range, skipping"
                    );
                }
            } else {
                return Err(FormatError::InvalidNode {
                    what: "action must have itemid or (fromid + toid) attributes",
                });
            }
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

    fn range_actions_xml() -> &'static str {
        r#"<actions>
          <action itemid="1386" script="teleport.onUse"/>
          <action fromid="2666" toid="2691" script="food.onUse"/>
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

    // ------------------------------------------------------------------
    // PHASE 1: fromid / toid range expansion
    // ------------------------------------------------------------------

    #[test]
    fn fromid_toid_range_expands_all_items_in_range() {
        // RED: fromid/toid range must expand to individual entries so
        // every item in the range is registered with the script.
        let registry = XmlRegistry::from_actions_xml(range_actions_xml())
            .expect("valid XML with fromid/toid must parse successfully");
        // First item in range
        assert_eq!(
            registry.lookup(2666),
            Some("food.onUse"),
            "item 2666 (start of range) must map to food.onUse"
        );
        // Middle of range
        assert_eq!(
            registry.lookup(2680),
            Some("food.onUse"),
            "item 2680 (middle of range) must map to food.onUse"
        );
        // Last item in range
        assert_eq!(
            registry.lookup(2691),
            Some("food.onUse"),
            "item 2691 (end of range) must map to food.onUse"
        );
        // Single-itemid entry still works alongside ranges
        assert_eq!(
            registry.lookup(1386),
            Some("teleport.onUse"),
            "single-itemid entry (1386) must still work alongside ranges"
        );
    }

    #[test]
    fn fromid_toid_single_item_equal_bounds() {
        // fromid == toid should register exactly one item.
        let registry = XmlRegistry::from_actions_xml(
            r#"<actions>
              <action fromid="2666" toid="2666" script="food.onUse"/>
            </actions>"#,
        )
        .expect("fromid==toid must parse successfully");
        assert_eq!(
            registry.lookup(2666),
            Some("food.onUse"),
            "single-item range must register the item"
        );
    }

    #[test]
    fn unregistered_item_outside_range_returns_none() {
        // Item not in any range or single-itemid entry must return None.
        let registry = XmlRegistry::from_actions_xml(range_actions_xml())
            .expect("valid XML with fromid/toid must parse successfully");
        assert_eq!(
            registry.lookup(3000),
            None,
            "item 3000 outside range must return None"
        );
        assert_eq!(
            registry.lookup(1),
            None,
            "item 1 (outside range) must return None"
        );
    }

    #[test]
    fn fromid_without_toid_is_rejected() {
        // fromid without toid is invalid
        let result = XmlRegistry::from_actions_xml(
            r#"<actions>
              <action fromid="2666" script="food.onUse"/>
            </actions>"#,
        );
        assert!(
            result.is_err(),
            "action with fromid but no toid must return Err; got {:?}",
            result
        );
    }

    #[test]
    fn toid_without_fromid_is_rejected() {
        // toid without fromid is invalid
        let result = XmlRegistry::from_actions_xml(
            r#"<actions>
              <action toid="2691" script="food.onUse"/>
            </actions>"#,
        );
        assert!(
            result.is_err(),
            "action with toid but no fromid must return Err; got {:?}",
            result
        );
    }

    #[test]
    fn fromid_greater_than_toid_is_empty_and_logged() {
        // When fromid > toid, the range is empty.
        let registry = XmlRegistry::from_actions_xml(
            r#"<actions>
              <action fromid="2691" toid="2666" script="food.onUse"/>
            </actions>"#,
        )
        .expect("inverted range must parse without error (empty expansion)");
        assert_eq!(
            registry.lookup(2666),
            None,
            "item 2666 must not be registered from inverted range"
        );
        assert_eq!(
            registry.lookup(2691),
            None,
            "item 2691 must not be registered from inverted range"
        );
    }
}
