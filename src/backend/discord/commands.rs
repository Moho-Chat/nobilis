//! Slash commands, and the three ways a bot can answer one.
//!
//! A message, a row of buttons, or a form. All three arrive as interactions
//! rather than as messages, which is why none of this is in `messages`: an
//! interaction is a request this client makes on somebody's behalf and then
//! waits to be answered.

use super::*;

#[cfg(test)]
mod modal_tests {
    use super::modal_fields;
    use serde_json::json;

    /// A modal as Discord sends one: fields are text inputs nested inside
    /// action rows, style 1 for a line and 2 for a box.
    #[test]
    fn a_form_is_read_into_its_fields() {
        let modal = json!({
            "title": "Report a thing",
            "custom_id": "report_form",
            "components": [
                { "type": 1, "components": [
                    { "type": 4, "custom_id": "subject", "label": "Subject", "style": 1, "required": true, "max_length": 100 }
                ]},
                { "type": 1, "components": [
                    { "type": 4, "custom_id": "details", "label": "What happened", "style": 2, "required": false,
                      "placeholder": "as much as you like" }
                ]}
            ]
        });
        let fields = modal_fields(&modal);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0]["customId"], "subject");
        assert_eq!(fields[0]["long"], false);
        assert_eq!(fields[0]["required"], true);
        assert_eq!(fields[0]["maxLength"], 100);
        // The second is a paragraph, which is a different box to draw.
        assert_eq!(fields[1]["long"], true);
        assert_eq!(fields[1]["required"], false);
        assert_eq!(fields[1]["placeholder"], "as much as you like");
    }

    #[test]
    fn anything_that_is_not_a_text_input_is_left_alone() {
        // Discord has begun putting other things in modals; a client that
        // drew a button as a text box would be worse than one that ignored
        // it, since the form would send a field the bot never asked for.
        let modal = json!({
            "components": [
                { "type": 1, "components": [ { "type": 2, "custom_id": "press", "label": "Press" } ] },
                { "type": 1, "components": [ { "type": 4, "custom_id": "name", "label": "Name", "style": 1 } ] }
            ]
        });
        let fields = modal_fields(&modal);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0]["customId"], "name");
    }

    #[test]
    fn a_form_with_nothing_in_it_is_no_fields_rather_than_an_error() {
        assert!(modal_fields(&json!({})).is_empty());
        assert!(modal_fields(&json!({ "components": [] })).is_empty());
    }
}

/// The slash commands this conversation offers.
///
/// Asked of Discord per channel rather than per guild, because that is the
/// question with the right answer: a bot can be installed guild-wide and
/// still be unusable in the channel somebody is typing in, and the same
/// endpoint is what Discord's own client asks when a `/` is typed.
pub async fn list_commands(state: &AppState, account_id: &str, buffer_id: &str, query: &str) -> Result<Value> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    let mut params: Vec<(&str, String)> = vec![
        // Type 1 is a slash command. The other two - the ones on the
        // right-click menus for a message or a person - have nowhere to be
        // offered from here.
        ("type", "1".to_string()),
        ("include_applications", "true".to_string()),
        ("limit", "25".to_string()),
    ];
    if !query.is_empty() {
        params.push(("query", query.to_string()));
    }
    let resp = http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/application-commands/search"))
        .query(&params)
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("reading the commands")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "reading the commands"));
    }
    let answer: Value = resp.json().await.context("reading the commands")?;

    // Which bot each command belongs to, so the list can say who answers it -
    // two servers commonly have a /ban that does different things.
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for app in answer["applications"].as_array().cloned().unwrap_or_default() {
        if let (Some(id), Some(name)) = (app["id"].as_str(), app["name"].as_str()) {
            names.insert(id.to_string(), name.to_string());
        }
    }
    let commands: Vec<Value> = answer["application_commands"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|command| {
            let application_id = command["application_id"].as_str().unwrap_or_default().to_string();
            json!({
                "id": command["id"],
                "name": command["name"],
                "description": command["description"],
                "applicationId": application_id,
                "application": names.get(&application_id),
                // Discord wants the command handed back to it as it was given,
                // so it travels to the client and back rather than being
                // rebuilt from the parts this list happens to show.
                "command": command,
            })
        })
        .collect();
    Ok(json!({ "commands": commands }))
}

