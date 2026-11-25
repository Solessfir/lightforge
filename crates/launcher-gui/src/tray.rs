//! A StatusNotifierItem and its small DBusMenu, using the portal's D-Bus client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use ashpd::zbus::{self, zvariant::{ObjectPath, OwnedValue, Value}};
use futures_util::{SinkExt, future::{Either, select}};
use tokio::sync::mpsc;

use crate::{Lightforge, Message, ProductCode, Task};

const ITEM: &str = "/StatusNotifierItem";
const MENU: &str = "/MenuBar";

/// How long a hidden Lightforge waits for a tray. A shell reload takes the tray
/// away for about a second, and must not bring the window back.
pub(crate) const TRAY_WAIT: Duration = Duration::from_secs(5);
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

#[derive(Debug, Clone)]
pub(crate) enum Event {
    Available(bool),
    Show,
    Quit,
    Play(ProductCode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Game {
    pub code: ProductCode,
    pub label: String,
    pub enabled: bool,
    /// The game's mark, as its tile shows it.
    pub icon: Option<PathBuf>,
}

#[derive(Default)]
struct Games {
    entries: Vec<Game>,
    ids: Vec<ProductCode>,
    revision: u32,
    /// PNG bytes per mark, encoded once: the menu is rebuilt on every message.
    icons: Option<HashMap<PathBuf, Option<Vec<u8>>>>,
}

static GAMES: RwLock<Games> = RwLock::new(Games {
    entries: Vec::new(),
    ids: Vec::new(),
    revision: 0,
    icons: None,
});

/// Menu icon size; hosts scale it down to their row height.
const MENU_ICON: u32 = 32;

fn menu_icon(path: &Path) -> Option<Vec<u8>> {
    let image = image::io::Reader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?;
    let mut png = Vec::new();
    image
        .thumbnail(MENU_ICON, MENU_ICON)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageOutputFormat::Png)
        .ok()?;
    Some(png)
}

/// A power glyph, drawn here because hosts do not always resolve theme icon names.
#[allow(clippy::cast_possible_truncation, reason = "coverage is clamped to 0..=1")]
static QUIT_ICON: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| {
    let size = MENU_ICON as f32;
    let (center, radius, stroke) = ((size / 2.0, size / 2.0 + 1.0), size * 0.27, size * 0.075);
    let image = image::RgbaImage::from_fn(MENU_ICON, MENU_ICON, |x, y| {
        let (dx, dy) = (x as f32 + 0.5 - center.0, y as f32 + 0.5 - center.1);
        // The ring is open at the top, around the stem.
        let ring = match dy < 0.0 && dx.abs() < -dy * 0.85 {
            true => f32::INFINITY,
            false => (dx.hypot(dy) - radius).abs(),
        };
        let stem = match dy {
            dy if dy > 0.0 => dx.hypot(dy),
            dy if dy < -radius - stroke => dx.hypot(dy + radius + stroke),
            _ => dx.abs(),
        };
        let coverage = (stroke / 2.0 + 0.5 - ring.min(stem)).clamp(0.0, 1.0);
        image::Rgba([0x9a, 0x9a, 0x9a, (coverage * 255.0) as u8])
    });
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageOutputFormat::Png)
        .expect("encoding a PNG in memory");
    png
});

pub(crate) fn set_games(games: Vec<Game>) {
    GAMES.write().expect("tray games lock").update(games);
}

impl Games {
    fn update(&mut self, games: Vec<Game>) {
        if self.entries == games {
            return;
        }
        // Hosts may send an old menu's click after a product was removed.
        for game in &games {
            if !self.ids.contains(&game.code) {
                self.ids.push(game.code.clone());
            }
            if let Some(path) = &game.icon {
                self.icons.get_or_insert_default().entry(path.clone()).or_insert_with(|| menu_icon(path));
            }
        }
        self.entries = games;
        self.revision = self.revision.wrapping_add(1);
    }

    fn game(&self, id: i32) -> Option<&Game> {
        let code = self.ids.get(usize::try_from(id.checked_sub(100)?).ok()?)?;
        self.entries.iter().find(|game| &game.code == code)
    }

    fn item_ids(&self) -> Vec<i32> {
        self.entries.iter().map(|game| {
            100 + i32::try_from(self.ids.iter().position(|code| code == &game.code).expect("registered game")).expect("menu fits i32")
        }).chain([2]).collect()
    }

