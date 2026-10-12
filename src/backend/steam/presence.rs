//! Presence: the words moho uses for how a person is showing, and Steam's.
//!
//! Steam has eight states and moho's window draws four. The mapping is the
//! only place that decides which is which, in both directions, so a friend's
//! dot and our own plaque cannot disagree about what "Snooze" is.
//!
//! Steam's states are numbers: Offline 0, Online 1, Busy 2, Away 3, Snooze 4,
//! Looking to Trade 5, Looking to Play 6, Invisible 7.

pub const OFFLINE: u32 = 0;
pub const ONLINE: u32 = 1;
pub const BUSY: u32 = 2;
pub const AWAY: u32 = 3;
pub const SNOOZE: u32 = 4;
pub const LOOKING_TO_TRADE: u32 = 5;
pub const LOOKING_TO_PLAY: u32 = 6;
pub const INVISIBLE: u32 = 7;

/// What to tell Steam for a status chosen in moho.
///
/// Do not disturb is moho's own switch for its own notifications and stays
/// Online to Steam (as it does for IRC and Matrix); Steam's "Busy" is a
/// different thing, a state friends see, and it is not set on anyone's behalf.
pub fn persona_state_for(status: &str) -> u32 {
    match status {
        "idle" => AWAY,
        "invisible" => INVISIBLE,
        _ => ONLINE,
    }
}

/// How a state Steam reported is drawn. Invisible is never seen on somebody
/// else - Steam shows them as offline - but it is understood on ourselves.
pub fn status_of(persona_state: u32) -> &'static str {
    match persona_state {
        ONLINE | LOOKING_TO_TRADE | LOOKING_TO_PLAY => "online",
        BUSY => "dnd",
        AWAY | SNOOZE => "idle",
        _ => "offline",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_moho_offers_is_what_steam_is_told() {
        assert_eq!(persona_state_for("online"), 1);
        assert_eq!(persona_state_for("idle"), 3, "idle is Steam's Away");
        assert_eq!(persona_state_for("invisible"), 7);
        assert_eq!(persona_state_for("dnd"), 1, "do not disturb is local; Steam still sees an online person");
        assert_eq!(persona_state_for("nonsense"), 1);
    }

    #[test]
    fn every_state_steam_reports_is_drawn_as_one_of_the_four() {
        assert_eq!(status_of(0), "offline");
        assert_eq!(status_of(1), "online");
        assert_eq!(status_of(2), "dnd");
        assert_eq!(status_of(3), "idle");
        assert_eq!(status_of(4), "idle");
        assert_eq!(status_of(5), "online");
        assert_eq!(status_of(6), "online");
        assert_eq!(status_of(7), "offline");
        assert_eq!(status_of(99), "offline", "a state from a newer Steam is not a guess in the direction that shows");
    }

    #[test]
    fn what_we_set_is_read_back_the_way_we_set_it() {
        for status in ["online", "idle"] {
            assert_eq!(status_of(persona_state_for(status)), status);
        }
    }
}