/// What goes after a command's name, written the way a manual page would.
///
/// Built from the command's own description of its arguments, because that is
/// the only place it exists: `/wordle` and `/ban somebody [reason]` are the
/// same kind of line to a menu, and one of them has to be assembled.
pub fn command_usage(command: &Value) -> String {
    let Some(options) = command["options"].as_array() else { return String::new() };
    options
        .iter()
        // Subcommands and groups are a menu of their own rather than an
        // argument, and writing them as one would promise something this
        // client cannot yet do.
        .filter(|o| !matches!(o["type"].as_i64(), Some(1) | Some(2)))
        .map(|o| {
            let name = o["name"].as_str().unwrap_or("arg");
            if o["required"].as_bool().unwrap_or(false) {
                format!("<{name}>")
            } else {
                format!("[{name}]")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs a slash command, or presses something on a message.
///
/// One function because Discord has one endpoint: an interaction says what
/// kind it is and carries the data for that kind. Both need the gateway
/// session id, which is Discord's way of asking that the thing doing this be
/// a client somebody is actually using.
pub(super) async fn interact(state: &AppState, account_id: &str, buffer_id: &str, kind: i64, application_id: &str, data: Value) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    let session_id = state
        .runtime
        .discord_gateway_session(account_id)
        .context("this account is not connected to Discord right now")?;
    let mut payload = json!({
        "type": kind,
        "application_id": application_id,
        "channel_id": channel_id,
        "session_id": session_id,
        "data": data,
        // Discord dedupes on this, the same way it does for a message.
        "nonce": snowflake_at(chrono::Utc::now().timestamp()),
    });
    if let Some(guild_id) = state.runtime.get_discord_guild(buffer_id) {
        payload["guild_id"] = json!(guild_id);
    }
    // Where this was done, for the answer that may not say. A modal arrives
    // as its own dispatch moments later and does not always name the channel
    // it belongs to; the conversation somebody just acted in is the answer.
    state.runtime.set_discord_last_interaction(account_id, buffer_id);
    let resp = send_write(
        http_client()
        .post(format!("{API_BASE}/interactions"))
        .header("Authorization", &cfg.token)
        .json(&payload)
        )
    .await
        .context("sending the interaction")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "sending that"));
    }
    // Nothing comes back but a 204: whatever the bot does about it arrives as
    // an ordinary message, or as an edit to the one that was pressed.
    Ok(())
}

/// A form a bot asked for, in the shape a window can draw.
///
/// Discord's third answer to a command, after "here is a message" and "here
/// are some buttons": a modal - a little form to fill in and send back. The
/// fields are text inputs inside action rows, the same nesting messages use,
/// and the only two kinds are one line and several.
pub fn modal_fields(modal: &Value) -> Vec<Value> {
    modal["components"]
        .as_array()
        .into_iter()
        .flatten()
        // An action row holds the field; a field outside one is not a shape
        // Discord sends, but reading both costs nothing.
        .flat_map(|row| {
            row["components"]
                .as_array()
                .map(|inner| inner.to_vec())
                .unwrap_or_else(|| vec![row.clone()])
        })
        .filter(|field| field["type"].as_i64() == Some(4))
        .filter_map(|field| {
            let custom_id = field["custom_id"].as_str()?.to_string();
            Some(json!({
                "customId": custom_id,
                "label": field["label"].as_str().unwrap_or("").to_string(),
                // Style 2 is Discord's "paragraph": a box rather than a line.
                "long": field["style"].as_i64() == Some(2),
                "placeholder": field["placeholder"].as_str().unwrap_or(""),
                "value": field["value"].as_str().unwrap_or(""),
                "required": field["required"].as_bool().unwrap_or(true),
                "minLength": field["min_length"].as_i64(),
                "maxLength": field["max_length"].as_i64(),
            }))
        })
        .collect()
}

