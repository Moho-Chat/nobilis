//! Everything this client says to the network.
//!
//! Messages, modes, and the bookkeeping that turns a line typed here into a
//! line the server has confirmed: with `echo-message` the confirmation is the
//! server sending it back, and until it arrives the line on screen is this
//! client's own guess at what it will look like.

use super::*;

/// Sends `body` to `target_buffer` (an already-open buffer's channel/nick
/// name) - slash command (/join /part /me /nick /topic /msg) if it starts
/// with "/" ("//" escapes a literal leading slash), otherwise a plain
/// PRIVMSG. Mirrors daemon/nobilis/actions.c's nobilis_send_message, minus
/// libpurple's purple_cmd_do_command registry (hand-rolled here instead -
/// see project plan). Returns Err with a user-facing message on failure.
pub fn send_message(state: &AppState, account_id: &str, sender: &Sender, target_buffer: &str, body: &str) -> Result<()> {
    if let Some(rest) = body.strip_prefix('/') {
        if let Some(literal) = rest.strip_prefix('/') {
            return send_plain(state, account_id, sender, target_buffer, &format!("/{literal}"));
        }
        let mut parts = rest.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let arg = parts.next().unwrap_or("").trim();
        return match cmd.as_str() {
            "join" => sender.send_join(arg).map_err(|e| anyhow!(e)),
            "part" => sender.send_part(target_buffer).map_err(|e| anyhow!(e)),
            "me" => {
                sender.send_action(target_buffer, arg)?;
                let own_nick = state.runtime.irc_current_nick(account_id).unwrap_or_default();
                state.runtime.record_message(state, account_id, target_buffer, buffer_kind_hint(target_buffer), &own_nick, arg, true, "chat", None, None, false, None, Vec::new(), Vec::new(), None);
                Ok(())
            }
            "nick" => sender.send(Command::NICK(arg.to_string())).map_err(|e| anyhow!(e)),
            // The other name you have here. `nick` is what people call you;
            // this is the sentence about yourself that USER fixed at
            // registration and that, without `setname`, could only be changed
            // by dropping the connection and every channel with it.
            //
            // Written to the account as well as sent, so the next connection
            // registers with it rather than reverting to whatever was typed
            // when the account was made.
            "setname" => {
                if arg.is_empty() {
                    bail!("/setname requires the name to use");
                }
                if !state.runtime.irc_has_cap(account_id, "setname") {
                    bail!("this network cannot change a realname without reconnecting");
                }
                sender.send(Command::Raw("SETNAME".to_string(), vec![arg.to_string()]))?;
                if let Err(e) = state.accounts.set_irc_realname(account_id, arg) {
                    tracing::debug!("irc[{account_id}]: keeping the new realname: {e:#}");
                }
                Ok(())
            }
            "topic" => sender.send_topic(target_buffer, arg).map_err(|e| anyhow!(e)),
            // Moderator actions - the frontend only offers these in the
            // userlist context menu when the presence data it already has
            // shows the local user holding founder/op/halfop in this
            // channel (see its member list), so there's no separate
            // permission check needed here: the server itself is the
            // actual authority and will simply reject these if we don't
            // really have the rank, same as any other IRC client.
            "kick" => {
                let mut kick_parts = arg.splitn(2, ' ');
                let nick = kick_parts.next().unwrap_or("");
                let reason = kick_parts.next();
                if nick.is_empty() {
                    bail!("/kick requires a nick");
                }
                sender.send(Command::KICK(target_buffer.to_string(), nick.to_string(), reason.map(String::from))).map_err(|e| anyhow!(e))
            }
            "ban" => {
                if arg.is_empty() {
                    bail!("/ban requires a nick");
                }
                sender
                    .send(Command::ChannelMODE(target_buffer.to_string(), vec![Mode::Plus(ChannelMode::Ban, Some(ban_mask(arg)))]))
                    .map_err(|e| anyhow!(e))
            }
            // Raw, deliberately. Channel modes are a small language of their
            // own with per-network extensions, and a client that only passes
            // through the ones it recognises is a client that cannot set the
            // one this network has. The server is the authority on what is
            // valid and says so itself when it is not.
            //
            // With no argument this asks rather than sets, which is how every
            // other client spells "what modes does this channel have" - and
            // "/mode #chan b" is how you read the ban list.
            "mode" => {
                let (target, rest) = split_mode_target(target_buffer, arg);
                if target.is_empty() {
                    bail!("/mode needs a channel or nick");
                }
                let raw = if rest.is_empty() { format!("MODE {target}") } else { format!("MODE {target} {rest}") };
                sender.send(raw.parse::<Message>().map_err(|e| anyhow!("{e}"))?).map_err(|e| anyhow!(e))
            }
            "unban" => {
                if arg.is_empty() {
                    bail!("/unban requires a nick or mask");
                }
                sender
                    .send(Command::ChannelMODE(target_buffer.to_string(), vec![Mode::Minus(ChannelMode::Ban, Some(ban_mask(arg)))]))
                    .map_err(|e| anyhow!(e))
            }
            "invite" => {
                let mut invite_parts = arg.split_whitespace();
                let nick = invite_parts.next().unwrap_or("");
                if nick.is_empty() {
                    bail!("/invite requires a nick");
                }
                // The channel may be named, or taken from where the command
                // was typed - which is what somebody means nine times in ten.
                let channel = invite_parts.next().unwrap_or(target_buffer);
                sender.send(Command::INVITE(nick.to_string(), channel.to_string())).map_err(|e| anyhow!(e))
            }
            // Asks the server for its channel list. The answer arrives as
            // numerics and is gathered up rather than printed line by line -
            // a network's list runs to tens of thousands of channels, which
            // is a directory to search rather than a wall of chat.
            "list" => {
                let raw = if arg.is_empty() { "LIST".to_string() } else { format!("LIST {arg}") };
                sender.send(raw.parse::<Message>().map_err(|e| anyhow!("{e}"))?).map_err(|e| anyhow!(e))
            }
            "op" => set_channel_mode(sender, target_buffer, ChannelMode::Oper, true, arg),
            "deop" => set_channel_mode(sender, target_buffer, ChannelMode::Oper, false, arg),
            "voice" => set_channel_mode(sender, target_buffer, ChannelMode::Voice, true, arg),
            "devoice" => set_channel_mode(sender, target_buffer, ChannelMode::Voice, false, arg),
            "msg" => {
                let mut msg_parts = arg.splitn(2, ' ');
                let to = msg_parts.next().unwrap_or("");
                let text = msg_parts.next().unwrap_or("");
                if to.is_empty() {
                    bail!("/msg requires a target");
                }
                send_plain(state, account_id, sender, to, text)
            }
            // Who to be told about when they arrive. The list is the
            // account's own - the server is what watches it - so this reads
            // and writes the stored one and puts the answer where the other
            // answers about the network go.
            "notify" | "unnotify" => {
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                let Some(config) = state.accounts.get_irc(account_id) else {
                    bail!("no such account");
                };
                let mut list = notify_list(&config);
                let nick = arg.split_whitespace().next().unwrap_or("");
                if cmd == "notify" && nick.is_empty() {
                    // Asking rather than setting, which is what a bare
                    // /notify means in every client that has one.
                    let body = if list.is_empty() {
                        "watching nobody - /notify <nick> adds somebody".to_string()
                    } else {
                        format!("watching: {}", list.join(", "))
                    };
                    state.runtime.record_message(state, account_id, host, "server", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                    return Ok(());
                }
                if nick.is_empty() || is_channel(nick) {
                    bail!("/{cmd} requires a nick");
                }
                let known = list.iter().any(|n| n.eq_ignore_ascii_case(nick));
                let body = if cmd == "notify" {
                    if known {
                        format!("already watching {nick}")
                    } else {
                        list.push(nick.to_string());
                        let raw = format!("MONITOR + {nick}");
                        if let Ok(msg) = raw.parse::<Message>() {
                            let _ = sender.send(msg);
                        }
                        format!("watching {nick}")
                    }
                } else if known {
                    list.retain(|n| !n.eq_ignore_ascii_case(nick));
                    // Told to forget as well as forgotten here: a server still
                    // monitoring a nick keeps sending news about them.
                    let raw = format!("MONITOR - {nick}");
                    if let Ok(msg) = raw.parse::<Message>() {
                        let _ = sender.send(msg);
                    }
                    notify_seen().lock().unwrap().entry(account_id.to_string()).or_default().remove(&nick.to_lowercase());
                    format!("no longer watching {nick}")
                } else {
                    format!("not watching {nick}")
                };
                state.accounts.set_irc_notify(account_id, &list.join(","))?;
                state.runtime.record_message(state, account_id, host, "server", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                Ok(())
            }
            // Who somebody is. The reply arrives as numerics and is
            // gathered into one answer rather than printed line by line -
            // see take_whois.
            "whois" | "whowas" => {
                let nick = if arg.is_empty() { target_buffer } else { arg.split_whitespace().next().unwrap_or("") };
                if nick.is_empty() || is_channel(nick) {
                    bail!("/{cmd} requires a nick");
                }
                state.runtime.begin_irc_whois(account_id, nick);
                let raw = format!("{} {nick}", cmd.to_uppercase());
                sender.send(raw.parse::<Message>().map_err(|e| anyhow!("{e}"))?).map_err(|e| anyhow!(e))
            }
            // Away with a reason, back without one. The server confirms
            // either way (305/306) and that confirmation is what gets shown,
            // rather than this claiming success on its own.
            "away" => sender.send(Command::AWAY((!arg.is_empty()).then(|| arg.to_string()))).map_err(|e| anyhow!(e)),
            "back" => sender.send(Command::AWAY(None)).map_err(|e| anyhow!(e)),
            // A notice, which is what a client sends when it does not want an
            // answer - and what services and bots are expected to reply with,
            // since a notice must never be auto-replied to.
            "notice" => {
                let mut notice_parts = arg.splitn(2, ' ');
                let to = notice_parts.next().unwrap_or("");
                let text = notice_parts.next().unwrap_or("").trim();
                if to.is_empty() || text.is_empty() {
                    bail!("/notice requires a target and a message");
                }
                sender.send(Command::NOTICE(to.to_string(), text.to_string())).map_err(|e| anyhow!(e))?;
                let own_nick = state.runtime.irc_current_nick(account_id).unwrap_or_default();
                let line = format!("-> -{to}- {text}");
                state.runtime.record_message(state, account_id, target_buffer, buffer_kind_hint(target_buffer), &own_nick, &line, false, "notice", None, None, false, None, Vec::new(), Vec::new(), None);
                Ok(())
            }
            // CTCP: a PRIVMSG wrapped in \x01, which is all CTCP has ever
            // been. VERSION, PING and TIME are the ones anybody sends; the
            // answers come back as notices and are already recognised as
            // administrative rather than conversational.
            "ctcp" => {
                let mut ctcp_parts = arg.splitn(2, ' ');
                let to = ctcp_parts.next().unwrap_or("");
                let rest = ctcp_parts.next().unwrap_or("").trim();
                if to.is_empty() || rest.is_empty() {
                    bail!("/ctcp requires a target and a command, e.g. /ctcp nick VERSION");
                }
                sender.send(Command::PRIVMSG(to.to_string(), format!("\u{1}{rest}\u{1}"))).map_err(|e| anyhow!(e))
            }
            // The escape hatch. Sixteen commands cannot cover a protocol with
            // per-network extensions, services with their own vocabularies and
            // a numeric for everything - and a client with no way to send a
            // line verbatim is a client that cannot do the thing its network
            // documents. What comes back shows in the server buffer, as it
            // already does for everything unrecognised.
            "quote" | "raw" => {
                if arg.is_empty() {
                    bail!("/{cmd} needs something to send");
                }
                sender.send(arg.parse::<Message>().map_err(|e| anyhow!("{e}"))?).map_err(|e| anyhow!(e))
            }
            other => bail!("unknown command \"/{other}\""),
        };
    }
    send_plain(state, account_id, sender, target_buffer, body)
}

/// A ban mask from whatever was typed.
///
/// A bare nick becomes `nick!*@*`, which is what somebody means by "ban them".
/// Anything already carrying a `!` or `@` is a mask and is left exactly as
/// written - turning `*!*@example.com` into `*!*@example.com!*@*` would ban
/// nobody while appearing to work.
pub(super) fn ban_mask(target: &str) -> String {
    let target = target.trim();
    if target.contains('!') || target.contains('@') {
        target.to_string()
    } else {
        format!("{target}!*@*")
    }
}

/// Splits `/mode` into what it acts on and what it does.
///
/// `/mode +o someone` in a channel means that channel; `/mode #other +o me`
/// names one. The difference is whether the first word looks like a target
/// rather than a mode string, and a mode string is the thing starting with
/// `+` or `-`.
pub(super) fn split_mode_target<'a>(current: &'a str, arg: &'a str) -> (&'a str, &'a str) {
    let arg = arg.trim();
    let first = arg.split_whitespace().next().unwrap_or("");
    if !first.is_empty() && !first.starts_with('+') && !first.starts_with('-') {
        let rest = arg[first.len()..].trim_start();
        return (first, rest);
    }
    (current, arg)
}

/// The letter a channel mode is written with.
///
/// `ChannelMode` carries the ones the crate knows and `Unknown(c)` the rest,
/// which is most of them on a real network - so the letter is what gets stored
/// rather than the enum.
pub(super) fn mode_letter(mode: &ChannelMode) -> char {
    match mode {
        ChannelMode::Ban => 'b',
        ChannelMode::Exception => 'e',
        ChannelMode::InviteException => 'I',
        ChannelMode::InviteOnly => 'i',
        ChannelMode::Key => 'k',
        ChannelMode::Limit => 'l',
        ChannelMode::Moderated => 'm',
        ChannelMode::NoExternalMessages => 'n',
        ChannelMode::RegisteredOnly => 'r',
        ChannelMode::Secret => 's',
        ChannelMode::ProtectedTopic => 't',
        ChannelMode::Oper => 'o',
        ChannelMode::Voice => 'v',
        ChannelMode::Founder => 'q',
        ChannelMode::Admin => 'a',
        ChannelMode::Halfop => 'h',
        ChannelMode::Unknown(c) => *c,
        // The crate may grow variants; a letter nobody here knows is still
        // better shown than swallowed.
        _ => '?',
    }
}

pub(super) fn mode_word(mode: &ChannelMode) -> &'static str {
    match mode {
        ChannelMode::Ban => "a ban on",
        ChannelMode::Exception => "a ban exception for",
        _ => "an invite exception for",
    }
}