    fn action(&self, id: i32) -> Option<Event> {
        match id {
            2 => Some(Event::Quit),
            _ => self.game(id).filter(|game| game.enabled).map(|game| Event::Play(game.code.clone())),
        }
    }
}

impl Lightforge {
    pub(crate) fn start_in_tray(&mut self) -> Option<Task<Message>> {
        if !self.config.setup_complete || self.config_error.is_some()
            || self.console() || crate::fit::under_gamescope()
        {
            return None;
        }
        self.tray_hidden = true;
        self.window_focused = false;
        Some(Task::perform(crate::tasks::after(TRAY_WAIT), |()| Message::TrayWaitOver))
    }

    /// Wayland cannot hide a winit window; the daemon keeps the app alive instead.
    pub(crate) fn hide_to_tray(&mut self) -> Option<Task<Message>> {
        if !self.tray_available || self.console() || crate::fit::under_gamescope() {
            return None;
        }
        self.tray_hidden = true;
        self.window_focused = false;
        self.close_panels();
        Some(iced::window::close(self.window_id))
    }

    pub(crate) fn restore_from_tray(&mut self) -> Task<Message> {
        if self.tray_hidden {
            let mut settings = crate::window_settings();
            settings.decorations = self.splash.is_none() && !self.console();
            let (id, open) = iced::window::open(settings);
            self.window_id = id;
            self.tray_hidden = false;
            return open.discard().chain(self.window_mode());
        }
        self.tray_hidden = false;
        let mode = match self.console() {
            true => iced::window::Mode::Fullscreen,
            false => iced::window::Mode::Windowed,
        };
        iced::window::oldest().and_then(move |id| {
            iced::window::set_mode(id, mode)
                .chain(iced::window::minimize(id, false))
                .chain(iced::window::gain_focus(id))
                .chain(iced::window::request_user_attention(id, Some(iced::window::UserAttention::Informational)))
        })
    }
}

pub(crate) fn events() -> impl futures_util::Stream<Item = Event> {
    iced::stream::channel(8, async |mut output| {
        loop {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let connection = match connect(sender).await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::debug!("system tray unavailable: {error}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            let mut registered_owner = None;
            let mut available = false;
            let mut menu_revision = 0;
            loop {
                let host = tokio::time::timeout(
                    Duration::from_secs(2),
                    register(&connection, registered_owner.as_deref()),
                ).await.ok().and_then(Result::ok).flatten();
                let now_available = host.is_some();
                registered_owner = host;
                let revision = GAMES.read().expect("tray games lock").revision;
                if revision != menu_revision {
                    let emitter = zbus::object_server::SignalEmitter::new(&connection, MENU).expect("static menu path");
                    if let Err(error) = Menu::layout_updated(&emitter, revision, 0).await {
                        tracing::debug!("tray menu update failed: {error}");
                    }
                    menu_revision = revision;
                }
                if available != now_available {
                    available = now_available;
                    if output.send(Event::Available(available)).await.is_err() {
                        return;
                    }
                }
                if connection.is_closed() {
                    break;
                }
                match select(Box::pin(receiver.recv()), Box::pin(tokio::time::sleep(Duration::from_secs(1)))).await {
                    Either::Left((Some(event), _)) => {
                        if output.send(event).await.is_err() {
                            return;
                        }
                    }
                    Either::Left((None, _)) => break,
                    Either::Right(_) => {}
                }
            }
            if available && output.send(Event::Available(false)).await.is_err() {
                return;
            }
        }
    })
}

async fn connect(sender: mpsc::UnboundedSender<Event>) -> zbus::Result<zbus::Connection> {
    zbus::connection::Builder::session()?
        .serve_at(ITEM, Item(sender.clone()))?
        .serve_at(MENU, Menu(sender))?
        .build().await
}

async fn register(connection: &zbus::Connection, registered: Option<&str>) -> zbus::Result<Option<String>> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let owner = bus.get_name_owner(WATCHER.try_into()?).await?.to_string();
    let watcher = zbus::proxy::Builder::<zbus::Proxy<'_>>::new(connection)
        .destination(WATCHER)?
        .path(WATCHER_PATH)?
        .interface(WATCHER)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build().await?;
    if !watcher.get_property::<bool>("IsStatusNotifierHostRegistered").await? {
        return Ok(None);
    }
    if registered != Some(owner.as_str()) {
        watcher.call::<_, _, ()>("RegisterStatusNotifierItem", &(ITEM,)).await?;
    }
    Ok(Some(owner))
}

