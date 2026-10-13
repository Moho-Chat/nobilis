//! Steam's result codes, said in words.
//!
//! Every answer from Steam carries one of these, a number from a list of over
//! a hundred. Most are only ever seen by the one team that sends them; what is
//! here is the ones a person signing in or sending a message can reach, put as
//! what it means for them, and everything else is named by its number so a
//! report of it can be looked up.

pub const OK: i32 = 1;
pub const FILE_NOT_FOUND: i32 = 9;
pub const INVALID_STATE: i32 = 11;
pub const COMMUNITY_COOLDOWN: i32 = 116;
pub const MUST_AGREE_TO_SSA: i32 = 118;
pub const FAIL: i32 = 2;
pub const NO_CONNECTION: i32 = 3;
pub const INVALID_PASSWORD: i32 = 5;
pub const LOGGED_IN_ELSEWHERE: i32 = 6;
pub const INVALID_PROTOCOL_VER: i32 = 7;
pub const INVALID_PARAM: i32 = 8;
pub const BUSY: i32 = 10;
pub const ACCESS_DENIED: i32 = 15;
pub const TIMEOUT: i32 = 16;
pub const BANNED: i32 = 17;
pub const ACCOUNT_NOT_FOUND: i32 = 18;
pub const INVALID_STEAM_ID: i32 = 19;
pub const SERVICE_UNAVAILABLE: i32 = 20;
pub const NOT_LOGGED_ON: i32 = 21;
pub const INSUFFICIENT_PRIVILEGE: i32 = 24;
pub const LIMIT_EXCEEDED: i32 = 25;
pub const REVOKED: i32 = 26;
pub const DUPLICATE_REQUEST: i32 = 29;
pub const EXPIRED: i32 = 27;
pub const BLOCKED: i32 = 40;
pub const IGNORED: i32 = 41;
pub const NO_MATCH: i32 = 42;
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
///
/// Every number Steam defines up to 130 has a sentence here, so a failure is never reported as a
/// bare number; the numbers are the ones the Steam client's own code uses for the ones it names
/// (checked against its constants), and the rest follow the public list. A number past the end of
/// that list is named by its number so a report of it can be looked up.
pub fn describe(code: i32) -> String {
    let said = match code {
        0 => "Steam gave no result",
        OK => "it worked",
        FAIL => "Steam reported a failure without saying why",
        NO_CONNECTION => "there is no connection to Steam just now",
        4 => "Steam gave no result",
        INVALID_PASSWORD => "Steam says that password is not right",
        LOGGED_IN_ELSEWHERE => "this account signed in somewhere else, which ended this session",
        INVALID_PROTOCOL_VER | CLIENT_NO_LONGER_SUPPORTED => "Steam no longer accepts this version of moho's connection",
        INVALID_PARAM => "Steam did not understand what was asked",
        FILE_NOT_FOUND => "Steam could not find what was asked for",
        BUSY | SERVICE_UNAVAILABLE | TIMEOUT => "Steam is not answering just now; try again in a moment",
        INVALID_STATE => "that cannot be done in the state it is in",
        12 => "Steam does not accept that name",
        13 => "Steam does not accept that email address",
        14 => "that name is already taken",
        ACCESS_DENIED => "Steam refused the request",
        BANNED | SUSPENDED => "this account is suspended",
        ACCOUNT_NOT_FOUND => "Steam has no account by that name",
        INVALID_STEAM_ID => "Steam does not recognise that as an account",
        NOT_LOGGED_ON => "the connection is not signed in",
        22 => "Steam is still working on that",
        23 => "Steam could not secure the connection",
        INSUFFICIENT_PRIVILEGE => "this account is not allowed to do that; on Steam that usually means it is a limited account, which has to spend $5 before it can send friend requests",
        LIMIT_EXCEEDED => "that is over a limit Steam keeps; wait a while or do less of it",
        REVOKED | EXPIRED | REQUIRE_PASSWORD_RE_ENTRY => "the saved sign-in is no longer valid; sign in again",
        28 => "that has already been used",
        DUPLICATE_REQUEST => "a request to this person is already waiting",
        30 => "this account already has that",
        31 => "Steam could not place this connection",
        32 | 33 => "Steam could not save that just now; try again in a moment",
        34 => "this session was replaced by another sign-in",
        35 | 36 => "Steam's server could not be reached just now; try again in a moment",
        37 => "Steam had a problem on its side; try again in a moment",
        38 => "Steam ended the connection",
        39 => "Steam could not find that cart",
        BLOCKED => "this person has blocked you",
        IGNORED => "this person is ignoring you",
        NO_MATCH => "Steam found nothing that matches",
        ACCOUNT_DISABLED | ACCOUNT_HAS_BEEN_DELETED => "this account is disabled",
        44 => "Steam is read-only just now; try again later",
        45 => "this account cannot use that feature",
        46 => "Steam gave an administrator's answer",
        47 => "Steam needs a newer version of the client",
        TRY_ANOTHER_CM => "that Steam server asked to be tried another way",
        49 => "a password is needed to end the other session",
        50 => "this account is already signed in elsewhere",
        52 => "that was cancelled",
        53 | 54 => "Steam could not store that",
        55 => "Steam's server could not complete the call; try again in a moment",
        56 => "this account has no password set",
        57..=60 => "Steam could not link that account",
        61 => "Steam does not accept that password",
        62 => "that is the same as before",
        ACCOUNT_LOGON_DENIED => "Steam wants the code it emailed to this account",
        64 => "that password has been used before",
        INVALID_LOGIN_AUTH_CODE => "that code is not right",
        66 => "Steam wants a code, and this account has no email to send it to",
        67 | 68 => "this device cannot do that",
        69 => "Family View is holding this back; unlock it in Steam to continue",
        70 => "Steam could not reach the other service",
        EXPIRED_LOGIN_AUTH_CODE => "that code has expired; ask for another",
        IP_LOGIN_RESTRICTION_FAILED => "Steam will not let this account sign in from this address",
        ACCOUNT_LOCKED_DOWN => "this account is locked until its owner secures it",
        74 => "this account must verify its email address first",
        75 | 76 => "Steam gave an answer that could not be used",
        78 => "that is out of the range Steam accepts",
        79 => "Steam reported an unexpected error",
        80 => "that feature is switched off",
        81 | 82 => "this device is not allowed to do that",
        83 => "that is not available where this account is",
        RATE_LIMIT_EXCEEDED | ACCOUNT_LOGIN_DENIED_THROTTLE => "Steam is limiting how often this can be tried; wait a while before trying again",
        ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR => "Steam wants the code from the Steam Guard authenticator",
        86 => "that has been deleted",
        TWO_FACTOR_CODE_MISMATCH => "that Steam Guard code is not right",
        89 => "that activation code is not right",
        90 => "this account is linked to more than one partner",
        91 => "nothing changed",
        92 => "no phone is attached to this account",
        93 => "the clock on this computer is too far from Steam's; set it right and try again",
        94 => "Steam could not send the code to the phone",
        95..=97 => "this account or phone has done that too many times; wait a while",
        98..=100 => "Steam could not settle that",
        101 => "Steam wants a captcha answered, which only its own page can do",
        102..=104 => "Steam refused that",
        105 => "this address is banned from Steam",
        106 => "that has expired",
        107 => "this account does not have the funds",
        108 => "too many are waiting; try again later",
        109 => "no licence was found",
        110 => "Steam could not send that much at once",
        ACCOUNT_NOT_FRIENDS => "you can only message people who are your friends",
        LIMITED_USER_ACCOUNT => "a limited account cannot do that; Steam asks for a purchase first",
        113 => "that cannot be removed",
        115 => "a licence was cancelled",
        COMMUNITY_COOLDOWN => "Steam asks this account to wait before doing that again",
        117 => "no launcher was given",
        MUST_AGREE_TO_SSA => "Steam wants the subscriber agreement accepted first, which only its own page can do",
        120 => "this is a different Steam realm",
        121 | 122 => "Steam gave an answer that could not be read",
        123 => "this account needs a verified phone number first",
        124 | 125 => "the device's power does not allow that",
        126 => "the saved sign-in is no longer valid; sign in again",
        127 => "a phone number that is internet-only cannot be used",
        128 => "that is not supported",
        129 => "that is over the family's size limit",
        130 => "the offline copy is no longer good",
        other => return format!("Steam answered with error {other}"),
    };
    said.to_string()
}