/// Sends a filled-in form back.
///
/// Interaction type 5, carrying the fields in the nesting they arrived in -
/// each value inside the action row it belongs to, which is what Discord
/// validates against the modal it sent.
pub async fn submit_modal(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    application_id: &str,
    custom_id: &str,
    modal_id: &str,
    values: &[(String, String)],
) -> Result<()> {
    let rows: Vec<Value> = values
        .iter()
        .map(|(field, value)| {
            json!({ "type": 1, "components": [{ "type": 4, "custom_id": field, "value": value }] })
        })
        .collect();
    let data = json!({
        "id": modal_id,
        "custom_id": custom_id,
        "components": rows,
    });
    interact(state, account_id, buffer_id, 5, application_id, data).await
}

/// Runs one of the commands `list_commands` offered.
pub async fn run_command(state: &AppState, account_id: &str, buffer_id: &str, command: &Value, options: Value) -> Result<()> {
    let application_id = command["application_id"].as_str().context("that command says which bot it belongs to")?;
    let data = json!({
        "version": command["version"],
        "id": command["id"],
        "name": command["name"],
        "type": command["type"],
        "options": options,
        // Handed back as it was given. Discord validates the command against
        // its own copy and refuses a description that does not match.
        "application_command": command,
        "attachments": [],
    });
    interact(state, account_id, buffer_id, 2, application_id, data).await
}

/// Presses a button, or answers a menu.
pub async fn use_component(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    message_id: &str,
    custom_id: &str,
    is_select: bool,
    values: Vec<String>,
) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    // Which bot to tell is on the message that carries the control, not on
    // the control itself - so it is read from the message rather than guessed.
    let resp = http_client()
        .get(format!("{API_BASE}/channels/{channel_id}/messages/{message_id}"))
        .header("Authorization", &cfg.token)
        .send()
        .await
        .context("finding out whose button that is")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "finding out whose button that is"));
    }
    let message: Value = resp.json().await.context("reading that message")?;
    let application_id = message["application_id"]
        .as_str()
        .or_else(|| message["author"]["id"].as_str())
        .context("that message does not say which bot posted it")?
        .to_string();

    let mut data = json!({
        // 2 is a button and 3 a menu, which is the one place these numbers
        // have to survive: Discord reads them back.
        "component_type": if is_select { 3 } else { 2 },
        "custom_id": custom_id,
    });
    if is_select {
        data["values"] = json!(values);
    }
    interact_with_message(state, account_id, buffer_id, &application_id, data, message_id).await
}

/// The same as `interact`, naming the message a control belongs to.
pub(super) async fn interact_with_message(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    application_id: &str,
    data: Value,
    message_id: &str,
) -> Result<()> {
    let cfg = state.accounts.get_discord(account_id).context("account not connected")?;
    let channel_id = state.runtime.get_discord_channel(buffer_id).context("no known Discord channel for this conversation")?;
    let session_id = state
        .runtime
        .discord_gateway_session(account_id)
        .context("this account is not connected to Discord right now")?;
    let mut payload = json!({
        "type": 3,
        "application_id": application_id,
        "channel_id": channel_id,
        "message_id": message_id,
        "session_id": session_id,
        "data": data,
        "nonce": snowflake_at(chrono::Utc::now().timestamp()),
    });
    if let Some(guild_id) = state.runtime.get_discord_guild(buffer_id) {
        payload["guild_id"] = json!(guild_id);
    }
    let resp = send_write(
        http_client()
        .post(format!("{API_BASE}/interactions"))
        .header("Authorization", &cfg.token)
        .json(&payload)
        )
    .await
        .context("sending the interaction")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("{}", discord_error_text(status, &text, "pressing that"));
    }
    Ok(())
}
