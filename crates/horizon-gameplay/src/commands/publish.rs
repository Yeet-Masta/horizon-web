//! `/publish` and `/unpublish`.
use super::host::{CommandHost, R};
use horizon_client::{
    command::{Arg, Context},
    text::Text,
};

/// What `/publish` and `/unpublish` need from the game: the LAN world it opens and closes.
pub trait PublishHost: CommandHost {
    /// The port the world is open on, if it is: the hosted server's published port, else the
    /// port of the LAN settings.
    fn lan_port(&self) -> Option<u16>;
    /// Opens the world to the LAN on `port` (guests may run commands when `guest_commands`):
    /// false when the game hosts no server or the server fails to publish.
    fn open_to_lan(&mut self, port: u16, guest_commands: bool) -> bool;
    /// Closes the world to the LAN: false when it was not open.
    fn close_to_lan(&mut self) -> bool;
}

/// `PublishCommand.publish` with `IntegratedServer.publishServer`.
pub fn publish(host: &mut dyn PublishHost, ctx: &Context) -> R<i32> {
    let allow = matches!(ctx.get("allowCommands"), Some(Arg::Bool(true)));
    let port = match ctx.get("port") {
        Some(Arg::Int(p)) => *p,
        // HttpUtil.getAvailablePort.
        _ => std::net::TcpListener::bind((
            if horizon_std::env::var_os("HORIZON_TEST_LOOPBACK").is_some_and(|v| v == "1") {
                "127.0.0.1"
            } else {
                "0.0.0.0"
            },
            0,
        ))
        .and_then(|l| l.local_addr())
        .map_or(25564, |a| i32::from(a.port())),
    };
    if let Some(port) = host.lan_port() {
        return Err(host.error("commands.publish.alreadyPublished.lan", &[&port]));
    }
    // A dedicated server is open from the start (`lan_port` above).
    if !host.open_to_lan(port as u16, allow) {
        return Err(host.error("commands.publish.failed", &[]));
    }
    host.feedback(publish_started(port));
    Ok(port)
}

/// `PublishCommand.getSuccessMessage`: the port as a copy-on-click link
/// (`ComponentUtils.copyOnClickText`), whose value a Shift-click types into the chat box.
pub fn publish_started(port: i32) -> Text {
    Text::translate(
        "commands.publish.started.lan",
        &[Text::copyable(port.to_string())],
    )
}

/// `/unpublish`.
pub fn unpublish(host: &mut dyn PublishHost) -> R<i32> {
    if !host.close_to_lan() {
        return Err(host.error("commands.unpublish.notPublished", &[]));
    }
    host.feedback(Text::translate("commands.unpublish.success", &[]));
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::publish_started;
    use horizon_client::{
        lang::{self, Language, ResourceSource},
        text::{ClickEvent, GREEN, Text},
    };

    /// The template, so the test needs no prepared resources.
    struct Vanilla;
    impl ResourceSource for Vanilla {
        fn read(&self, path: &str) -> Option<Vec<u8>> {
            (path == "assets/minecraft/lang/en_us.json").then(|| {
                br#"{
                    "commands.publish.started.lan": "Local game hosted on port %s",
                    "chat.square_brackets": "[%s]"
                }"#
                .to_vec()
            })
        }
    }

    /// The link a click copies and a Shift-click types, and only it: the
    /// sentence around it has no click.
    #[test]
    fn the_published_port_is_a_copy_on_click_link() {
        lang::scoped(Language::load(&[&Vanilla], "en_us"), || {
            let message = publish_started(25565);
            let port = "25565";
            let link = message
                .spans
                .iter()
                .find(|span| span.style.click.is_some())
                .unwrap_or_else(|| panic!("{} has no link", message.plain()));
            assert_eq!(
                link.style.click,
                Some(ClickEvent::CopyToClipboard(port.into()))
            );
            assert_eq!(link.style.insertion.as_deref(), Some(port));
            assert_eq!(link.style.color, Some(GREEN));
            assert_eq!(
                link.style.hover.as_deref().map(Text::plain),
                Some("chat.copy.click".to_owned())
            );
            assert!(
                message.plain().contains(&format!("[{port}]")),
                "{}",
                message.plain()
            );
        });
    }
}
