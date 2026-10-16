//! App-wide commands: their actions, keybindings, the macOS menu bar, and
//! the entries of the command palette. The workspace handles every action
//! here; one table feeds the palette, the menus and Settings › Keyboard.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::*;

actions!(
    workspace,
    [
        NewConsole,
        CommandPalette,
        OpenSettings,
        CloseTab,
        NextTab,
        PreviousTab,
        UseSystemAppearance,
        UseLightAppearance,
        UseDarkAppearance,
        CheckForUpdates,
        OpenDocs,
        ReportIssue,
        SupportOnKofi,
        Quit,
    ]
);

pub const DOCS_URL: &str = "https://joaogabriel55.github.io/savoia-studio/";
pub const KOFI_URL: &str = "https://ko-fi.com/O5I528IC3A";
pub const NEW_CONSOLE_KEYS: &str = "secondary-t";

/// A command as the palette and the keyboard list show it.
pub struct Command {
    pub label: &'static str,
    pub icon: Icon,
    /// Extra words the palette matches on.
    pub keywords: &'static [&'static str],
    pub keys: Option<&'static str>,
    pub action: fn() -> Box<dyn Action>,
}

macro_rules! command {
    ($label:literal, $icon:expr, $action:expr, $keys:expr, [$($kw:literal),*]) => {
        Command {
            label: $label,
            icon: Icon::new($icon),
            keywords: &[$($kw),*],
            keys: $keys,
            action: || Box::new($action),
        }
    };
}

/// Palette order: most used first.
pub fn commands() -> Vec<Command> {
    vec![
        command!(
            "New console",
            Lucide::SquareTerminal,
            NewConsole,
            Some(NEW_CONSOLE_KEYS),
            ["sql", "query", "tab"]
        ),
        command!(
            "Settings",
            Lucide::Settings,
            OpenSettings,
            Some("secondary-,"),
            ["preferences", "options"]
        ),
        command!(
            "Close tab",
            IconName::Close,
            CloseTab,
            Some("secondary-w"),
            []
        ),
        command!(
            "Next tab",
            Lucide::ArrowRight,
            NextTab,
            Some("ctrl-tab"),
            []
        ),
        command!(
            "Previous tab",
            Lucide::ArrowLeft,
            PreviousTab,
            Some("ctrl-shift-tab"),
            []
        ),
        command!(
            "Theme: match system",
            Lucide::SunMoon,
            UseSystemAppearance,
            None,
            ["appearance", "auto"]
        ),
        command!(
            "Theme: light",
            Lucide::Sun,
            UseLightAppearance,
            None,
            ["appearance"]
        ),
        command!(
            "Theme: dark",
            Lucide::Moon,
            UseDarkAppearance,
            None,
            ["appearance"]
        ),
        command!(
            "Check for updates",
            Lucide::RefreshCw,
            CheckForUpdates,
            None,
            ["upgrade", "version"]
        ),
        command!(
            "Open documentation",
            Lucide::BookOpen,
            OpenDocs,
            None,
            ["help", "docs"]
        ),
        command!(
            "Report an issue",
            Lucide::Bug,
            ReportIssue,
            None,
            ["bug", "feedback", "github"]
        ),
        command!(
            "Support Savoia on Ko-fi",
            Lucide::Coffee,
            SupportOnKofi,
            None,
            ["donate", "sponsor", "kofi"]
        ),
        command!(
            "Quit Savoia Studio",
            Lucide::Power,
            Quit,
            Some("secondary-q"),
            ["exit"]
        ),
    ]
}

/// Shortcuts that only work in one view, listed after the commands in
/// Settings › Keyboard. Bound in their own modules; keep these in step.
pub const VIEW_SHORTCUTS: &[(&str, &str)] = &[
    ("Run statement at cursor (console)", "secondary-enter"),
    ("Run whole script (console)", "secondary-shift-enter"),
    ("Format SQL (console)", crate::console::FORMAT_KEYS),
];

const PALETTE_KEYS: &str = "secondary-shift-p";

pub fn init(cx: &mut App) {
    let mut bindings = vec![KeyBinding::new(PALETTE_KEYS, CommandPalette, None)];
    for command in commands() {
        if let Some(keys) = command.keys {
            let action = (command.action)();
            let binding = KeyBinding::load(keys, action, None, false, None, &DummyKeyboardMapper);
            bindings.push(binding.expect("a valid keystroke"));
        }
    }
    cx.bind_keys(bindings);

    cx.set_menus([
        Menu::new("Savoia Studio").items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Check for Updates…", CheckForUpdates),
            MenuItem::separator(),
            MenuItem::action("Quit Savoia Studio", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Console", NewConsole),
            MenuItem::action("Close Tab", CloseTab),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette…", CommandPalette),
            MenuItem::separator(),
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PreviousTab),
        ]),
        Menu::new("Help").items([
            MenuItem::action("Documentation", OpenDocs),
            MenuItem::action("Report an Issue", ReportIssue),
            MenuItem::action("Support Savoia on Ko-fi", SupportOnKofi),
        ]),
    ]);
}

/// The palette's own shortcut, for the keyboard list.
pub fn palette_keys() -> &'static str {
    PALETTE_KEYS
}

/// `keys` as this platform writes them, for hints in text: "⌘T" on macOS,
/// "Ctrl+T" elsewhere.
pub fn keys_label(keys: &str) -> String {
    Keystroke::parse(keys)
        .map(|keys| Kbd::format(&keys))
        .unwrap_or_default()
}