pub(super) fn set_channel_mode(sender: &Sender, channel: &str, mode: ChannelMode, add: bool, nick: &str) -> Result<()> {
    if nick.is_empty() {
        bail!("this command requires a nick");
    }
    let m = if add { Mode::Plus(mode, Some(nick.to_string())) } else { Mode::Minus(mode, Some(nick.to_string())) };
    sender.send(Command::ChannelMODE(channel.to_string(), vec![m])).map_err(|e| anyhow!(e))
}

pub(super) fn send_plain(state: &AppState, account_id: &str, sender: &Sender, target: &str, body: &str) -> Result<()> {
    // Longer than a line. Without `draft/multiline` the server truncates at
    // its own limit and the rest is simply gone, which is the one failure
    // here that loses what somebody wrote - so where the network offers the
    // capability the message goes as one message in several pieces, and where
    // it does not the pieces go as separate messages rather than as a cut.
    let pieces = drafts::split_for_wire(body, drafts::SAFE_LINE);
    if pieces.len() > 1 {
        let own_nick = state.runtime.irc_current_nick(account_id).unwrap_or_default();
        if state.runtime.irc_has_cap(account_id, "draft/multiline") {
            drafts::send_multiline(sender, target, &pieces)?;
            // Recorded whole, because whole is what was sent - the pieces are
            // a transport detail and putting them in the log as separate
            // lines would be showing the reader the envelope.
            if !state.runtime.irc_has_cap(account_id, "echo-message") {
                state.runtime.record_message(state, account_id, target, buffer_kind_hint(target), &own_nick, body, false, "chat", None, None, false, None, Vec::new(), Vec::new(), None);
            }
            return Ok(());
        }
        for piece in &pieces {
            send_plain(state, account_id, sender, target, piece)?;
        }
        return Ok(());
    }

    // Where the server echoes what we send, it is the echo that gets written:
    // it carries the server's id and time, and it is proof the message was
    // actually delivered rather than merely handed over. Without the
    // capability nothing comes back, so the local copy is still the only
    // copy - same as libpurple's write_im/write_chat firing for locally-sent
    // messages too.
    let echoes = state.runtime.irc_has_cap(account_id, "echo-message");
    if !echoes {
        sender.send_privmsg(target, body)?;
        let own_nick = state.runtime.irc_current_nick(account_id).unwrap_or_default();
        state.runtime.record_message(state, account_id, target, buffer_kind_hint(target), &own_nick, body, false, "chat", None, None, false, None, Vec::new(), Vec::new(), None);
        return Ok(());
    }

    let label = next_label();
    let mut msg = Message::from(Command::PRIVMSG(target.to_string(), body.to_string()));
    if state.runtime.irc_has_cap(account_id, "labeled-response") {
        msg.tags = Some(vec![irc::proto::message::Tag("label".to_string(), Some(label.clone()))]);
    }
    sender.send(msg)?;
    pending_sends().lock().unwrap().insert(
        label,
        PendingSend {
            account_id: account_id.to_string(),
            target: target.to_string(),
            body: body.to_string(),
            sent: std::time::Instant::now(),
        },
    );
    Ok(())
}