/// Whether the saved sign-in is dead, so only a fresh sign-in can help.
pub fn means_sign_in_again(code: i32) -> bool {
    matches!(
        code,
        INVALID_PASSWORD
            | REVOKED
            | EXPIRED
            | REQUIRE_PASSWORD_RE_ENTRY
            | ACCOUNT_NOT_FOUND
            | ACCOUNT_LOGON_DENIED
            | ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR
            | 126
    )
}

/// Whether the answer is about the account or this client and so the same on every server and every
/// try: signed in elsewhere, replaced, suspended, locked down, banned by address, a version Steam no
/// longer takes, a clock that is wrong, a page only Steam's own site can show. Retrying only repeats
/// it - and for the first two takes the other session back and forth.
pub fn is_final(code: i32) -> bool {
    matches!(
        code,
        LOGGED_IN_ELSEWHERE
            | 34
            | 50
            | BANNED
            | SUSPENDED
            | ACCOUNT_DISABLED
            | ACCOUNT_HAS_BEEN_DELETED
            | ACCOUNT_LOCKED_DOWN
            | IP_LOGIN_RESTRICTION_FAILED
            | 105
            | INVALID_PROTOCOL_VER
            | CLIENT_NO_LONGER_SUPPORTED
            | 69
            | 74
            | 93
            | MUST_AGREE_TO_SSA
    )
}

