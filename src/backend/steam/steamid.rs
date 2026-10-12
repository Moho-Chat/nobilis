//! A Steam account's identity: the 32-bit account number a friends list and a
//! chat message use, and the 64-bit SteamID the connection and the profile
//! pages use.
//!
//! A SteamID64 is that account number with three more things packed in beside
//! it: the universe (1, the public one), the type of account (1, an
//! individual - groups and games have their own) and an instance (1, the
//! desktop one, which every ordinary person's account has). So for a person it
//! is always the account number added to one constant, and these two functions
//! are the whole conversion.

/// Universe 1, type 1, instance 1, account 0: what a person's id starts from.
pub const INDIVIDUAL_BASE: u64 = (1 << 56) | (1 << 52) | (1 << 32);

/// The 64-bit id of the person with this account number.
pub fn steamid_of(account: u32) -> u64 {
    INDIVIDUAL_BASE + u64::from(account)
}

/// The account number in a person's 64-bit id, or `None` for an id that is not
/// a person's (a group's, a game server's, or nonsense).
pub fn account_of(steamid: u64) -> Option<u32> {
    // The universe, type and instance must be exactly an individual's; what is
    // left is the account number, which is 32 bits.
    let rest = steamid.checked_sub(INDIVIDUAL_BASE)?;
    u32::try_from(rest).ok()
}

/// The id the way it is written in a profile address.
pub fn text_of(account: u32) -> String {
    steamid_of(account).to_string()
}

/// A person's id typed or pasted as a SteamID64, or as the account number on
/// its own.
pub fn parse(text: &str) -> Option<u64> {
    let n: u64 = text.trim().parse().ok()?;
    if account_of(n).is_some() {
        return Some(n);
    }
    u32::try_from(n).ok().map(steamid_of)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gabe Newell's account, whose id is the most photographed in existence:
    /// account number 22202 is 76561197960287930.
    #[test]
    fn the_two_forms_of_an_id_convert_both_ways() {
        assert_eq!(steamid_of(22202), 76561197960287930);
        assert_eq!(account_of(76561197960287930), Some(22202));
        assert_eq!(INDIVIDUAL_BASE, 76561197960265728);
        assert_eq!(account_of(steamid_of(u32::MAX)), Some(u32::MAX));
        assert_eq!(account_of(steamid_of(0)), Some(0));
    }

    #[test]
    fn something_that_is_not_a_person_is_not_converted() {
        // A group's id has type 7 where a person's has 1.
        let group = (1u64 << 56) | (7 << 52) | (1 << 32) | 12345;
        assert_eq!(account_of(group), None);
        assert_eq!(account_of(0), None);
        assert_eq!(account_of(INDIVIDUAL_BASE - 1), None);
        // Too large to be an account number added to the base.
        assert_eq!(account_of(INDIVIDUAL_BASE + u64::from(u32::MAX) + 1), None);
    }

    #[test]
    fn what_somebody_types_is_understood_either_way() {
        assert_eq!(parse("76561197960287930"), Some(76561197960287930));
        assert_eq!(parse("  22202 "), Some(76561197960287930));
        assert_eq!(parse("not an id"), None);
        assert_eq!(parse(""), None);
        // Big, but not a person's.
        assert_eq!(parse("99999999999999999"), None);
        assert_eq!(text_of(22202), "76561197960287930");
    }
}