/// Width, height and ARGB32 pixels, as StatusNotifierItem sends icons.
type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

struct Item(mpsc::UnboundedSender<Event>);

#[zbus::interface(name = "org.kde.StatusNotifierItem", crate = "ashpd::zbus")]
impl Item {
    fn activate(&self, _x: i32, _y: i32) { let _ = self.0.send(Event::Show); }
    fn secondary_activate(&self, _x: i32, _y: i32) { let _ = self.0.send(Event::Show); }
    fn context_menu(&self, _x: i32, _y: i32) {}
    fn scroll(&self, _delta: i32, _orientation: &str) {}
    #[zbus(property)]
    fn category(&self) -> &str { "ApplicationStatus" }
    #[zbus(property)]
    fn id(&self) -> &str { crate::APP_ID }
    #[zbus(property)]
    fn title(&self) -> &str { "Lightforge" }
    #[zbus(property)]
    fn status(&self) -> &str { "Active" }
    #[zbus(property)]
    fn window_id(&self) -> u32 { 0 }
    #[zbus(property)]
    fn icon_name(&self) -> &str { "" }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        let Ok(image) = image::load_from_memory(include_bytes!("../../../packaging/icons/hicolor/32x32/apps/pro.tuxcore.Lightforge.png")) else {
            return Vec::new();
        };
        let mut pixels = image.into_rgba8().into_raw();
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.rotate_right(1);
        }
        vec![(32, 32, pixels)]
    }
    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str { "" }
    #[zbus(property)]
    fn attention_icon_name(&self) -> &str { "" }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool { false }
    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'static> { ObjectPath::from_static_str_unchecked(MENU) }
    #[zbus(property)]
    fn tool_tip(&self) -> (&str, Pixmaps, &str, &str) {
        (crate::APP_ID, Vec::new(), "Lightforge", "Open Lightforge or quit from the tray menu")
    }
}

type Properties = HashMap<String, OwnedValue>;
type Layout = (i32, Properties, Vec<OwnedValue>);

fn properties(id: i32, names: &[&str], games: &Games) -> Properties {
    let mut properties = Properties::new();
    if id == 0 {
        properties.insert("children-display".into(), Value::from("submenu").try_to_owned().expect("static string"));
    } else if let Some((label, enabled)) = match id {
        2 => Some(("Quit".to_owned(), true)),
        _ => games.game(id).map(|game| (game.label.clone(), game.enabled)),
    } {
        properties.insert("label".into(), Value::from(label).try_to_owned().expect("static string"));
        if let Some(png) = games.game(id).and_then(|game| games.icons.as_ref()?.get(game.icon.as_ref()?)?.as_ref()) {
            properties.insert("icon-data".into(), Value::from(png.clone()).try_to_owned().expect("byte array"));
        }
        if id == 2 {
            properties.insert("icon-data".into(), Value::from(QUIT_ICON.clone()).try_to_owned().expect("byte array"));
        }
        properties.insert("enabled".into(), enabled.into());
        properties.insert("visible".into(), true.into());
    }
    properties.retain(|name, _| names.is_empty() || names.contains(&name.as_str()));
    properties
}

fn layout(id: i32, depth: i32, names: &[&str], games: &Games) -> Layout {
    let children = match id == 0 && depth != 0 {
        true => games.item_ids().into_iter().map(|id| {
            Value::from(layout(id, 0, names, games)).try_to_owned().expect("menu contains no file descriptors")
        }).collect(),
        false => Vec::new(),
    };
    (id, properties(id, names, games), children)
}

struct Menu(mpsc::UnboundedSender<Event>);