/// Whether Steam is limiting how often this may be tried, so the answer is to wait before trying
/// again, not to try another server.
pub fn is_rate_limit(code: i32) -> bool {
    matches!(
        code,
        RATE_LIMIT_EXCEEDED | ACCOUNT_LOGIN_DENIED_THROTTLE | LIMIT_EXCEEDED | COMMUNITY_COOLDOWN
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ones_a_person_reaches_are_said_plainly() {
        assert!(describe(INVALID_PASSWORD).contains("password"));
        assert!(describe(RATE_LIMIT_EXCEEDED).contains("limiting"));
        assert!(describe(ACCOUNT_NOT_FRIENDS).contains("friends"));
        assert!(describe(INSUFFICIENT_PRIVILEGE).contains("limited account"));
        assert!(describe(NO_MATCH).contains("nothing"));
        assert!(describe(DUPLICATE_REQUEST).contains("already"));
        assert!(describe(LIMITED_USER_ACCOUNT).contains("limited account"));
        assert!(describe(TWO_FACTOR_CODE_MISMATCH).contains("Steam Guard"));
    }

    #[test]
    fn every_result_steam_defines_has_a_sentence_and_only_a_number_past_the_list_is_named_by_number(
    ) {
        for code in 0..=130 {
            let said = describe(code);
            assert!(
                !said.starts_with("Steam answered with error"),
                "{code} has no sentence"
            );
            assert!(!said.is_empty());
        }
        assert_eq!(describe(131), "Steam answered with error 131");
        assert_eq!(describe(9999), "Steam answered with error 9999");
        assert_eq!(describe(-1), "Steam answered with error -1");
    }

    #[test]
    fn what_ends_a_session_for_good_what_waits_and_what_another_server_may_fix_are_told_apart() {
        for code in [
            LOGGED_IN_ELSEWHERE,
            34,
            50,
            SUSPENDED,
            ACCOUNT_LOCKED_DOWN,
            105,
            CLIENT_NO_LONGER_SUPPORTED,
            93,
            MUST_AGREE_TO_SSA,
        ] {
            assert!(is_final(code), "{code} is the same on every try");
        }
        for code in [
            RATE_LIMIT_EXCEEDED,
            ACCOUNT_LOGIN_DENIED_THROTTLE,
            LIMIT_EXCEEDED,
            COMMUNITY_COOLDOWN,
        ] {
            assert!(is_rate_limit(code) && !is_final(code));
        }
        assert!(means_sign_in_again(126));
        for code in [FAIL, TIMEOUT, SERVICE_UNAVAILABLE, 37, 55] {
            assert!(
                !is_final(code) && !is_rate_limit(code) && !means_sign_in_again(code),
                "{code} may be better elsewhere"
            );
        }
    }

    #[test]
    fn the_ones_the_clients_own_code_treats_specially_say_what_it_says() {
        // 15 is the client's "not friends" on a direct message, 84 its rate limit, 118 its agreement page
        assert!(describe(MUST_AGREE_TO_SSA).contains("agreement"));
        assert!(describe(COMMUNITY_COOLDOWN).contains("wait"));
        assert!(describe(69).contains("Family View"));
        assert!(describe(93).contains("clock"));
    }

    #[test]
    fn some_failures_are_the_servers_and_some_are_the_accounts() {
        assert!(means_sign_in_again(REVOKED));
        assert!(means_sign_in_again(INVALID_PASSWORD));
        assert!(
            !means_sign_in_again(RATE_LIMIT_EXCEEDED),
            "waiting helps, signing in again does not"
        );
        assert!(!means_sign_in_again(SERVICE_UNAVAILABLE));
    }
}
