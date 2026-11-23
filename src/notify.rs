//! Desktop notification when the computer is not set up for the plugin (Linux).
//!
//! The editor's banner is only seen by users who open the plugin's settings. This
//! notification appears on its own, when OpenDeck starts (or at the first failed key
//! press): it says what is missing, how to fix it, and has a button that opens the setup
//! guide.
//!
//! - Inside the OpenDeck Flatpak, the sandbox cannot reach the notification service
//!   directly: the notification goes through the desktop portal
//!   (org.freedesktop.portal.Notification) and is shown as coming from OpenDeck.
//! - Otherwise it uses the standard org.freedesktop.Notifications service.
//!
//! This is the one place where the engine produces user-facing text: a notification is
//! not shown by the editor, so it cannot be translated there.

/// Title, one line per problem, button label.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only Linux shows the notification
pub struct Texts {
    pub title: &'static str,
    pub lines: Vec<&'static str>,
    pub button: &'static str,
}

/// Notification texts for the given setup codes, in English or French.
pub fn texts(problems: &[&str], french: bool) -> Texts {
    let line = |code: &str| -> Option<&'static str> {
        Some(match (code, french) {
            ("uinput_missing", false) => {
                "Macros cannot play: the uinput kernel module is not loaded. Run: sudo modprobe uinput"
            }
            ("uinput_missing", true) => {
                "Les macros ne peuvent pas être jouées : le module noyau uinput n'est pas chargé. Lancez : sudo modprobe uinput"
            }
            ("uinput_denied", false) => {
                "Macros cannot play: no access to /dev/uinput. Run: sudo usermod -aG input $USER, then log out and back in."
            }
            ("uinput_denied", true) => {
                "Les macros ne peuvent pas être jouées : pas d'accès à /dev/uinput. Lancez : sudo usermod -aG input $USER, puis fermez et rouvrez votre session."
            }
            ("no_keyboards", false) => {
                "Macros cannot be recorded: no keyboard can be read. Your user must be in the input group (same command)."
            }
            ("no_keyboards", true) => {
                "Les macros ne peuvent pas être enregistrées : aucun clavier lisible. Votre utilisateur doit être dans le groupe input (même commande)."
            }
            _ => return None,
        })
    };
    Texts {
        title: if french {
            "Keyboard Macros : configuration nécessaire"
        } else {
            "Keyboard Macros: setup needed"
        },
        lines: problems.iter().filter_map(|c| line(c)).collect(),
        button: if french { "Ouvrir le guide d'installation" } else { "Open the setup guide" },
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Texts;
    use futures_util::StreamExt;
    use std::collections::HashMap;
    use zbus::{Connection, Proxy};
    use zbus::{OwnedVariant, Variant};

    const NOTIFICATION_ID: &str = "keymacro-setup";
    const ACTION: &str = "open-guide";

    fn in_flatpak() -> bool {
        std::path::Path::new("/.flatpak-info").exists()
    }

    /// Shows the notification, then waits for a click on it (for the rest of the session)
    /// and calls `on_guide` when the user asks for the guide.
    pub async fn show(texts: Texts, on_guide: impl Fn() + Send + 'static) -> zbus::Result<()> {
        let body = texts.lines.join("\n");
        let conn = Connection::session().await?;
        if in_flatpak() {
            let proxy = Proxy::new(
                &conn,
                "org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Notification",
            )
            .await?;
            let mut clicks = proxy.receive_signal("ActionInvoked").await?;
            let button: HashMap<&str, Variant> =
                HashMap::from([("label", Variant::from(texts.button)), ("action", Variant::from(ACTION))]);
            let notification: HashMap<&str, Variant> = HashMap::from([
                ("title", Variant::from(texts.title)),
                ("body", Variant::from(body.as_str())),
                ("priority", Variant::from("high")),
                ("default-action", Variant::from(ACTION)),
                ("buttons", Variant::from(vec![button])),
            ]);
            proxy.call_method("AddNotification", &(NOTIFICATION_ID, notification)).await?;
            // ActionInvoked(app_id, notification_id, action, parameter)
            while let Some(signal) = clicks.next().await {
                if let Ok((_, id, action, _)) =
                    signal.body().deserialize::<(String, String, String, Vec<OwnedVariant>)>()
                    && id == NOTIFICATION_ID
                    && action == ACTION
                {
                    on_guide();
                }
            }
        } else {
            let proxy = Proxy::new(
                &conn,
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            )
            .await?;
            let mut clicks = proxy.receive_signal("ActionInvoked").await?;
            let actions = vec!["default", texts.button, ACTION, texts.button];
            let hints: HashMap<&str, Variant> = HashMap::from([("urgency", Variant::from(2_u8))]);
            // Notify(app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout)
            let reply = proxy
                .call_method(
                    "Notify",
                    &(
                        "Keyboard Macros",
                        0_u32,
                        "input-keyboard",
                        texts.title,
                        body.as_str(),
                        actions,
                        hints,
                        0_i32,
                    ),
                )
                .await?;
            let id: u32 = reply.body().deserialize()?;
            // ActionInvoked(id, action_key)
            while let Some(signal) = clicks.next().await {
                if let Ok((clicked, _)) = signal.body().deserialize::<(u32, String)>()
                    && clicked == id
                {
                    on_guide();
                }
            }
        }
        Ok(())
    }
}

/// Shows the setup notification (Linux only; other systems have their own prompts).
pub async fn show_setup(problems: Vec<&'static str>, french: bool, on_guide: impl Fn() + Send + 'static) {
    let texts = texts(&problems, french);
    if texts.lines.is_empty() {
        return;
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = linux::show(texts, on_guide).await {
        crate::log!("could not show the setup notification: {e}");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (texts, on_guide);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_linux_setup_code_has_both_languages() {
        for code in ["uinput_missing", "uinput_denied", "no_keyboards"] {
            assert_eq!(texts(&[code], false).lines.len(), 1, "{code} en");
            assert_eq!(texts(&[code], true).lines.len(), 1, "{code} fr");
        }
        assert!(texts(&["unknown"], false).lines.is_empty());
    }
}