#[zbus::interface(name = "com.canonical.dbusmenu", crate = "ashpd::zbus")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 { 3 }
    #[zbus(property)]
    fn text_direction(&self) -> &str { "ltr" }
    #[zbus(property)]
    fn status(&self) -> &str { "normal" }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> { Vec::new() }
    fn get_layout(&self, parent_id: i32, recursion_depth: i32, property_names: Vec<&str>) -> (u32, Layout) {
        let games = GAMES.read().expect("tray games lock");
        (games.revision, layout(parent_id, recursion_depth, &property_names, &games))
    }
    fn get_group_properties(&self, ids: Vec<i32>, property_names: Vec<&str>) -> Vec<(i32, Properties)> {
        let games = GAMES.read().expect("tray games lock");
        let ids = if ids.is_empty() { [0].into_iter().chain(games.item_ids()).collect() } else { ids };
        ids.into_iter().map(|id| (id, properties(id, &property_names, &games))).collect()
    }
    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedValue> {
        properties(id, &[], &GAMES.read().expect("tray games lock")).remove(name).ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown menu property".into()))
    }
    fn event(&self, id: i32, event_id: &str, _data: Value<'_>, _timestamp: u32) {
        if event_id == "clicked" {
            if let Some(event) = GAMES.read().expect("tray games lock").action(id) {
                let _ = self.0.send(event);
            }
        }
    }
    fn event_group(&self, events: Vec<(i32, &str, Value<'_>, u32)>) -> Vec<i32> {
        for (id, event_id, data, timestamp) in events {
            self.event(id, event_id, data, timestamp);
        }
        Vec::new()
    }
    fn about_to_show(&self, _id: i32) -> bool { true }
    fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) { (ids, Vec::new()) }
    #[zbus(signal)]
    async fn layout_updated(emitter: &zbus::object_server::SignalEmitter<'_>, revision: u32, parent: i32) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ashpd::zvariant::Type;

    #[test]
    fn hidden_start_requires_completed_desktop_setup() {
        let mut app = Lightforge::load_state(None);
        assert!(app.start_in_tray().is_none());
        assert!(!app.tray_hidden);
        app.config.setup_complete = true;
        app.config.steamos_use = Some(lightforge_core::config::SteamOsUse::Console);
        assert!(app.start_in_tray().is_none());
        app.config.steamos_use = Some(lightforge_core::config::SteamOsUse::Desktop);
        app.config_error = Some("invalid config".into());
        assert!(app.start_in_tray().is_none());
        app.config_error = None;
        if !crate::fit::under_gamescope() {
            assert!(app.start_in_tray().is_some());
            assert!(app.tray_hidden);
            assert!(!app.window_focused);
        }
    }

    #[test]
    fn hidden_start_stays_hidden_with_a_host_and_recovers_without_one() {
        if crate::fit::under_gamescope() {
            return;
        }
        let mut app = Lightforge::load_state(None);
        app.config.setup_complete = true;
        let _ = app.start_in_tray();
        let _ = app.update(Message::Tray(Event::Available(true)));
        let _ = app.update(Message::TrayWaitOver);
        assert!(app.tray_hidden);
        let _ = app.update(Message::Tray(Event::Available(false)));
        let _ = app.update(Message::Tray(Event::Available(true)));
        let _ = app.update(Message::TrayWaitOver);
        assert!(app.tray_hidden, "a tray that comes back in time, as on a shell reload, keeps the window hidden");
        let _ = app.update(Message::Tray(Event::Available(false)));
        assert!(app.tray_hidden, "the tray gets time to come back");
        let _ = app.update(Message::TrayWaitOver);
        assert!(!app.tray_hidden);
        assert!(!app.window_focused);

        let _ = app.start_in_tray();
        let _ = app.update(Message::TrayWaitOver);
        assert!(!app.tray_hidden);

        let _ = app.start_in_tray();
        let _ = app.update(Message::Raise);
        let restored_window = app.window_id;
        let _ = app.update(Message::TrayWaitOver);
        assert_eq!(app.window_id, restored_window);
    }

    #[test]
    fn tray_icon_uses_the_embedded_argb_pixmap_without_theme_lookup() {
        let (sender, _) = mpsc::unbounded_channel();
        let item = Item(sender);
        assert!(item.icon_name().is_empty());
        let pixmaps = item.icon_pixmap();
        let (width, height, pixels) = &pixmaps[0];
        assert_eq!((*width, *height), (32, 32));
        assert_eq!(pixels.len(), 32 * 32 * 4);
        let source = image::load_from_memory(include_bytes!("../../../packaging/icons/hicolor/32x32/apps/pro.tuxcore.Lightforge.png")).unwrap().into_rgba8();
        for (argb, rgba) in pixels.chunks_exact(4).zip(source.pixels()) {
            assert_eq!(argb, [rgba[3], rgba[0], rgba[1], rgba[2]]);
        }
    }

    #[test]
    fn menu_has_the_dbusmenu_layout_and_actions() {
        assert_eq!(Layout::SIGNATURE.to_string(), "(ia{sv}av)");
        let (id, _, children) = layout(0, -1, &[], &Games::default());
        assert_eq!(id, 0);
        assert_eq!(children.len(), 1);
        assert!(layout(0, 0, &[], &Games::default()).2.is_empty());
        assert!(properties(1, &["label"], &Games::default()).is_empty());
        let quit = Vec::<u8>::try_from(properties(2, &["icon-data"], &Games::default()).remove("icon-data").unwrap()).unwrap();
        assert_eq!(image::load_from_memory(&quit).unwrap().width(), MENU_ICON);
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let menu = Menu(sender);
        menu.event(1, "hovered", Value::from(0i32), 0);
        assert!(receiver.try_recv().is_err());
        menu.event(1, "clicked", Value::from(0i32), 0);
        assert!(receiver.try_recv().is_err());
        menu.event(2, "clicked", Value::from(0i32), 0);
        assert!(matches!(receiver.try_recv(), Ok(Event::Quit)));
    }

    #[test]
    fn game_actions_keep_their_ids_and_reject_removed_or_disabled_games() {
        let retail = Game { code: ProductCode::Retail, label: "World of Warcraft".into(), enabled: true, icon: None };
        let classic = Game { code: ProductCode::Classic, label: "WoW Classic".into(), enabled: true, icon: None };
        let mut games = Games::default();
        games.update(vec![retail.clone(), classic.clone()]);
        assert_eq!(games.item_ids(), vec![100, 101, 2]);
        assert_eq!(layout(0, -1, &[], &games).2.len(), 3);
        assert!(matches!(games.action(100), Some(Event::Play(ProductCode::Retail))));
        let revision = games.revision;
        games.update(vec![retail.clone(), classic.clone()]);
        assert_eq!(games.revision, revision);
        games.update(vec![classic.clone()]);
        assert_eq!(games.item_ids(), vec![101, 2]);
        assert!(games.action(100).is_none());
        assert!(matches!(games.action(101), Some(Event::Play(ProductCode::Classic))));
        games.update(vec![classic, Game { enabled: false, ..retail }]);
        assert_eq!(games.item_ids(), vec![101, 100, 2]);
        assert!(games.action(100).is_none());
        assert!(!bool::try_from(properties(100, &[], &games).remove("enabled").unwrap()).unwrap());
        assert!(games.action(i32::MIN).is_none());
    }

    #[test]
    fn game_entries_carry_their_mark_as_png_icon_data() {
        let icon = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packaging/icons/hicolor/256x256/apps/pro.tuxcore.Lightforge.png"));
        let mut games = Games::default();
        games.update(vec![
            Game { code: ProductCode::Retail, label: "World of Warcraft".into(), enabled: true, icon: Some(icon) },
            Game { code: ProductCode::Classic, label: "WoW Classic".into(), enabled: true, icon: None },
        ]);
        let mut retail = properties(100, &[], &games);
        assert_eq!(String::try_from(retail.remove("label").unwrap()).unwrap(), "World of Warcraft");
        let png = Vec::<u8>::try_from(retail.remove("icon-data").unwrap()).unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (MENU_ICON, MENU_ICON));
        assert!(!properties(101, &[], &games).contains_key("icon-data"));
    }

    #[test]
    fn missing_tray_never_hides_the_window() {
        let mut app = Lightforge::load_state(None);
        assert!(app.hide_to_tray().is_none());
        assert!(!app.tray_hidden);
        app.tray_available = true;
        app.config.steamos_use = Some(lightforge_core::config::SteamOsUse::Console);
        assert!(app.hide_to_tray().is_none());
    }

    #[test]
    fn hiding_drops_focus_until_the_restored_window_is_focused() {
        if crate::fit::under_gamescope() {
            return;
        }
        let mut app = Lightforge::load_state(None);
        app.tray_available = true;
        assert!(app.window_focused);
        assert!(app.hide_to_tray().is_some());
        assert!(!app.window_focused);
        let _ = app.update(Message::Nav(crate::NavFrom::Pad, crate::pad::Input::Press));
        assert!(app.nav_last.is_none(), "hidden windows ignore controller input");

        let _ = app.restore_from_tray();
        assert!(!app.tray_hidden);
        assert!(!app.window_focused);
        let _ = app.update(Message::WindowFocused(true));
        assert!(app.window_focused);
    }

    #[test]
    fn closing_hides_but_exit_waits_for_a_writer_even_while_hidden() {
        if crate::fit::under_gamescope() {
            return;
        }
        let mut app = Lightforge::load_state(None);
        app.tray_available = true;
        app.repairing = Some(app.selected.code.clone());
        let _ = app.update(Message::CloseRequested(app.window_id));
        assert!(app.tray_hidden);
        assert!(app.closing.is_none());
        let _ = app.update(Message::Exit);
        assert_eq!(app.closing, Some(app.window_id));
        let _ = app.update(Message::Tray(Event::Available(false)));
        let _ = app.update(Message::TrayWaitOver);
        assert!(!app.tray_hidden);
    }

    struct Watcher;

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher", crate = "ashpd::zbus")]
    impl Watcher {
        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool { true }
        fn register_status_notifier_item(&self, service: &str) -> zbus::fdo::Result<()> {
            if service != ITEM {
                return Err(zbus::fdo::Error::InvalidArgs("expected item path".into()));
            }
            Ok(())
        }
    }

    #[test]
    #[ignore = "requires dbus-run-session to isolate the test watcher"]
    fn dbus_roundtrip_registers_actions_and_detects_watcher_loss() {
        use futures_util::StreamExt;
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let watcher = zbus::connection::Builder::session().unwrap()
                .name(WATCHER).unwrap()
                .serve_at(WATCHER_PATH, Watcher).unwrap()
                .build().await.unwrap();
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let connection = connect(sender).await.unwrap();
            let owner = register(&connection, None).await.unwrap();
            assert!(owner.is_some());
            assert_eq!(register(&connection, owner.as_deref()).await.unwrap(), owner);
            let destination = connection.unique_name().unwrap().as_str();
            let menu = zbus::Proxy::new(&connection, destination, MENU, "com.canonical.dbusmenu").await.unwrap();
            let (_, tree): (u32, Layout) = menu.call("GetLayout", &(0i32, -1i32, Vec::<String>::new())).await.unwrap();
            assert_eq!(tree.2.len(), 1);
            menu.call::<_, _, ()>("Event", &(2i32, "clicked", Value::from(0i32), 0u32)).await.unwrap();
            assert!(matches!(receiver.recv().await, Some(Event::Quit)));
            let item = zbus::Proxy::new(&connection, destination, ITEM, "org.kde.StatusNotifierItem").await.unwrap();
            assert_eq!(item.get_property::<String>("Title").await.unwrap(), "Lightforge");
            let pixmap = item.get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap").await.unwrap();
            assert_eq!(pixmap[0].2.len(), 32 * 32 * 4);
            item.call::<_, _, ()>("Activate", &(0i32, 0i32)).await.unwrap();
            assert!(matches!(receiver.recv().await, Some(Event::Show)));
            set_games(vec![Game { code: ProductCode::Retail, label: "World of Warcraft".into(), enabled: true, icon: None }]);
            let (_, tree): (u32, Layout) = menu.call("GetLayout", &(0i32, -1i32, Vec::<String>::new())).await.unwrap();
            assert_eq!(tree.2.len(), 2);
            menu.call::<_, _, ()>("Event", &(100i32, "clicked", Value::from(0i32), 0u32)).await.unwrap();
            assert!(matches!(receiver.recv().await, Some(Event::Play(ProductCode::Retail))));
            set_games(Vec::new());

            let events = events();
            futures_util::pin_mut!(events);
            let available = tokio::time::timeout(Duration::from_secs(5), events.next()).await.unwrap();
            assert!(matches!(available, Some(Event::Available(true))));
            watcher.release_name(WATCHER).await.unwrap();
            let unavailable = tokio::time::timeout(Duration::from_secs(5), events.next()).await.unwrap();
            assert!(matches!(unavailable, Some(Event::Available(false))));
        });
    }
}
