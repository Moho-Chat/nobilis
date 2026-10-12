//! Steam's result codes, said in words.
//!
//! Every answer from Steam carries one of these, a number from a list of over
//! a hundred. Most are only ever seen by the one team that sends them; what is
//! here is the ones a person signing in or sending a message can reach, put as
//! what it means for them, and everything else is named by its number so a
//! report of it can be looked up.

pub const OK: i32 = 1;
pub const FAIL: i32 = 2;
pub const NO_CONNECTION: i32 = 3;
pub const INVALID_PASSWORD: i32 = 5;
pub const LOGGED_IN_ELSEWHERE: i32 = 6;
pub const INVALID_PROTOCOL_VER: i32 = 7;
pub const BUSY: i32 = 10;
pub const ACCESS_DENIED: i32 = 15;
pub const TIMEOUT: i32 = 16;
pub const BANNED: i32 = 17;
pub const ACCOUNT_NOT_FOUND: i32 = 18;
pub const SERVICE_UNAVAILABLE: i32 = 20;
pub const NOT_LOGGED_ON: i32 = 21;
pub const LIMIT_EXCEEDED: i32 = 25;
pub const REVOKED: i32 = 26;
pub const EXPIRED: i32 = 27;
pub const BLOCKED: i32 = 40;
pub const IGNORED: i32 = 41;
pub const ACCOUNT_DISABLED: i32 = 43;
pub const TRY_ANOTHER_CM: i32 = 48;
pub const SUSPENDED: i32 = 51;
pub const ACCOUNT_LOGON_DENIED: i32 = 63;
pub const INVALID_LOGIN_AUTH_CODE: i32 = 65;
pub const EXPIRED_LOGIN_AUTH_CODE: i32 = 71;
pub const IP_LOGIN_RESTRICTION_FAILED: i32 = 72;
pub const ACCOUNT_LOCKED_DOWN: i32 = 73;
pub const REQUIRE_PASSWORD_RE_ENTRY: i32 = 77;
pub const RATE_LIMIT_EXCEEDED: i32 = 84;
pub const ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR: i32 = 85;
pub const ACCOUNT_LOGIN_DENIED_THROTTLE: i32 = 87;
pub const TWO_FACTOR_CODE_MISMATCH: i32 = 88;
pub const ACCOUNT_NOT_FRIENDS: i32 = 111;
pub const LIMITED_USER_ACCOUNT: i32 = 112;
pub const ACCOUNT_HAS_BEEN_DELETED: i32 = 114;
pub const CLIENT_NO_LONGER_SUPPORTED: i32 = 119;

/// What a result means to a person, in a sentence that stands on its own.
pub fn describe(code: i32) -> String {
    match code {
        OK => "it worked".to_string(),
        INVALID_PASSWORD => "Steam says that password is not right".to_string(),
        LOGGED_IN_ELSEWHERE => "this account signed in somewhere else, which ended this session".to_string(),
        INVALID_PROTOCOL_VER | CLIENT_NO_LONGER_SUPPORTED => "Steam no longer accepts this version of moho's connection".to_string(),
        BUSY | SERVICE_UNAVAILABLE | NO_CONNECTION | TIMEOUT => "Steam is not answering just now; try again in a moment".to_string(),
        ACCESS_DENIED => "Steam refused the request".to_string(),
        BANNED | SUSPENDED => "this account is suspended".to_string(),
        ACCOUNT_NOT_FOUND => "Steam has no account by that name".to_string(),
        NOT_LOGGED_ON => "the connection is not signed in".to_string(),
        LIMIT_EXCEEDED | RATE_LIMIT_EXCEEDED | ACCOUNT_LOGIN_DENIED_THROTTLE => {
            "Steam is limiting how often this can be tried; wait a while before trying again".to_string()
        }
        REVOKED | EXPIRED | REQUIRE_PASSWORD_RE_ENTRY => "the saved sign-in is no longer valid; sign in again".to_string(),
        BLOCKED => "this person has blocked you".to_string(),
        IGNORED => "this person is ignoring you".to_string(),
        ACCOUNT_DISABLED | ACCOUNT_HAS_BEEN_DELETED => "this account is disabled".to_string(),
        TRY_ANOTHER_CM => "that Steam server asked to be tried another way".to_string(),
        ACCOUNT_LOGON_DENIED => "Steam wants the code it emailed to this account".to_string(),
        INVALID_LOGIN_AUTH_CODE => "that code is not right".to_string(),
        EXPIRED_LOGIN_AUTH_CODE => "that code has expired; ask for another".to_string(),
        TWO_FACTOR_CODE_MISMATCH => "that Steam Guard code is not right".to_string(),
        ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR => "Steam wants the code from the Steam Guard authenticator".to_string(),
        IP_LOGIN_RESTRICTION_FAILED => "Steam will not let this account sign in from this address".to_string(),
        ACCOUNT_LOCKED_DOWN => "this account is locked until its owner secures it".to_string(),
        ACCOUNT_NOT_FRIENDS => "you can only message people who are your friends".to_string(),
        LIMITED_USER_ACCOUNT => "a limited account cannot do that; Steam asks for a purchase first".to_string(),
        FAIL => "Steam reported a failure without saying why".to_string(),
        other => format!("Steam answered with error {other}"),
    }
}

/// Whether trying a different connection manager could help, as against the
/// answer being about the account and so the same everywhere.
pub fn worth_another_server(code: i32) -> bool {
    matches!(code, NO_CONNECTION | BUSY | SERVICE_UNAVAILABLE | TIMEOUT | TRY_ANOTHER_CM | FAIL)
}

/// Whether the saved sign-in is dead, so only a fresh sign-in can help.
pub fn means_sign_in_again(code: i32) -> bool {
    matches!(code, INVALID_PASSWORD | REVOKED | EXPIRED | REQUIRE_PASSWORD_RE_ENTRY | ACCOUNT_NOT_FOUND | ACCOUNT_LOGON_DENIED | ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ones_a_person_reaches_are_said_plainly() {
        assert!(describe(INVALID_PASSWORD).contains("password"));
        assert!(describe(RATE_LIMIT_EXCEEDED).contains("limiting"));
        assert!(describe(ACCOUNT_NOT_FRIENDS).contains("friends"));
        assert!(describe(LIMITED_USER_ACCOUNT).contains("limited account"));
        assert!(describe(TWO_FACTOR_CODE_MISMATCH).contains("Steam Guard"));
    }

    #[test]
    fn the_rest_are_named_by_their_number() {
        assert_eq!(describe(9999), "Steam answered with error 9999");
        assert_eq!(describe(-1), "Steam answered with error -1");
    }

    #[test]
    fn some_failures_are_the_servers_and_some_are_the_accounts() {
        assert!(worth_another_server(TRY_ANOTHER_CM));
        assert!(worth_another_server(SERVICE_UNAVAILABLE));
        assert!(!worth_another_server(INVALID_PASSWORD));
        assert!(means_sign_in_again(REVOKED));
        assert!(means_sign_in_again(INVALID_PASSWORD));
        assert!(!means_sign_in_again(RATE_LIMIT_EXCEEDED), "waiting helps, signing in again does not");
        assert!(!means_sign_in_again(SERVICE_UNAVAILABLE));
    }
}
