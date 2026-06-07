//! Gendered outfit catalog for the change-outfit dialog (`0xC8`).
//!
//! Source of truth: `reference/tfs/data/XML/outfits.xml` (TFS 1.4.2). Every
//! entry — free and premium — is included; the 10.98 wire format has no premium
//! flag per entry, so a player is offered an outfit simply by its presence here.

/// One selectable outfit entry: `(look_type, name)`.
///
/// Source: `reference/tfs/data/XML/outfits.xml`.
/// `type="0"` = FEMALE, `type="1"` = MALE.
/// All entries are included regardless of the `premium`/`unlocked` attributes;
/// the 10.98 wire protocol has no premium flag per entry — presence in the list
/// means "available" (always addons=3).
pub type CatalogEntry = (u16, &'static [u8]);

/// Female outfit catalog — all 57 `type="0"` entries from outfits.xml.
/// Ordered as they appear in the XML (ascending look_type within the file).
pub static FEMALE: &[CatalogEntry] = &[
    (136, b"Citizen"),
    (137, b"Hunter"),
    (138, b"Mage"),
    (139, b"Knight"),
    (140, b"Noblewoman"),
    (141, b"Summoner"),
    (142, b"Warrior"),
    (147, b"Barbarian"),
    (148, b"Druid"),
    (149, b"Wizard"),
    (150, b"Oriental"),
    (155, b"Pirate"),
    (156, b"Assassin"),
    (157, b"Beggar"),
    (158, b"Shaman"),
    (252, b"Norsewoman"),
    (269, b"Nightmare"),
    (270, b"Jester"),
    (279, b"Brotherhood"),
    (288, b"Demon Hunter"),
    (324, b"Yalaharian"),
    (329, b"Newly Wed"),
    (336, b"Warmaster"),
    (366, b"Wayfarer"),
    (431, b"Afflicted"),
    (433, b"Elementalist"),
    (464, b"Deepling"),
    (466, b"Insectoid"),
    (471, b"Entrepreneur"),
    (513, b"Crystal Warlord"),
    (514, b"Soil Guardian"),
    (542, b"Demon Outfit"),
    (575, b"Cave Explorer"),
    (578, b"Dream Warden"),
    (618, b"Glooth Engineer"),
    (620, b"Jersey"),
    (632, b"Champion"),
    (635, b"Conjurer"),
    (636, b"Beastmaster"),
    (664, b"Chaos Acolyte"),
    (666, b"Death Herald"),
    (683, b"Ranger"),
    (694, b"Ceremonial Garb"),
    (696, b"Puppeteer"),
    (698, b"Spirit Caller"),
    (724, b"Evoker"),
    (732, b"Seaweaver"),
    (745, b"Recruiter"),
    (749, b"Sea Dog"),
    (759, b"Royal Pumpkin"),
    (845, b"Rift Warrior"),
    (852, b"Winter Warden"),
    (874, b"Philosopher"),
    (885, b"Arena Champion"),
    (900, b"Lupine Warden"),
];

/// Male outfit catalog — all 57 `type="1"` entries from outfits.xml.
/// Ordered as they appear in the XML (ascending look_type within the file).
pub static MALE: &[CatalogEntry] = &[
    (128, b"Citizen"),
    (129, b"Hunter"),
    (130, b"Mage"),
    (131, b"Knight"),
    (132, b"Nobleman"),
    (133, b"Summoner"),
    (134, b"Warrior"),
    (143, b"Barbarian"),
    (144, b"Druid"),
    (145, b"Wizard"),
    (146, b"Oriental"),
    (151, b"Pirate"),
    (152, b"Assassin"),
    (153, b"Beggar"),
    (154, b"Shaman"),
    (251, b"Norseman"),
    (268, b"Nightmare"),
    (273, b"Jester"),
    (278, b"Brotherhood"),
    (289, b"Demon Hunter"),
    (325, b"Yalaharian"),
    (328, b"Newly Wed"),
    (335, b"Warmaster"),
    (367, b"Wayfarer"),
    (430, b"Afflicted"),
    (432, b"Elementalist"),
    (463, b"Deepling"),
    (465, b"Insectoid"),
    (472, b"Entrepreneur"),
    (512, b"Crystal Warlord"),
    (516, b"Soil Guardian"),
    (541, b"Demon Outfit"),
    (574, b"Cave Explorer"),
    (577, b"Dream Warden"),
    (610, b"Glooth Engineer"),
    (619, b"Jersey"),
    (633, b"Champion"),
    (634, b"Conjurer"),
    (637, b"Beastmaster"),
    (665, b"Chaos Acolyte"),
    (667, b"Death Herald"),
    (684, b"Ranger"),
    (695, b"Ceremonial Garb"),
    (697, b"Puppeteer"),
    (699, b"Spirit Caller"),
    (725, b"Evoker"),
    (733, b"Seaweaver"),
    (746, b"Recruiter"),
    (750, b"Sea Dog"),
    (760, b"Royal Pumpkin"),
    (846, b"Rift Warrior"),
    (853, b"Winter Warden"),
    (873, b"Philosopher"),
    (884, b"Arena Champion"),
    (899, b"Lupine Warden"),
];

/// Return the gendered outfit catalog for the given `sex`.
///
/// `sex == 0` → FEMALE (TFS outfits.xml `type="0"`).
/// Any other value → MALE (TFS outfits.xml `type="1"`), matching the TFS
/// default and the default look_type 128 (male Citizen).
pub fn catalog_for_sex(sex: u8) -> &'static [CatalogEntry] {
    if sex == 0 { FEMALE } else { MALE }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn female_catalog_has_55_entries() {
        assert_eq!(FEMALE.len(), 55, "female catalog must mirror the 55 type=0 entries in outfits.xml");
    }

    #[test]
    fn male_catalog_has_55_entries() {
        assert_eq!(MALE.len(), 55, "male catalog must mirror the 55 type=1 entries in outfits.xml");
    }

    #[test]
    fn female_catalog_first_entry_is_136_citizen() {
        assert_eq!(FEMALE[0], (136, b"Citizen" as &[u8]), "female Citizen is look_type 136");
    }

    #[test]
    fn male_catalog_first_entry_is_128_citizen() {
        assert_eq!(MALE[0], (128, b"Citizen" as &[u8]), "male Citizen is look_type 128");
    }

    #[test]
    fn female_catalog_last_entry_is_900_lupine_warden() {
        assert_eq!(FEMALE[54], (900, b"Lupine Warden" as &[u8]));
    }

    #[test]
    fn male_catalog_last_entry_is_899_lupine_warden() {
        assert_eq!(MALE[54], (899, b"Lupine Warden" as &[u8]));
    }

    #[test]
    fn catalog_for_sex_0_returns_female() {
        let cat = catalog_for_sex(0);
        assert_eq!(cat.len(), 55);
        assert_eq!(cat[0].0, 136, "sex=0 (female) first look_type must be 136");
    }

    #[test]
    fn catalog_for_sex_1_returns_male() {
        let cat = catalog_for_sex(1);
        assert_eq!(cat.len(), 55);
        assert_eq!(cat[0].0, 128, "sex=1 (male) first look_type must be 128");
    }

    #[test]
    fn catalog_for_sex_female_does_not_contain_128() {
        let cat = catalog_for_sex(0);
        assert!(
            !cat.iter().any(|&(lt, _)| lt == 128),
            "female catalog must not contain male Citizen look_type 128"
        );
    }

    #[test]
    fn catalog_for_sex_male_does_not_contain_136() {
        let cat = catalog_for_sex(1);
        assert!(
            !cat.iter().any(|&(lt, _)| lt == 136),
            "male catalog must not contain female Citizen look_type 136"
        );
    }
}
