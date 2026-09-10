//! What the server says, and what this client makes of it.
//!
//! One long dispatch over IRC's numerics and commands - which is what an IRC
//! client mostly is. The helpers around it answer the questions the dispatch
//! keeps asking: is this a channel, is this a service, is this notice
//! addressed to a person or to the room.

use super::*;

pub fn is_channel(target: &str) -> bool {
    target.starts_with(['#', '&', '+', '!'])
}

/// Whether a notice sent straight to us is the network talking rather than a
/// person.
///
/// NOTICE is what RFC1459 means for status announcements, and the server and
/// its services use it that way: connection banners, NickServ's "you are now
/// identified", ChanServ refusing something. Those belong in the server tab,
/// which is where nobody is expecting a conversation.
///
/// Everything else is somebody talking, whether or not it is a person. An
/// XDCC bot conducts its whole side of a transfer in notices, and those have
/// to land in the conversation that asked for it.
///
/// Where the two are hard to tell apart the answer leans towards a
/// conversation, because the costs are not symmetrical: a service that gets a
/// query window is a minor untidiness - you talk to services by messaging
/// them anyway - while a bot's reply filed under the server tab is a reply
/// nobody finds.
pub(super) fn notice_is_administrative(prefix: Option<&Prefix>, body: &str) -> bool {
    // The server itself, which is not somebody you can reply to. Its prefix
    // is a bare hostname with no nick attached.
    let Some(Prefix::Nickname(nick, _, host)) = prefix else { return true };
    // A reply to a CTCP query - VERSION, PING, TIME - which travels as a
    // notice but is protocol chatter rather than anything anybody said.
    if body.starts_with('\u{1}') {
        return true;
    }
    is_service(nick, host)
}

/// A network service rather than somebody using the network.
///
/// Almost every network names them the same way, and where the name does not
/// give it away the host usually does. Deliberately not exhaustive:
/// QuakeNet's `Q` and Undernet's `X` are services with names a person could
/// just as easily have, and guessing at those would misfile a real
/// conversation to save a service from having a window.
pub(super) fn is_service(nick: &str, host: &str) -> bool {
    let nick = nick.to_ascii_lowercase();
    // NickServ, ChanServ, MemoServ, HostServ, OperServ, SaslServ, BotServ...
    if nick.ends_with("serv") {
        return true;
    }
    // Network-wide announcements, and the odd network that spells it out.
    if matches!(nick.as_str(), "global" | "services") {
        return true;
    }
    host.to_ascii_lowercase().contains("services")
}

pub(super) fn strip_action(body: &str) -> Option<&str> {
    body.strip_prefix('\u{1}')
        .and_then(|s| s.strip_prefix("ACTION "))
        .and_then(|s| s.strip_suffix('\u{1}'))
}

