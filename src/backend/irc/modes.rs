//! What a channel's modes mean, in words.
//!
//! A server answers `/mode #chan` with letters - `+Cnrt` - which a new IRC user cannot be expected to
//! read. The channel line moho writes on joining explains them: "#archlinux is +Cnrt -- No CTCP messages
//! to the channel, no messages from outside the channel, registered users only, topic locked to operators."
//!
//! The letters that mean the same everywhere (RFC 2811 and what every server inherited) are plain; a few
//! that nearly every server agrees on are included; a letter a network invented for itself is named as
//! one rather than guessed at, since a wrong explanation is worse than none.

/// One mode letter's meaning, given its argument if it takes one. `None` for a letter this does not know.
fn meaning(letter: char, param: Option<&str>) -> Option<String> {
    let with = |what: &str| param.map(|p| format!("{what} {p}"));
    Some(match letter {
        'n' => "no messages from outside the channel".into(),
        't' => "topic locked to operators".into(),
        'm' => "moderated: only voiced people and operators can talk".into(),
        'i' => "invite only".into(),
        's' => "secret: left out of the channel list".into(),
        'p' => "private".into(),
        'r' => "registered users only".into(),
        'c' => "no colours or formatting".into(),
        'C' => "no CTCP messages to the channel".into(),
        'S' => "only people connected over TLS".into(),
        'g' => "anybody can invite".into(),
        'P' => "permanent: stays when it is empty".into(),
        'L' => "a larger ban list".into(),
        'k' => "needs a key to join".into(),
        'l' => match param {
            Some(p) => format!("limited to {p} people"),
            None => "limited in size".into(),
        },
        'f' => with("people who cannot join are sent to")?,
        'j' => with("joining is throttled to")?,
        _ => return None,
    })
}

/// Whether a mode letter takes an argument when it is set, for the ones this explains.
fn takes_param(letter: char) -> bool {
    matches!(letter, 'k' | 'l' | 'f' | 'j')
}

/// Explains a channel's modes: the string the server sent (`+Cnrt`, `+lk`) and the arguments after it.
/// `None` when there is nothing to say (no modes set).
pub fn describe(modes: &str, params: &[String]) -> Option<String> {
    let mut args = params.iter();
    let mut said: Vec<String> = Vec::new();
    let mut unknown: Vec<char> = Vec::new();
    let mut adding = true;
    for letter in modes.chars() {
        match letter {
            '+' => adding = true,
            '-' => adding = false,
            c if !adding || !c.is_ascii_alphabetic() => {
                let _ = c;
            }
            c => {
                let param = takes_param(c).then(|| args.next().map(String::as_str)).flatten();
                // A key is a secret: said that there is one, never what it is.
                let param = if c == 'k' { None } else { param };
                match meaning(c, param) {
                    Some(text) => said.push(text),
                    None => unknown.push(c),
                }
            }
        }
    }
    if !unknown.is_empty() {
        let list = unknown.iter().map(|c| format!("+{c}")).collect::<Vec<_>>().join(", ");
        said.push(format!("{list} {} this network's own", if unknown.len() == 1 { "is" } else { "are" }));
    }
    if said.is_empty() {
        return None;
    }
    let mut text = said.join(", ");
    // A sentence: a capital to begin, a full stop to end.
    if let Some(first) = text.chars().next() {
        text.replace_range(..first.len_utf8(), &first.to_uppercase().to_string());
    }
    Some(format!("{text}."))
}

/// What one mode being set or cleared means, for the line that says somebody changed it. `None` for a letter
/// this does not know, where the line says no more than it did.
pub fn describe_change(letter: char, adding: bool) -> Option<String> {
    let text = meaning(letter, None)?;
    if adding {
        let mut c = text.chars();
        return c.next().map(|f| format!("{}{}.", f.to_uppercase(), c.as_str()));
    }
    Some(format!("No longer in effect: {text}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(modes: &str, params: &[&str]) -> Option<String> {
        describe(modes, &params.iter().map(|p| p.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_letters_of_a_busy_channel_are_said_in_the_order_the_server_sent_them() {
        assert_eq!(
            d("+Cnrt", &[]).as_deref(),
            Some("No CTCP messages to the channel, no messages from outside the channel, registered users only, topic locked to operators.")
        );
        assert_eq!(d("+nt", &[]).as_deref(), Some("No messages from outside the channel, topic locked to operators."));
    }

    #[test]
    fn a_limit_says_its_number_and_a_key_is_never_told() {
        assert_eq!(d("+lntk", &["50", "hunter2"]).as_deref(), Some("Limited to 50 people, no messages from outside the channel, topic locked to operators, needs a key to join."));
        assert!(!d("+k", &["hunter2"]).unwrap().contains("hunter2"));
        assert_eq!(d("+f", &["#overflow"]).as_deref(), Some("People who cannot join are sent to #overflow."));
    }

    #[test]
    fn a_letter_nobody_agrees_on_is_named_and_not_guessed_at() {
        assert_eq!(d("+nQ", &[]).as_deref(), Some("No messages from outside the channel, +Q is this network's own."));
        assert_eq!(d("+QZ", &[]).as_deref(), Some("+Q, +Z are this network's own."));
    }

    #[test]
    fn a_change_says_what_it_did() {
        assert_eq!(describe_change('m', true).as_deref(), Some("Moderated: only voiced people and operators can talk."));
        assert_eq!(describe_change('t', false).as_deref(), Some("No longer in effect: topic locked to operators."));
        assert_eq!(describe_change('Q', true), None);
    }

    #[test]
    fn no_modes_say_nothing() {
        assert_eq!(d("+", &[]), None);
        assert_eq!(d("", &[]), None);
    }
}