/// A send waiting for the server to say what became of it.
pub(super) struct PendingSend {
    account_id: String,
    target: String,
    body: String,
    sent: std::time::Instant,
}

/// How long to wait for an echo before writing the message locally anyway.
///
/// A server that granted `echo-message` and then does not echo would
/// otherwise swallow the message silently, which is the one outcome worse
/// than a duplicate. Long enough that a slow network is not mistaken for a
/// broken server.
pub(super) const ECHO_GRACE: Duration = Duration::from_secs(6);

pub(super) fn pending_sends() -> &'static std::sync::Mutex<HashMap<String, PendingSend>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<HashMap<String, PendingSend>>> = std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}

/// A label nothing else will be using. Per process, which is the scope a label
/// has to be unique in.
pub(super) fn next_label() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!("moho-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// Writes anything the server never answered about.
///
/// Run from the ISON tick and after each incoming message, so on a busy
/// connection it is immediate and on a silent one it is at worst a poll late.
/// Both cheap: the map is empty except in the moment between a send and its
/// echo.
pub(super) fn sweep_pending_sends(state: &AppState) {
    let overdue: Vec<PendingSend> = {
        let mut all = pending_sends().lock().unwrap();
        if all.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        let stale: Vec<String> = all
            .iter()
            .filter(|(_, p)| now.duration_since(p.sent) > ECHO_GRACE)
            .map(|(label, _)| label.clone())
            .collect();
        stale.into_iter().filter_map(|label| all.remove(&label)).collect()
    };
    for send in overdue {
        let own_nick = state.runtime.irc_current_nick(&send.account_id).unwrap_or_default();
        state.runtime.record_message(
            state,
            &send.account_id,
            &send.target,
            buffer_kind_hint(&send.target),
            &own_nick,
            &send.body,
            false,
            "chat",
            None,
            None,
            false,
            None,
            Vec::new(),
            Vec::new(),
            None,
        );
    }
}

/// Drops the record of a send the server has now accounted for.
pub(super) fn settle_send(msg: &Message) {
    let Some(label) = msg
        .tags
        .as_ref()
        .and_then(|tags| tags.iter().find(|t| t.0 == "label"))
        .and_then(|t| t.1.clone())
    else {
        return;
    };
    pending_sends().lock().unwrap().remove(&label);
}