pub(super) async fn handle_message(
    state: &AppState,
    account_id: &str,
    own_nick: &str,
    sender: &Sender,
    msg: Message,
    nickserv_wait: &Option<NickservWait>,
    channels: &mut HashMap<String, HashMap<String, Who>>,
) {
    let from = msg.source_nickname().unwrap_or("").to_string();
    // Kept because the match below moves `msg.command`, and telling a service
    // apart from a person needs what sent this rather than what it said.
    let prefix = msg.prefix.clone();
    // Only for what somebody actually said. The rest of what arrives here
    // is this client's own narration - join lines, topic notices, error
    // text - which belongs at the moment it is shown rather than at
    // whatever the server stamped the triggering event with.
    let sent_at = server_time(&msg);
    // Carried alongside, because everything below matches on `msg.command`
    // and would otherwise have moved the message out from under it.
    let msg_id = message_id(&msg);
    // Who the network says this came from, as opposed to what they are
    // calling themselves. `account-tag` puts the services account on every
    // line, which is the half that survives a netsplit or a client that was
    // not watching when they identified; `bot-mode` marks a program as one.
    // Both are learned here rather than per arm, so a NOTICE from a bot
    // teaches the roster as much as a PRIVMSG does.
    let tagged_account = message_tag(&msg, "account").map(str::to_string);
    let tagged_bot = message_tag(&msg, "bot").is_some();
    // Anything carrying one of our labels is the server accounting for a send
    // - the echo of it, or a refusal - so the send is no longer outstanding.
    settle_send(&msg);
    // And anything the server never accounted for gets written locally. Here
    // as well as on the ISON tick so that on a busy connection it happens at
    // once; the map is empty except between a send and its echo.
    sweep_pending_sends(state);

    // Fold what the tags said into every roster this person is in. Before
    // the dispatch, so a line that is about to open a conversation has
    // already taught the member list who sent it.
    if !from.is_empty() && (tagged_account.is_some() || tagged_bot) {
        let host = userhost_of(prefix.as_ref());
        let mut touched: Vec<String> = Vec::new();
        for (channel, members) in channels.iter_mut() {
            let Some(who) = members.get_mut(&from) else { continue };
            let before = who.clone();
            who.learn(host.as_deref(), tagged_account.as_deref(), tagged_bot.then_some(true));
            if *who != before {
                touched.push(channel.clone());
            }
        }
        // Redrawn only where something actually changed: every message
        // carries these tags, and re-emitting an identical roster on each
        // one would be a presence event per line of chat.
        for channel in touched {
            if let Some(members) = channels.get(&channel) {
                emit_presence(state, account_id, &channel, members);
            }
        }
    }

    // A piece of a multiline message, rather than a message. Held until the
    // batch closes and then recorded once - without this a paragraph somebody
    // sent as one thing arrives as four lines, and our own echo of a long
    // send comes back as its first piece only.
    if let Some(reference) = message_tag(&msg, "batch").map(str::to_string) {
        let key = (account_id.to_string(), reference);
        let mut open = drafts::open_batches().lock().unwrap();
        if let Some(batch) = open.get_mut(&key) {
            if let Command::PRIVMSG(_, ref body) = msg.command {
                let concat = message_tag(&msg, "draft/multiline-concat").is_some();
                batch.pieces.push((body.clone(), concat));
                return;
            }
        }
    }

    match msg.command {
        Command::PRIVMSG(target, body) => {
            let own = state.runtime.irc_current_nick(account_id).unwrap_or_else(|| own_nick.to_string());
            let (buffer_name, kind) = if is_channel(&target) {
                (target.clone(), "channel")
            } else if from.eq_ignore_ascii_case(&own) {
                // Our own message, echoed back by a server with
                // echo-message: it belongs in the conversation it was sent
                // to, not in one named after ourselves.
                (target.clone(), "dm")
            } else {
                (from.clone(), "dm")
            };
            // Before it is treated as something somebody said. A file offer
            // is CTCP, and recording it as a message put a line of control
            // characters in the log where the offer should have been.
            if let Some(dcc) = dcc::parse_dcc(&body) {
                dcc::incoming(state, account_id, &from, &buffer_name, kind, dcc).await;
                return;
            }
            if let Some(action_body) = strip_action(&body) {
                state.runtime.record_message_at(state, account_id, &buffer_name, kind, &from, action_body, true, "chat", None, msg_id, false, None, Vec::new(), Vec::new(), None, sent_at, None, None);
            } else {
                state.runtime.record_message_at(state, account_id, &buffer_name, kind, &from, &body, false, "chat", None, msg_id, false, None, Vec::new(), Vec::new(), None, sent_at, None, None);
            }
        }

        // A NOTICE addressed directly to us (not a channel) is, per
        // RFC1459's own intent for the command, a service/status
        // announcement rather than a real conversation - this is exactly
        // how NickServ sends registration reminders, identify
        // confirmations, and "invalid password" errors. HexChat (and most
        // other clients) surface these in the server tab rather than
        // opening a query window with "NickServ" - matching that here.
        // A NOTICE actually sent *to a channel* (e.g. from network staff)
        // still belongs in that channel.
        Command::NOTICE(target, body) => {
            if let Some(wait) = nickserv_wait {
                // Still resolves the identify-wait/autojoin-timer side
                // effect - just no longer suppresses showing the message
                // (previously `return`ed here, hiding NickServ's own
                // "you are now identified" confirmation entirely).
                wait.notice(&from, &body);
            }
            if is_channel(&target) {
                state.runtime.record_message(state, account_id, &target, "channel", &from, &body, false, "chat", None, None, false, None, Vec::new(), Vec::new(), None);
            } else if notice_is_administrative(prefix.as_ref(), &body) {
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                state.runtime.record_message(state, account_id, host, "server", &from, &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            } else {
                // Somebody talking, so it goes where talking goes - opening the
                // conversation if there is not one yet. An XDCC bot answers a
                // request entirely in notices: what it is sending, where you
                // are in its queue, why it refused. In the server tab those sit
                // a long way from the request they answer, and the conversation
                // that asked shows nothing but your own message.
                state.runtime.record_message_at(state, account_id, &from, "dm", &from, &body, false, "chat", None, None, false, None, Vec::new(), Vec::new(), None, sent_at, None, None);
            }
        }

        Command::JOIN(channel, account, _) => {
            let members = channels.entry(channel.clone()).or_default();
            let who = members.entry(from.clone()).or_default();
            // `extended-join` puts the services account on the JOIN itself,
            // and the hostmask is on every line's prefix whether or not any
            // capability was granted - so somebody arriving is known as soon
            // as they arrive rather than after a WHOIS.
            who.learn(userhost_of(prefix.as_ref()).as_deref(), account.as_deref(), None);
            if from == own_nick {
                state.runtime.ensure_buffer(state, account_id, &channel, "channel");
                // What was said before we arrived. Asked for on join rather
                // than on connect, because a channel nobody opens is a request
                // for history nobody reads - and asked for at all only where
                // the server granted the capability, since otherwise it is a
                // command answered with an error in the server tab.
                //
                // The messages come back as ordinary ones carrying their own
                // ids, so anything already stored is recognised and stored
                // once. That is what `message-tags` is for.
                if let Some(sender) = state.runtime.irc_sender(account_id) {
                    // What the channel is set to. Servers announce a change
                    // but not the state, so a client that never asks shows
                    // nothing until somebody happens to alter it - which is
                    // why the header sat empty on every channel already
                    // joined.
                    let _ = sender.send(Command::ChannelMODE(channel.clone(), Vec::new()));
                    if state.runtime.irc_has_chathistory(account_id) {
                        let _ = sender.send(chathistory_latest(&channel));
                    }
                }
                // Deliberately not asking who is in here yet. That happens
                // when the conversation is opened - see `who::ask_roster` and
                // the `no-implicit-names` capability - because joining a
                // channel and reading one are different things, and an
                // autojoin list is mostly the first without the second.
            }
            if from != own_nick {
                let line = format!("{from} entered the room");
                state.runtime.record_message(state, account_id, &channel, "channel", "*", &line, false, "join", None, None, false, None, Vec::new(), Vec::new(), None);
            }
            emit_presence(state, account_id, &channel, members);
        }

        Command::PART(channel, reason) => {
            if from == own_nick {
                let buffer_id = crate::model::buffer_id(account_id, &channel);
                state.runtime.remove_buffer(state, &buffer_id);
                channels.remove(&channel);
            } else if let Some(members) = channels.get_mut(&channel) {
                members.remove(&from);
                let line = match reason.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
                    Some(why) => format!("{from} left the room ({why})"),
                    None => format!("{from} left the room"),
                };
                state.runtime.record_message(state, account_id, &channel, "channel", "*", &line, false, "part", None, None, false, None, Vec::new(), Vec::new(), None);
                emit_presence(state, account_id, &channel, members);
            }
        }

        // A rank given or taken after the join.
        //
        // Without this the member list is only ever as accurate as the
        // moment RPL_NAMREPLY arrived: an op handed out afterwards never
        // showed, and one taken away never went, so the list quietly drifted
        // and stayed wrong until the channel was rejoined.
        //
        // Losing a rank sets the member back to none rather than to whatever
        // they held underneath it. IRC sends one prefix per user unless
        // multi-prefix is negotiated, so there is genuinely no way to know
        // from here that somebody who just lost +o still holds +v - the
        // capability that would say so is its own piece of work.
        Command::ChannelMODE(channel, modes) => {
            if let Some(members) = channels.get_mut(&channel) {
                let mut changed = false;
                for m in &modes {
                    let (mode, target, granting) = match m {
                        Mode::Plus(mode, Some(target)) => (mode, target, true),
                        Mode::Minus(mode, Some(target)) => (mode, target, false),
                        // A mode with no nick belongs to the channel rather
                        // than to anybody in it - +m, +t, +i, +k, +l. Those
                        // used to be dropped on the floor, so a channel going
                        // moderated was invisible until a message bounced.
                        Mode::Plus(mode, None) | Mode::Minus(mode, None) => {
                            let adding = matches!(m, Mode::Plus(_, None));
                            let letter = mode_letter(mode);
                            state.runtime.set_irc_channel_mode(state, account_id, &channel, letter, adding);
                            let body = format!("{from} sets {}{letter} on {channel}", if adding { "+" } else { "-" });
                            state.runtime.record_message(state, account_id, &channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                            continue;
                        }
                        _ => continue,
                    };
                    // A ban or an exception carries a mask, not a nick, so it
                    // re-ranks nobody - but it is the channel changing and is
                    // worth saying out loud.
                    if matches!(mode, ChannelMode::Ban | ChannelMode::Exception | ChannelMode::InviteException) {
                        let body = format!(
                            "{from} {} {} {target}",
                            if granting { "adds" } else { "removes" },
                            mode_word(mode),
                        );
                        state.runtime.record_message(state, account_id, &channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                        continue;
                    }
                    let Some(rank) = rank_from_mode(mode) else { continue };
                    let Some(slot) = members.get_mut(target.as_str()) else { continue };
                    slot.rank = if granting { rank } else { MemberRank::None };
                    changed = true;
                    let body = format!(
                        "{from} {} {} {} {target}",
                        if granting { "gives" } else { "takes" },
                        rank_word(rank),
                        if granting { "to" } else { "from" },
                    );
                    state.runtime.record_message(state, account_id, &channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                }
                if changed {
                    emit_presence(state, account_id, &channel, members);
                }
            }
        }

        // Being removed from a channel by somebody else.
        //
        // Previously silent: no message, and the buffer sat there looking
        // joined until the next attempt to speak into it failed.
        Command::KICK(channel, target, reason) => {
            let reason = reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
            if target == own_nick {
                // Said in the server buffer rather than the channel: the
                // channel buffer is about to be removed, so anything written
                // there goes to something nobody can open.
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                let body = match reason {
                    Some(r) => format!("Kicked from {channel} by {from} ({r})"),
                    None => format!("Kicked from {channel} by {from}"),
                };
                state.runtime.record_message(state, account_id, host, "server", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                let buffer_id = crate::model::buffer_id(account_id, &channel);
                state.runtime.remove_buffer(state, &buffer_id);
                channels.remove(&channel);
            } else if let Some(members) = channels.get_mut(&channel) {
                members.remove(&target);
                let body = match reason {
                    Some(r) => format!("{target} was kicked by {from} ({r})"),
                    None => format!("{target} was kicked by {from}"),
                };
                state.runtime.record_message(state, account_id, &channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                emit_presence(state, account_id, &channel, members);
            }
        }

        Command::QUIT(reason) => {
            let reason = reason.unwrap_or_default();
            let reason = reason.trim();
            // A netsplit says so in the quit message itself: two server names
            // and nothing else. Everybody on the far side goes in the same
            // second, so they are gathered and reported once instead of one
            // line per person - which on a large network is hundreds of them.
            let split = is_netsplit(reason).then(|| reason.to_string());
            for (channel, members) in channels.iter_mut() {
                if members.remove(&from).is_none() {
                    continue;
                }
                match &split {
                    Some(reason) => {
                        if state.runtime.add_irc_split(account_id, channel, reason, &from) {
                            schedule_split_report(state.clone(), account_id.to_string(), channel.clone(), reason.clone());
                        }
                    }
                    None => {
                        let line = if reason.is_empty() {
                            format!("{from} has quit")
                        } else {
                            format!("{from} has quit ({reason})")
                        };
                        state.runtime.record_message(state, account_id, channel, "channel", "*", &line, false, "part", None, None, false, None, Vec::new(), Vec::new(), None);
                    }
                }
                emit_presence(state, account_id, channel, members);
            }
        }

        Command::NICK(new_nick) => {
            for (channel, members) in channels.iter_mut() {
                // The record follows the name: a nick change is the same
                // person, so their host, account and bot mark come with them.
                if let Some(who) = members.remove(&from) {
                    members.insert(new_nick.clone(), who);
                    let line = format!("{from} is now known as {new_nick}");
                    state.runtime.record_message(state, account_id, channel, "channel", "*", &line, false, "nick", None, None, false, None, Vec::new(), Vec::new(), None);
                    emit_presence(state, account_id, channel, members);
                }
            }
        }

        // --- WHOIS/WHOWAS: one answer assembled from several numerics ---
        //
        // Each of these carries the queried nick as args[1]; the pieces are
        // collected under it and shown when 318 (or 369) says the reply is
        // complete. A numeric for somebody nothing asked about is dropped,
        // so another client sharing this connection cannot open a window
        // here.
        Command::Response(Response::RPL_WHOISUSER, args) | Command::Response(Response::RPL_WHOWASUSER, args) => {
            // args: [me, nick, user, host, "*", :real name]
            if let Some(nick) = args.get(1) {
                state.runtime.add_irc_whois_field(account_id, nick, "nick", json!(nick));
                if let (Some(user), Some(host)) = (args.get(2), args.get(3)) {
                    state.runtime.add_irc_whois_field(account_id, nick, "mask", json!(format!("{nick}!{user}@{host}")));
                }
                if let Some(real) = args.last() {
                    state.runtime.add_irc_whois_field(account_id, nick, "realName", json!(real));
                }
            }
        }
        Command::Response(Response::RPL_WHOISSERVER, args) => {
            if let (Some(nick), Some(server)) = (args.get(1), args.get(2)) {
                let info = args.get(3).cloned().unwrap_or_default();
                state.runtime.add_irc_whois_field(account_id, nick, "server", json!(format!("{server} ({info})")));
            }
        }
        Command::Response(Response::RPL_WHOISOPERATOR, args) => {
            if let Some(nick) = args.get(1) {
                state.runtime.add_irc_whois_field(account_id, nick, "operator", json!(args.last().cloned().unwrap_or_default()));
            }
        }
        Command::Response(Response::RPL_WHOISIDLE, args) => {
            // args: [me, nick, idle seconds, signon unix time, :info]
            if let Some(nick) = args.get(1) {
                if let Some(idle) = args.get(2).and_then(|v| v.parse::<i64>().ok()) {
                    state.runtime.add_irc_whois_field(account_id, nick, "idleSeconds", json!(idle));
                }
                if let Some(signon) = args.get(3).and_then(|v| v.parse::<i64>().ok()) {
                    state.runtime.add_irc_whois_field(account_id, nick, "signOnTs", json!(signon));
                }
            }
        }
        Command::Response(Response::RPL_WHOISCHANNELS, args) => {
            if let (Some(nick), Some(list)) = (args.get(1), args.last()) {
                let channels: Vec<&str> = list.split_whitespace().collect();
                state.runtime.add_irc_whois_field(account_id, nick, "channels", json!(channels));
            }
        }
        Command::Response(Response::RPL_ENDOFWHOIS, args) | Command::Response(Response::RPL_ENDOFWHOWAS, args) => {
            if let Some(nick) = args.get(1) {
                if let Some(whois) = state.runtime.take_irc_whois(account_id, nick) {
                    crate::profile::emit(state, irc_profile(state, account_id, nick, whois, channels));
                }
            }
        }

        // --- away ---
        //
        // 301 arrives two ways: inside a WHOIS reply, and on its own when a
        // message is sent to somebody away. Both mean the same thing, so it
        // feeds the roster either way and only joins the WHOIS if one is
        // being assembled.
        Command::Response(Response::RPL_AWAY, args) => {
            if let Some(nick) = args.get(1) {
                let reason = args.last().cloned().unwrap_or_default();
                state.runtime.add_irc_whois_field(account_id, nick, "away", json!(reason));
                if state.runtime.set_irc_away(account_id, nick, true) {
                    refresh_rosters_containing(state, account_id, nick, channels);
                }
            }
        }
        // The server's own confirmation, which is what gets shown - rather
        // than /away claiming success the moment it is typed.
        Command::Response(Response::RPL_NOWAWAY, args) | Command::Response(Response::RPL_UNAWAY, args) => {
            let text = args.last().cloned().unwrap_or_default();
            let now_away = text.to_lowercase().contains("marked as being away") && !text.to_lowercase().contains("no longer");
            if let Some(own) = state.runtime.irc_current_nick(account_id) {
                if state.runtime.set_irc_away(account_id, &own, now_away) {
                    refresh_rosters_containing(state, account_id, &own, channels);
                }
            }
            let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
            state.runtime.record_message(state, account_id, host, "server", "*", &text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
        }

        // Somebody else going away or coming back, live - the `away-notify`
        // capability. Without it away state is only ever known about people
        // who have just been WHOISed or messaged.
        Command::AWAY(reason) => {
            if state.runtime.set_irc_away(account_id, &from, reason.is_some()) {
                refresh_rosters_containing(state, account_id, &from, channels);
            }
        }

        // args: [nick, "nick1 nick2 ..."] - only those who are on.
        // The watch list, answered by the server rather than polled for.
        Command::Response(code @ (Response::RPL_MONONLINE | Response::RPL_MONOFFLINE), args) => {
            let targets = args.last().map(String::as_str).unwrap_or("");
            apply_monitor(state, account_id, targets, matches!(code, Response::RPL_MONONLINE));
        }

        Command::Response(Response::RPL_ISON, args) => {
            let online = args.last().map(String::as_str).unwrap_or("");
            // Gathered as well as applied: the watch list is settled a poll
            // later, once every chunk of this answer has come back.
            {
                let mut all = ison_replies().lock().unwrap();
                let set = all.entry(account_id.to_string()).or_default();
                for nick in online.split_whitespace() {
                    set.insert(nick.to_lowercase());
                }
            }
            ison_asked().lock().unwrap().insert(account_id.to_string());
            apply_ison(state, account_id, online);
        }

        // The server's own answer that a message went nowhere. Authoritative
        // where the poll above is merely recent: this arrives because of
        // something just sent, so it also corrects the stored presence.
        Command::Response(Response::ERR_NOSUCHNICK, args) => {
            if let Some(nick) = args.get(1) {
                let existing = state
                    .runtime
                    .list_buffers()
                    .into_iter()
                    .find(|b| b.account_id == account_id && b.kind == "dm" && b.name.eq_ignore_ascii_case(nick));
                if let Some(buffer) = existing {
                    let members = json!([{
                        "nick": nick,
                        "userId": nick,
                        "prefix": "",
                        "away": true,
                        "status": "offline",
                    }]);
                    state.runtime.set_presence(&buffer.id, members.clone());
                    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": members }));
                    state.events.emit(
                        "deliveryFailed",
                        json!({
                            "bufferId": buffer.id,
                            "reason": "offline",
                            "text": format!("{nick} is not online. They will not receive this message."),
                        }),
                    );
                }
            }
        }

        // Who somebody is, as opposed to what they are called. Both reply
        // shapes land in the same place: what differs is which fields the
        // server had room for, and `Seen` is what is left once that is
        // resolved.
        Command::Response(Response::RPL_WHOREPLY, ref args) => {
            note_seen(state, account_id, channels, who::read_who(args));
        }

        Command::Raw(ref cmd, ref args) if cmd == "354" => {
            note_seen(state, account_id, channels, who::read_whox(args));
        }

        Command::Response(Response::RPL_NAMREPLY, args) => {
            // args: [nick, symbol, channel, "name1 @name2 +name3 ..."]
            if let (Some(channel), Some(names)) = (args.get(2), args.get(3)) {
                let members = channels.entry(channel.clone()).or_default();
                for raw in names.split_whitespace() {
                    let (rank, entry) = parse_prefixed_nick(raw);
                    // With `userhost-in-names` the entry is `nick!user@host`
                    // rather than a bare nick, so the roster is filled in on
                    // join with no WHO and no round trip. Without it, split
                    // finds nothing and this is the bare nick it always was.
                    let (nick, host) = match entry.split_once('!') {
                        Some((nick, userhost)) => (nick, Some(userhost)),
                        None => (entry, None),
                    };
                    let who = members.entry(nick.to_string()).or_default();
                    who.rank = rank;
                    who.learn(host, None, None);
                }
                emit_presence(state, account_id, channel, members);
            }
        }

        // Sent once on join if the channel has a topic set (args: [nick,
        // channel, topic]) - previously silently dropped, meaning topics
        // never showed up at all under this backend.
        Command::Response(Response::RPL_TOPIC, args) => {
            if let (Some(channel), Some(topic)) = (args.get(1), args.get(2)) {
                let body = format!("Topic for {channel}: {topic}");
                state.runtime.record_message(state, account_id, channel, "channel", "*", &body, false, "topic", None, None, false, None, Vec::new(), Vec::new(), None);
            }
        }

        // A live topic change while we're in the channel.
        Command::TOPIC(channel, Some(topic)) => {
            let body = format!("{from} changed the topic to: {topic}");
            state.runtime.record_message(state, account_id, &channel, "channel", "*", &body, false, "topic", None, None, false, None, Vec::new(), Vec::new(), None);
        }

        // Connection banner (001-005), LUSERS (251-255, 265-266), and MOTD
        // (372/375/376, 422) - previously silently dropped entirely, same
        // gap as topics before the earlier fix. This is exactly the
        // "server-wide, not addressed to any channel or person" content
        // the unclosable server buffer exists for (see model.c's
        // nobilis_buffer_kind) - libpurple's own irc_msg_default() fallback
        // numeric handler wrote to this same buffer for the same reason.
        Command::Response(code, args) if is_connection_banner(code) => {
            // What this server will accept, as opposed to what it was asked
            // to send. Recorded as well as shown, because WHOX is announced
            // here and nowhere else - it is the one ratified IRCv3 extension
            // with no capability of its own to negotiate.
            if code == Response::RPL_ISUPPORT && args.len() > 2 {
                state.runtime.note_irc_isupport(account_id, &args[1..args.len() - 1].join(" "));
            }
            if let Some(text) = banner_text(code, &args) {
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                state.runtime.record_message(state, account_id, host, "server", "*", &text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            }
        }

        // Channel-operation failures (can't join, banned, full, wrong key,
        // invite-only, etc.) - previously silently dropped, meaning a
        // failed /join gave no feedback at all. These happen precisely
        // when there's no channel buffer to show them in (the join never
        // succeeded), so the always-present server buffer is the only
        // reliable place - same as HexChat's server tab.
        // Which capabilities the server actually granted. Requested without
        // waiting for the answer, so this is where we find out - and asking a
        // server for history it never offered is an error in the server tab.
        Command::CAP(_, CapSubCommand::ACK, ref param, ref suffix) => {
            let caps = cap_list(param.as_deref(), suffix.as_deref());
            tracing::debug!("irc[{account_id}]: capabilities granted: {caps}");
            state.runtime.grant_irc_caps(account_id, caps);
        }

        // The frame around a multiline message. Only multiline batches are
        // collected: a chathistory batch contains messages that are each
        // their own message, and folding those together would turn a replayed
        // conversation into a wall of text.
        // `Command::BATCH`, not `Raw`: the crate parses this one into a
        // variant of its own, and a Raw arm for it silently never fires -
        // which is exactly what happened, and why a 659-byte message came
        // back as its two pieces after the reassembly was written.
        Command::BATCH(ref tag, ref sub, ref params) => {
            let kind = sub.as_ref().map(|s| s.to_str().to_string());
            let param = params.as_ref().and_then(|p| p.first().cloned());
            let Some((opening, reference)) = drafts::read_batch_tag(tag) else { return };
            let key = (account_id.to_string(), reference);
            if opening {
                // Case-insensitively: the crate parses the type through an
                // uppercasing enum, so what a server sent as
                // `draft/multiline` arrives as `DRAFT/MULTILINE`. Comparing
                // exactly meant the batch was never opened, every piece fell
                // through as an ordinary message, and the reassembly below
                // had nothing to reassemble.
                if kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("draft/multiline")) {
                    if let Some(target) = param {
                        drafts::open_batches()
                            .lock()
                            .unwrap()
                            .insert(key, drafts::Assembling { target, pieces: Vec::new() });
                    }
                }
                return;
            }
            let Some(batch) = drafts::open_batches().lock().unwrap().remove(&key) else { return };
            if batch.pieces.is_empty() {
                return;
            }
            let body = batch.finish();
            let own = state.runtime.irc_current_nick(account_id).unwrap_or_else(|| own_nick.to_string());
            let kind = buffer_kind_hint(&batch.target);
            // Whose conversation it belongs in, by the same rule a single
            // PRIVMSG follows: a channel is itself, and a direct message is
            // filed under whoever is not us.
            let buffer = if is_channel(&batch.target) || from.eq_ignore_ascii_case(&own) {
                batch.target.clone()
            } else {
                from.clone()
            };
            state.runtime.record_message_at(state, account_id, &buffer, kind, &from, &body, false, "chat", None, msg_id, false, None, Vec::new(), Vec::new(), None, sent_at, None, None);
        }

        // A message somebody took back. IRC never had this; every other
        // service here always did, which is why the entry existed in the menu
        // and did nothing on this one.
        Command::Raw(ref cmd, ref args) if cmd.eq_ignore_ascii_case("REDACT") => {
            let (Some(target), Some(msg_id)) = (args.first(), args.get(1)) else { return };
            let buffer = if is_channel(target) { target.clone() } else { from.clone() };
            state.runtime.delete_message(state, &crate::model::buffer_id(account_id, &buffer), msg_id);
        }

        // Where this conversation has been read up to, according to the
        // server - which is to say, according to whatever other client of
        // this account last looked at it.
        Command::Raw(ref cmd, ref args) if cmd.eq_ignore_ascii_case("MARKREAD") => {
            if let Some((target, at)) = drafts::read_marker(args) {
                state.runtime.note_irc_read_marker(state, account_id, &target, at);
            }
        }

        // A channel that changed its name. Without this it looks like leaving
        // one channel and joining another, and the conversation is split in
        // two with the history stranded in a room nobody is in.
        Command::Raw(ref cmd, ref args) if cmd.eq_ignore_ascii_case("RENAME") => {
            let (Some(old_name), Some(new_name)) = (args.first(), args.get(1)) else { return };
            if let Some(members) = channels.remove(old_name.as_str()) {
                channels.insert(new_name.clone(), members);
            }
            // The buffer's identity is its name, so this is a move rather
            // than a rename: the old one goes and the new one arrives
            // carrying what was said. Ordered that way round on purpose -
            // creating first would briefly show the channel twice.
            state.runtime.rename_irc_buffer(state, account_id, old_name, new_name);
            let reason = args.get(2).map(String::as_str).filter(|r| !r.is_empty());
            let line = match reason {
                Some(why) => format!("{old_name} is now called {new_name} ({why})"),
                None => format!("{old_name} is now called {new_name}"),
            };
            state.runtime.record_message(state, account_id, new_name, "channel", "*", &line, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            if let Some(members) = channels.get(new_name.as_str()) {
                emit_presence(state, account_id, new_name, members);
            }
        }

        // Somebody's realname changed without them reconnecting, which is
        // the whole point of `setname`. Reported where they can be seen; a
        // realname is not shown in a roster, so this is the only place the
        // change is ever visible.
        Command::Raw(ref cmd, ref args) if cmd.eq_ignore_ascii_case("SETNAME") => {
            let name = args.last().map(String::as_str).unwrap_or("");
            if !from.is_empty() && !name.is_empty() {
                let body = format!("{from} is now {name}");
                for (channel, members) in channels.iter() {
                    if members.contains_key(&from) {
                        state.runtime.record_message(state, account_id, channel, "channel", "*", &body, false, "nick", None, None, false, None, Vec::new(), Vec::new(), None);
                    }
                }
            }
        }

        // The server explaining itself in words rather than in a numeric.
        //
        // `FAIL`, `WARN` and `NOTE` are what a modern server uses for
        // anything without a numeric of its own - `<command> <code> [context]
        // :<description>`. Three weights, kept apart: a FAIL is a refusal
        // somebody needs to see, a WARN is worth showing, a NOTE is the
        // server being chatty.
        Command::Raw(ref cmd, ref args)
            if matches!(cmd.to_ascii_uppercase().as_str(), "FAIL" | "WARN" | "NOTE") =>
        {
            let weight = cmd.to_ascii_uppercase();
            // The description is the trailing parameter; everything between
            // the code and it is context the server thought worth naming.
            let description = args.last().map(String::as_str).unwrap_or("");
            let about = args.first().map(String::as_str).unwrap_or("");
            let body = match weight.as_str() {
                "FAIL" if about.is_empty() => description.to_string(),
                "FAIL" => format!("{about} failed: {description}"),
                "WARN" => format!("{description}"),
                _ => description.to_string(),
            };
            if body.is_empty() {
                return;
            }
            // A NOTE is not worth interrupting a conversation with; the other
            // two are about something somebody just tried to do.
            let kind = if weight == "NOTE" { "system" } else { "error" };
            let host = server_buffer(account_id);
            state.runtime.record_message(state, account_id, host, "server", "*", &body, false, kind, None, None, false, None, Vec::new(), Vec::new(), None);
        }

        // What modes a channel currently has, in answer to a bare /mode.
        Command::Response(Response::RPL_CHANNELMODEIS, args) => {
            if let Some(channel) = args.get(1) {
                let modes = args.get(2).map(String::as_str).unwrap_or("");
                state.runtime.set_irc_channel_modes(state, account_id, channel, modes);
                let body = if modes.trim_matches('+').is_empty() {
                    format!("{channel} has no modes set")
                } else {
                    format!("{channel} is {modes}")
                };
                state.runtime.record_message(state, account_id, channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            }
        }

        // The ban list, an entry at a time, in answer to "/mode #chan b".
        // Rendered in the channel rather than gathered into a panel: a ban
        // list is short, it is read once, and it belongs beside the channel
        // it governs.
        Command::Response(code @ (Response::RPL_BANLIST | Response::RPL_EXCEPTLIST | Response::RPL_INVITELIST), args) => {
            if let (Some(channel), Some(mask)) = (args.get(1), args.get(2)) {
                let kind = match code {
                    Response::RPL_BANLIST => "banned",
                    Response::RPL_EXCEPTLIST => "ban exception",
                    _ => "invite exception",
                };
                // Who set it and when, where the server says.
                let by = args.get(3).map(|who| format!(" by {who}")).unwrap_or_default();
                let body = format!("{kind}: {mask}{by}");
                state.runtime.record_message(state, account_id, channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            }
        }
        Command::Response(Response::RPL_ENDOFBANLIST, args) => {
            if let Some(channel) = args.get(1) {
                state.runtime.record_message(state, account_id, channel, "channel", "*", "end of ban list", false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            }
        }

        // Somebody asked us to join something.
        // An invite. Addressed to us it arrives whatever the server offers;
        // addressed to somebody else it arrives only with `invite-notify`,
        // and that one goes to the channel it is about rather than to the
        // server tab, because it is news about a room we are in.
        //
        // Filed as a room event either way - a line written *about* somebody
        // is not a line addressed to them, which is the same rule quit and
        // part lines follow.
        Command::INVITE(ref who, ref channel) => {
            let own = state.runtime.irc_current_nick(account_id).unwrap_or_else(|| own_nick.to_string());
            if who.eq_ignore_ascii_case(&own) {
                let host = server_buffer(account_id);
                let body = format!("{from} invites you to {channel}");
                state.runtime.record_message(state, account_id, host, "server", "*", &body, false, "system", None, None, true, None, Vec::new(), Vec::new(), None);
            } else if channels.contains_key(channel.as_str()) {
                let body = format!("{from} invited {who} to {channel}");
                state.runtime.record_message(state, account_id, channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            }
        }
        // Confirmation that ours went out.
        Command::Response(Response::RPL_INVITING, args) => {
            if let (Some(nick), Some(channel)) = (args.get(1), args.get(2)) {
                // The server answers with the pair either way round depending
                // on how old it is, and both readings say the same thing.
                let body = format!("invited {nick} to {channel}");
                state.runtime.record_message(state, account_id, channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            }
        }

        // The channel directory, gathered rather than printed: a network's
        // list runs to tens of thousands of entries, which is something to
        // search rather than a wall of chat.
        Command::Response(Response::RPL_LISTSTART, _) => state.runtime.begin_irc_channel_list(account_id),
        Command::Response(Response::RPL_LIST, args) => {
            if let Some(channel) = args.get(1) {
                let users = args.get(2).and_then(|u| u.parse::<u32>().ok()).unwrap_or(0);
                let topic = args.get(3).cloned().unwrap_or_default();
                state.runtime.push_irc_channel_list(account_id, channel, users, &topic);
            }
        }
        Command::Response(Response::RPL_LISTEND, _) => state.runtime.finish_irc_channel_list(state, account_id),

        Command::Response(code, args) if is_channel_error(code) => {
            if let Some(text) = channel_error_text(&args) {
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                state.runtime.record_message(state, account_id, host, "server", "*", &text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
            }
        }

        // Somebody composing. A TAGMSG carries tags and nothing else, which
        // is how a client says something that is not a message - and the only
        // one this cares about is `+typing`.
        Command::Raw(ref cmd, ref args) if cmd.eq_ignore_ascii_case("TAGMSG") => {
            if let (Some(target), Some(tags)) = (args.first(), msg.tags.as_ref()) {
                if let Some(value) = tags.iter().find(|t| t.0 == "+typing").and_then(|t| t.1.as_deref()) {
                    note_typing(state, account_id, &from, target, value);
                }
            }
        }

        _ => {}
    }

    let _ = sender; // reserved for future PING/PONG or raw passthrough handling
}

pub(super) fn is_channel_error(code: Response) -> bool {
    matches!(
        code,
        Response::ERR_NOSUCHNICK
            | Response::ERR_NOSUCHCHANNEL
            | Response::ERR_CANNOTSENDTOCHAN
            | Response::ERR_TOOMANYCHANNELS
            | Response::ERR_UNAVAILRESOURCE
            | Response::ERR_NOTONCHANNEL
            | Response::ERR_CHANNELISFULL
            | Response::ERR_INVITEONLYCHAN
            | Response::ERR_BANNEDFROMCHAN
            | Response::ERR_BADCHANNELKEY
    )
}

/// `<nick> <channel-or-nick> :<reason>` for all of these - combine into
/// one readable line, e.g. "#foo: Cannot join channel (+i)".
pub(super) fn channel_error_text(args: &[String]) -> Option<String> {
    let target = args.get(1)?;
    let reason = args.last()?;
    Some(format!("{target}: {reason}"))
}

/// The rank a channel mode letter grants, or None for a mode that is about
/// the channel rather than about a person in it.
///
/// Admin (+a) folds into Founder: it outranks op, and the member model has
/// no separate step for it.
pub(super) fn rank_from_mode(mode: &ChannelMode) -> Option<MemberRank> {
    match mode {
        ChannelMode::Founder | ChannelMode::Admin => Some(MemberRank::Founder),
        ChannelMode::Oper => Some(MemberRank::Op),
        ChannelMode::Halfop => Some(MemberRank::HalfOp),
        ChannelMode::Voice => Some(MemberRank::Voice),
        _ => None,
    }
}

/// What to call a rank in a sentence.
pub(super) fn rank_word(rank: MemberRank) -> &'static str {
    match rank {
        MemberRank::Founder => "founder status",
        MemberRank::Op => "operator status",
        MemberRank::HalfOp => "half-operator status",
        MemberRank::Voice => "voice",
        MemberRank::None => "nothing",
    }
}

/// A message's own id, where the server gives one.
///
/// Used as the message id rather than a generated one, so a message seen twice
/// - replayed history, a reconnect, a batch that overlaps what is already
/// stored - is stored once. That dedup already exists for every other protocol
/// here; IRC could not use it because it had no id to dedup on.
pub(super) fn message_id(msg: &Message) -> Option<String> {
    let tags = msg.tags.as_ref()?;
    tags.iter()
        .find(|tag| tag.0 == "msgid")?
        .1
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// One message tag's value, where the server sent it.
///
/// A tag present with no value ("bot") and a tag present with an empty value
/// are the same thing on the wire, and both mean "this is set" - which is why
/// the emptiness test is left to callers rather than done here. `account`
/// wants a non-empty name; `bot` wants only to know the tag was there.
pub(super) fn message_tag<'a>(msg: &'a Message, name: &str) -> Option<&'a str> {
    let tags = msg.tags.as_ref()?;
    tags.iter().find(|tag| tag.0 == name).map(|tag| tag.1.as_deref().unwrap_or(""))
}

pub(super) fn server_time(msg: &Message) -> Option<i64> {
    let tags = msg.tags.as_ref()?;
    // Tag is a tuple struct of (name, value); matched by field rather than
    // by pattern to save importing it for one line.
    let raw = tags.iter().find(|tag| tag.0 == "time")?.1.as_deref()?;
    Some(chrono::DateTime::parse_from_rfc3339(raw).ok()?.timestamp())
}

/// Folds one WHO answer into the roster it is about.
///
/// Also records the away flag, which is the same fact `away-notify` reports
/// and is worth taking from here too: a channel joined mid-session is full of
/// people whose away state nobody has announced since before we arrived.
pub(super) fn note_seen(
    state: &AppState,
    account_id: &str,
    channels: &mut HashMap<String, HashMap<String, Who>>,
    seen: Option<who::Seen>,
) {
    let Some(seen) = seen else { return };
    state.runtime.set_irc_away(account_id, &seen.nick, seen.away);
    let Some(members) = channels.get_mut(&seen.channel) else { return };
    let entry = members.entry(seen.nick.clone()).or_default();
    entry.learn(Some(&seen.host), seen.account.as_deref(), Some(seen.bot));
    emit_presence(state, account_id, &seen.channel, members);
}

/// The buffer an account's server messages go to.
///
/// An IRC account id is `nick@host` and the server tab is named for the host,
/// which is a fact this file was repeating inline everywhere it needed one.
pub(super) fn server_buffer(account_id: &str) -> &str {
    account_id.split_once('@').map(|(_, host)| host).unwrap_or(account_id)
}

/// The `user@host` off a line's prefix.
///
/// Every line from a person carries one whether or not any capability was
/// granted, which makes it the cheapest source of a hostmask there is - it
/// just only ever covers people who have said or done something. WHO and
/// `userhost-in-names` are what cover the rest of the room.
pub(super) fn userhost_of(prefix: Option<&Prefix>) -> Option<String> {
    match prefix {
        Some(Prefix::Nickname(_, user, host)) if !host.is_empty() => Some(format!("{user}@{host}")),
        _ => None,
    }
}

/// Splits "@+nick" into the rank it carries and the nick itself.
///
/// Every prefix is consumed, not just the first. With multi-prefix granted a
/// server lists all of them, highest first - so the first is the rank, and
/// leaving the rest attached would make "+nick" the person's name.
pub(super) fn parse_prefixed_nick(raw: &str) -> (MemberRank, &str) {
    let mut rest = raw;
    let mut rank = MemberRank::None;
    while let Some(byte) = rest.as_bytes().first() {
        let this = match byte {
            b'~' => MemberRank::Founder,
            b'@' => MemberRank::Op,
            b'%' => MemberRank::HalfOp,
            b'+' => MemberRank::Voice,
            _ => break,
        };
        // The highest is listed first, so only the first one read counts.
        if rank == MemberRank::None {
            rank = this;
        }
        rest = &rest[1..];
    }
    (rank, rest)
}

pub(super) fn buffer_kind_hint(target: &str) -> &'static str {
    if is_channel(target) { "channel" } else { "dm" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(nick: &str, host: &str) -> Prefix {
        Prefix::Nickname(nick.to_string(), "u".to_string(), host.to_string())
    }

    #[test]
    fn the_server_and_its_services_stay_in_the_server_tab() {
        // The server itself, whose prefix carries no nick at all.
        assert!(notice_is_administrative(Some(&Prefix::ServerName("irc.example.net".into())), "*** Looking up your hostname"));
        // And a notice with no prefix, which is the same thing.
        assert!(notice_is_administrative(None, "anything"));

        for (nick, host) in [
            ("NickServ", "services.libera.chat"),
            ("nickserv", "services."),
            ("ChanServ", "services.rizon.net"),
            ("MemoServ", "x"),
            ("SaslServ", "x"),
            ("Global", "x"),
            // Named like a person, but on a services host.
            ("Alis", "services.libera.chat"),
        ] {
            assert!(notice_is_administrative(Some(&person(nick, host)), "hello"), "{nick} should be a service");
        }
    }

    #[test]
    fn a_bot_or_a_person_gets_a_conversation() {
        // The case this exists for: an XDCC bot conducts its whole side of a
        // transfer in notices, and in the server tab they sit nowhere near the
        // request they answer.
        for (nick, host) in [
            ("AxA-TV2", "bot.example.net"),
            ("Ginpachi-Sensei", "user/ginpachi"),
            ("someone", "example.com"),
            // Ends in "server", not "serv" - a real word, not a service.
            ("Observer", "example.com"),
        ] {
            assert!(!notice_is_administrative(Some(&person(nick, host)), "** Sending you pack #5"), "{nick} should get a DM");
        }
    }

    #[test]
    fn a_ctcp_reply_is_not_a_conversation() {
        // VERSION and PING answers come back as notices. They are protocol
        // chatter, and opening a window full of control characters for them
        // would be worse than not showing them at all.
        assert!(notice_is_administrative(Some(&person("someone", "example.com")), "\u{1}VERSION HexChat 2.16\u{1}"));
    }

    /// multi-prefix lists every rank a person holds, highest first. Reading
    /// only the first byte left the rest stuck to the front of the nick.
    #[test]
    fn every_prefix_is_stripped_and_the_highest_wins() {
        assert_eq!(parse_prefixed_nick("@+bob"), (MemberRank::Op, "bob"));
        assert_eq!(parse_prefixed_nick("~@%+bob"), (MemberRank::Founder, "bob"));
        assert_eq!(parse_prefixed_nick("+bob"), (MemberRank::Voice, "bob"));
        assert_eq!(parse_prefixed_nick("bob"), (MemberRank::None, "bob"));
        // A nick is never only prefixes, but it must not panic if it is.
        assert_eq!(parse_prefixed_nick("@"), (MemberRank::Op, ""));
        assert_eq!(parse_prefixed_nick(""), (MemberRank::None, ""));
    }

    /// server-time is what stops replayed history dating itself to the
    /// moment it was read.
    #[test]
    fn a_server_timestamp_is_read_when_the_server_sends_one() {
        let msg = "@time=2026-08-29T12:34:56.789Z :bob!b@h PRIVMSG #chan :hi"
            .parse::<irc::proto::Message>()
            .unwrap();
        assert_eq!(server_time(&msg), Some(1788006896));

        // No tag at all is the ordinary case on a server without the
        // capability, and must fall back rather than resolve to the epoch.
        let bare = ":bob!b@h PRIVMSG #chan :hi".parse::<irc::proto::Message>().unwrap();
        assert_eq!(server_time(&bare), None);
    }

    /// Only the modes that rank a person move the member list; the ones
    /// that configure the channel must not be mistaken for one.
    #[test]
    fn only_person_modes_carry_a_rank() {
        assert_eq!(rank_from_mode(&ChannelMode::Oper), Some(MemberRank::Op));
        assert_eq!(rank_from_mode(&ChannelMode::Voice), Some(MemberRank::Voice));
        assert_eq!(rank_from_mode(&ChannelMode::Halfop), Some(MemberRank::HalfOp));
        assert_eq!(rank_from_mode(&ChannelMode::Founder), Some(MemberRank::Founder));
        // +a outranks op and the member model has no separate step for it.
        assert_eq!(rank_from_mode(&ChannelMode::Admin), Some(MemberRank::Founder));

        for mode in [ChannelMode::Ban, ChannelMode::Moderated, ChannelMode::InviteOnly, ChannelMode::Key, ChannelMode::Limit, ChannelMode::Secret] {
            assert_eq!(rank_from_mode(&mode), None, "{mode:?} is about the channel, not a person in it");
        }
    }

    /// The sentence has to read correctly in both directions, since the same
    /// words are used for granting and for taking away.
    #[test]
    fn a_rank_change_reads_as_a_sentence() {
        assert_eq!(
            format!("bob {} {} {} carol", "gives", rank_word(MemberRank::Op), "to"),
            "bob gives operator status to carol"
        );
        assert_eq!(
            format!("bob {} {} {} carol", "takes", rank_word(MemberRank::Voice), "from"),
            "bob takes voice from carol"
        );
    }

    #[test]
    fn tls_carries_sasl_without_asking() {
        assert!(sasl_transport_ok(true, false));
    }

    #[test]
    fn cleartext_refuses_sasl_by_default() {
        assert!(!sasl_transport_ok(false, false));
    }

    #[test]
    fn cleartext_carries_sasl_when_allowed_on_purpose() {
        assert!(sasl_transport_ok(false, true));
    }

    #[test]
    fn allowing_plaintext_does_not_change_the_tls_case() {
        // The allowance is a floor, not a mode: it only ever answers for the
        // connection that had no encryption to begin with.
        assert!(sasl_transport_ok(true, true));
    }

    /// `userhost-in-names` turns each NAMES entry into `nick!user@host`, so
    /// the split has to happen before the name is used as a key - otherwise
    /// every member is filed under a name nothing else will ever match, and
    /// the roster fills up with strangers who never speak.
    #[test]
    fn a_names_entry_may_carry_the_hostmask_too() {
        let (rank, entry) = parse_prefixed_nick("@ada!ada@example.org");
        assert_eq!(rank, MemberRank::Op);
        assert_eq!(entry.split_once('!'), Some(("ada", "ada@example.org")));

        // And without the capability it is the bare nick it always was.
        let (rank, entry) = parse_prefixed_nick("+bob");
        assert_eq!(rank, MemberRank::Voice);
        assert_eq!(entry.split_once('!'), None);
        assert_eq!(entry, "bob");
    }

}
