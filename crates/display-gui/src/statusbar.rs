//! Status bar item, menu, and AppKit event handling.
//!
//! The menu is built from a cached [`Snapshot`] the background worker maintains,
//! so `populate` never touches the socket and the UI cannot freeze on a wedged
//! daemon (see `client.rs`). Actions are posted to the worker over a channel.
//!
//! Sliders and clickable items carry an integer `tag` that indexes a table the
//! handler owns — that is how an AppKit action, which only hands back the
//! sender, maps to "brightness on display 3" or "apply profile coding".

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DeclaredClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSImage, NSMenu,
    NSMenuDelegate, NSMenuItem, NSSlider, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    NSView,
};
use objc2_foundation::{MainThreadMarker, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;

use crate::client::{Cmd, Snapshot, Worker};

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// What a tagged control drives. Index into `Handler`'s action table.
#[derive(Clone)]
enum Action {
    SetVcp {
        display: String,
        code: u8,
    },
    /// DPMS off via VCP 0xD6 = 0x04.
    PowerOff {
        display: String,
    },
    /// DPMS on via VCP 0xD6 = 0x01. Whether it works depends on the monitor: in
    /// 0x04 (DPMS off) the scaler often still answers I2C and wakes, but a
    /// display in deeper sleep may only wake on input.
    PowerOn {
        display: String,
    },
    /// Pick an enumerated value (input, color preset, orientation, …): write
    /// `code` = `value`. Immediate, like other DDC tools.
    PickValue {
        display: String,
        code: u8,
        value: u8,
    },
    /// Built-in panel "off": set the backlight (brightness) to 0, remembering
    /// the prior value. There is no public per-panel DPMS power for the internal
    /// LCD, so backlight-off is the practical equivalent.
    BacklightOff {
        display: String,
    },
    /// Built-in panel "on": restore the remembered brightness.
    BacklightOn {
        display: String,
    },
    ApplyProfile(String),
    OpenSettings,
    About,
    Quit,
}

/// A control in the settings window. Indexed by an NSControl `tag`.
#[derive(Clone)]
pub enum SettingsAction {
    /// A checkbox: show/hide `code` for the display `key`. Reads sender state.
    ToggleVisible {
        key: String,
        code: u8,
    },
    ApplyProfile(String),
    DeleteProfile(String),
    /// "Save current settings as profile…" — prompts for a name.
    SaveProfilePrompt,
    /// Fetch a display's capability string (chunked read) for the Capabilities tab.
    FetchCaps(String),
    /// Launch-at-login checkbox. Reads sender state.
    ToggleLogin,
}

pub struct HandlerIvars {
    worker: Worker,
    status_item: RefCell<Option<Retained<NSStatusItem>>>,
    /// Tag → action. Rebuilt on every menu open.
    actions: RefCell<Vec<Action>>,
    /// Tag → settings-window control action. Rebuilt when settings opens.
    settings_actions: RefCell<Vec<SettingsAction>>,
    /// Snapshot generation the settings window was last built at, so a timer can
    /// rebuild it only when something it shows actually changed.
    settings_gen: RefCell<u64>,
    /// Brightness to restore when turning a built-in panel's backlight back on,
    /// keyed by display selector.
    backlight_restore: RefCell<std::collections::HashMap<String, u16>>,
}

define_class!(
    #[unsafe(super(objc2::runtime::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DSHandler"]
    #[ivars = HandlerIvars]
    pub struct Handler;

    unsafe impl NSObjectProtocol for Handler {}

    unsafe impl NSApplicationDelegate for Handler {
        // Keep running when the settings window is closed — this is a menu bar
        // app, so the status item is the app, not any window. Without this,
        // AppKit terminates on last-window-closed and the whole app quits when
        // settings closes.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window(&self, _app: &NSApplication) -> bool {
            false
        }
    }

    unsafe impl NSMenuDelegate for Handler {
        // Rebuild from the cached snapshot just before the menu opens, and nudge
        // the worker to refresh so the next open is current. Never blocks.
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let mtm = MainThreadMarker::new().unwrap();
            // Catch panics here: a panic unwinding into AppKit aborts the whole
            // app with a bare "trace trap". Recovering leaves the user with a
            // usable (if sparse) menu and a diagnosable log line instead.
            let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.populate(mtm, menu);
            }));
            if built.is_err() {
                eprintln!("display-gui: recovered from a panic while building the menu");
            }
            self.ivars().worker.post(Cmd::Refresh);
        }
    }

    impl Handler {
        #[unsafe(method(sliderChanged:))]
        fn slider_changed(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let slider: &NSSlider = unsafe { &*(sender as *const AnyObject as *const NSSlider) };
            let tag = unsafe { slider.tag() };
            let value = unsafe { slider.integerValue() } as u16;
            if let Some(Action::SetVcp { display, code }) =
                self.ivars().actions.borrow().get(tag as usize).cloned()
            {
                self.ivars().worker.post(Cmd::Set {
                    display,
                    code,
                    value,
                });
            }
        }

        #[unsafe(method(itemClicked:))]
        fn item_clicked(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let item: &NSMenuItem = unsafe { &*(sender as *const AnyObject as *const NSMenuItem) };
            let tag = unsafe { item.tag() };
            self.run_action(tag as usize);
        }

        // One entry point for every settings-window control; dispatches on tag.
        #[unsafe(method(settingsControl:))]
        fn settings_control(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let control: &objc2_app_kit::NSControl =
                unsafe { &*(sender as *const AnyObject as *const objc2_app_kit::NSControl) };
            let tag = unsafe { control.tag() } as usize;
            // Checkbox state: on == 1.
            let on = unsafe { control.integerValue() } != 0;
            let action = self.ivars().settings_actions.borrow().get(tag).cloned();
            match action {
                Some(SettingsAction::ToggleVisible { key, code }) => {
                    // Checkbox checked means "shown".
                    self.set_visible(&key, code, on);
                }
                Some(SettingsAction::ApplyProfile(name)) => {
                    self.apply_profile(&name);
                    self.reopen_settings();
                }
                Some(SettingsAction::DeleteProfile(name)) => {
                    // Reflect the delete at once; the worker confirms it.
                    self.ivars().worker.forget_profile(&name);
                    self.ivars().worker.post(Cmd::DeleteProfile(name));
                    self.reopen_settings();
                }
                Some(SettingsAction::SaveProfilePrompt) => {
                    let mtm = MainThreadMarker::new().unwrap();
                    if let Some(name) = crate::settings::prompt_name(mtm) {
                        self.ivars().worker.remember_profile(&name);
                        self.ivars().worker.post(Cmd::SaveProfile(name));
                        self.reopen_settings();
                    }
                }
                Some(SettingsAction::FetchCaps(display)) => {
                    self.ivars().worker.post(Cmd::FetchCaps(display));
                    // Result arrives async; the settings timer rebuilds the tab
                    // when the snapshot generation bumps.
                }
                Some(SettingsAction::ToggleLogin) => self.set_launch_at_login(on),
                None => {}
            }
        }

        // Fired by a repeating main-thread timer: rebuild the settings window
        // content when the worker's snapshot has changed, so async results
        // (fetched caps, saved profiles, updated values) appear without the user
        // reopening the window. Cheap when nothing changed.
        #[unsafe(method(settingsTick:))]
        fn settings_tick(&self, _timer: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().unwrap();
            if !crate::settings::is_open() {
                return;
            }
            let gen = self.ivars().worker.generation();
            if *self.ivars().settings_gen.borrow() == gen {
                return;
            }
            *self.ivars().settings_gen.borrow_mut() = gen;
            crate::settings::rebuild_content(mtm, self);
        }
    }
);

impl Handler {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Handler>().set_ivars(HandlerIvars {
            worker: Worker::spawn(),
            status_item: RefCell::new(None),
            actions: RefCell::new(Vec::new()),
            settings_actions: RefCell::new(Vec::new()),
            settings_gen: RefCell::new(0),
            backlight_restore: RefCell::new(std::collections::HashMap::new()),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn run_action(&self, tag: usize) {
        let action = self.ivars().actions.borrow().get(tag).cloned();
        let worker = &self.ivars().worker;
        match action {
            Some(Action::ApplyProfile(name)) => worker.post(Cmd::ApplyProfile(name)),
            // A menu click is the confirmation; power writes are fire-and-forget.
            // 0xD6 = Power Mode: 0x04 off (DPMS), 0x01 on.
            Some(Action::PowerOff { display }) => worker.post(Cmd::Set {
                display,
                code: 0xD6,
                value: 4,
            }),
            Some(Action::PowerOn { display }) => worker.post(Cmd::Set {
                display,
                code: 0xD6,
                value: 1,
            }),
            // Immediate switch, matching MonitorControl/Lunar. For input source,
            // recovery from a no-signal input is the monitor's own buttons.
            Some(Action::PickValue {
                display,
                code,
                value,
            }) => worker.post(Cmd::Set {
                display,
                code,
                value: value as u16,
            }),
            // Built-in panel: no DPMS power, so "off" is backlight to 0. Stash
            // the current brightness first so "on" restores it.
            Some(Action::BacklightOff { display }) => {
                if let Some(cur) = self.builtin_brightness(&display) {
                    if cur > 0 {
                        self.ivars()
                            .backlight_restore
                            .borrow_mut()
                            .insert(display.clone(), cur);
                    }
                }
                worker.post(Cmd::Set {
                    display,
                    code: 0x10,
                    value: 0,
                });
            }
            Some(Action::BacklightOn { display }) => {
                let value = self
                    .ivars()
                    .backlight_restore
                    .borrow()
                    .get(&display)
                    .copied()
                    .unwrap_or(60);
                worker.post(Cmd::Set {
                    display,
                    code: 0x10,
                    value,
                });
            }
            Some(Action::OpenSettings) => {
                crate::settings::open(MainThreadMarker::new().unwrap(), self)
            }
            Some(Action::About) => self.show_about(),
            Some(Action::Quit) => {
                let mtm = MainThreadMarker::new().unwrap();
                NSApplication::sharedApplication(mtm).terminate(None);
            }
            // SetVcp is driven by sliderChanged:, not by a menu click.
            Some(Action::SetVcp { .. }) | None => {}
        }
    }

    /// Adjust the brightness of the external display under the cursor, in
    /// response to a keyboard brightness key. Returns true if it handled the key
    /// (so the tap swallows it); false to let macOS handle it (built-in panel or
    /// a display we do not drive).
    pub fn media_brightness(&self, up: bool) -> bool {
        let Some(id) = crate::mediakeys::display_under_cursor() else {
            return false;
        };
        let snap = self.ivars().worker.snapshot();
        let Some(m) = snap.monitors.iter().find(|m| m.id == id) else {
            return false;
        };
        // Built-in brightness is handled natively by macOS — let the key pass.
        if !m.is_ddc {
            return false;
        }
        let Some(s) = m.sliders.iter().find(|s| s.code == 0x10) else {
            return false; // brightness not readable / hidden — don't swallow
        };
        // ~16 steps across the range, matching macOS's brightness granularity.
        let step = (s.max as i32 / 16).max(1);
        let delta = if up { step } else { -step };
        let new = (s.current as i32 + delta).clamp(0, s.max as i32) as u16;
        self.ivars().worker.post(Cmd::Set {
            display: id.to_string(),
            code: 0x10,
            value: new,
        });
        true
    }

    /// Show the native About panel, crediting the author.
    fn show_about(&self) {
        use objc2_app_kit::NSApplicationActivationPolicy;
        use objc2_foundation::{NSDictionary, NSString};

        let mtm = MainThreadMarker::new().unwrap();
        let app = NSApplication::sharedApplication(mtm);

        // Options for the standard about panel: name, version, and a credits
        // line with the author. Keys are the documented NSAboutPanelOption raw
        // strings.
        let version = NSString::from_str(env!("CARGO_PKG_VERSION"));
        let credits = objc2_foundation::NSAttributedString::from_nsstring(&NSString::from_str(
            "Created by Marcin Spoczynski",
        ));
        let keys = [
            NSString::from_str("ApplicationName"),
            NSString::from_str("ApplicationVersion"),
            NSString::from_str("Credits"),
        ];
        let name = NSString::from_str("Display Studio");
        let vals: [&objc2::runtime::AnyObject; 3] = [&name, &version, &credits];
        let key_refs: [&NSString; 3] = [&keys[0], &keys[1], &keys[2]];
        let options = NSDictionary::from_slices(&key_refs, &vals);

        // Bring the app forward so the panel is visible for an accessory app.
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        app.activate();
        unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }

    /// Current brightness of a display from the cached snapshot (VCP 0x10).
    fn builtin_brightness(&self, selector: &str) -> Option<u16> {
        let id: u32 = selector.parse().ok()?;
        let snap = self.ivars().worker.snapshot();
        let m = snap.monitors.iter().find(|m| m.id == id)?;
        m.sliders.iter().find(|s| s.code == 0x10).map(|s| s.current)
    }

    /// Current cached snapshot, for the settings window.
    pub fn snapshot(&self) -> Snapshot {
        self.ivars().worker.snapshot()
    }

    /// Record the generation the settings window was just built at (so the timer
    /// does not immediately rebuild it).
    pub fn mark_settings_built(&self) {
        *self.ivars().settings_gen.borrow_mut() = self.ivars().worker.generation();
    }

    /// Reset the settings control table (called when the window rebuilds).
    pub fn clear_settings_actions(&self) {
        self.ivars().settings_actions.borrow_mut().clear();
    }

    /// Register a settings control action, returning its tag.
    pub fn register_settings_action(&self, action: SettingsAction) -> isize {
        let mut a = self.ivars().settings_actions.borrow_mut();
        a.push(action);
        (a.len() - 1) as isize
    }

    /// Whether a code is shown in the menu for a display key.
    pub fn is_visible(&self, key: &str, code: u8) -> bool {
        self.ivars()
            .worker
            .config()
            .lock()
            .unwrap()
            .is_visible(key, code)
    }

    /// Toggle a code's menu visibility for a display key, persisting the change.
    pub fn set_visible(&self, key: &str, code: u8, visible: bool) {
        self.ivars()
            .worker
            .config()
            .lock()
            .unwrap()
            .set_visible(key, code, visible);
    }

    pub fn launch_at_login(&self) -> bool {
        self.ivars().worker.config().lock().unwrap().launch_at_login
    }

    /// Set launch-at-login: update the LaunchAgent and persist the preference.
    pub fn set_launch_at_login(&self, on: bool) {
        crate::login::set_enabled(on);
        let cfg = self.ivars().worker.config();
        let mut c = cfg.lock().unwrap();
        c.launch_at_login = on;
        c.save();
    }

    /// Apply a profile (used by the settings Profiles tab).
    pub fn apply_profile(&self, name: &str) {
        self.ivars()
            .worker
            .post(Cmd::ApplyProfile(name.to_string()));
    }

    /// Rebuild the settings window so profile/customization changes show. The
    /// worker updates the snapshot asynchronously, so a just-saved profile may
    /// appear on the next rebuild rather than instantly.
    fn reopen_settings(&self) {
        crate::settings::open(MainThreadMarker::new().unwrap(), self);
    }

    fn register(&self, action: Action) -> isize {
        let mut actions = self.ivars().actions.borrow_mut();
        actions.push(action);
        (actions.len() - 1) as isize
    }

    /// Rebuild the menu from the cached snapshot. Pure UI; no socket I/O.
    fn populate(&self, mtm: MainThreadMarker, menu: &NSMenu) {
        menu.removeAllItems();
        self.ivars().actions.borrow_mut().clear();

        let snap = self.ivars().worker.snapshot();

        if !snap.daemon_up {
            self.add_disabled(mtm, menu, "displayd not running");
        } else if snap.monitors.is_empty() {
            self.add_disabled(mtm, menu, "No displays found");
        } else {
            for m in &snap.monitors {
                self.add_monitor_section(mtm, menu, m);
            }
        }

        self.add_profiles_section(mtm, menu, &snap);

        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let settings = self.action_item(mtm, "Settings…", Action::OpenSettings, "");
        menu.addItem(&settings);
        let about = self.action_item(mtm, "About Display Studio", Action::About, "");
        menu.addItem(&about);
        let quit = self.action_item(mtm, "Quit Display Studio", Action::Quit, "q");
        menu.addItem(&quit);
    }

    fn add_disabled(&self, mtm: MainThreadMarker, menu: &NSMenu, text: &str) {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(text));
        unsafe { item.setEnabled(false) };
        menu.addItem(&item);
    }

    fn add_monitor_section(
        &self,
        mtm: MainThreadMarker,
        menu: &NSMenu,
        m: &crate::client::MonitorView,
    ) {
        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let header = NSMenuItem::new(mtm);
        header.setTitle(&NSString::from_str(&m.name));
        unsafe { header.setEnabled(false) };
        menu.addItem(&header);

        let selector = m.id.to_string();
        // The worker already returns only the visible (adjustable, advertised)
        // sliders per the settings customization, so no filtering here.
        for s in &m.sliders {
            let tag = self.register(Action::SetVcp {
                display: selector.clone(),
                code: s.code,
            });
            let item = self.slider_item(mtm, &s.label, s.current, s.max, tag, s.enabled);
            menu.addItem(&item);
        }

        // Power off/on for DDC displays only — the built-in panel has no DPMS
        // control over this path.
        if m.is_ddc {
            // Unicode glyphs: ⏻ the standard power symbol for off, ☀ (matching
            // the menu-bar icon) for wake. Monochrome, so they sit right in a
            // native menu rather than reading as colour emoji.
            let off = self.action_item(
                mtm,
                "⏻  Off",
                Action::PowerOff {
                    display: selector.clone(),
                },
                "",
            );
            menu.addItem(&off);
            let on = self.action_item(
                mtm,
                "☀  On",
                Action::PowerOn {
                    display: selector.clone(),
                },
                "",
            );
            menu.addItem(&on);

            // Enumerated pickers (Input, Color Preset, Orientation, …): one
            // submenu each. Only pickers with more than one choice reach here.
            for picker in &m.pickers {
                self.add_picker_submenu(mtm, menu, picker, &selector);
            }
        } else {
            // Built-in panel: backlight off/on (no true DPMS power available).
            let off = self.action_item(
                mtm,
                "⏻  Off",
                Action::BacklightOff {
                    display: selector.clone(),
                },
                "",
            );
            menu.addItem(&off);
            let on = self.action_item(
                mtm,
                "☀  On",
                Action::BacklightOn {
                    display: selector.clone(),
                },
                "",
            );
            menu.addItem(&on);
        }
    }

    /// A "<Title> ▸" submenu of enumerated choices, the current one checkmarked.
    /// Clicking writes that value immediately.
    fn add_picker_submenu(
        &self,
        mtm: MainThreadMarker,
        menu: &NSMenu,
        picker: &crate::client::PickerView,
        selector: &str,
    ) {
        let parent = NSMenuItem::new(mtm);
        parent.setTitle(&NSString::from_str(&picker.title));

        let submenu = NSMenu::new(mtm);
        for (value, name) in &picker.values {
            let tag = self.register(Action::PickValue {
                display: selector.to_string(),
                code: picker.code,
                value: *value,
            });
            let item = NSMenuItem::new(mtm);
            item.setTitle(&NSString::from_str(name));
            unsafe {
                item.setTag(tag);
                item.setAction(Some(sel!(itemClicked:)));
                item.setTarget(Some(self));
                if picker.current == Some(*value) {
                    item.setState(1);
                }
            }
            submenu.addItem(&item);
        }
        // Set the submenu on the item itself — `menu.setSubmenu:forItem:`
        // requires the item to already be a member of the menu, and throwing an
        // ObjC exception across the objc2 boundary aborts the process.
        unsafe { parent.setSubmenu(Some(&submenu)) };
        menu.addItem(&parent);
    }

    fn add_profiles_section(&self, mtm: MainThreadMarker, menu: &NSMenu, snap: &Snapshot) {
        if snap.profiles.is_empty() {
            return;
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let header = NSMenuItem::new(mtm);
        header.setTitle(&NSString::from_str("Apply Profile"));
        unsafe { header.setEnabled(false) };
        menu.addItem(&header);

        for name in &snap.profiles {
            let item = self.action_item(
                mtm,
                &format!("  {name}"),
                Action::ApplyProfile(name.clone()),
                "",
            );
            menu.addItem(&item);
        }
    }

    fn action_item(
        &self,
        mtm: MainThreadMarker,
        title: &str,
        action: Action,
        key: &str,
    ) -> Retained<NSMenuItem> {
        let tag = self.register(action);
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(title));
        unsafe {
            item.setTag(tag);
            item.setAction(Some(sel!(itemClicked:)));
            item.setTarget(Some(self));
            item.setKeyEquivalent(&NSString::from_str(key));
        }
        item
    }

    fn slider_item(
        &self,
        mtm: MainThreadMarker,
        label: &str,
        current: u16,
        max: u16,
        tag: isize,
        enabled: bool,
    ) -> Retained<NSMenuItem> {
        let item = NSMenuItem::new(mtm);

        let width = 240.0;
        let height = 22.0;
        let view =
            unsafe { NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, width, height)) };

        // A control the monitor ignores gets an "(unsupported)" hint so the grey
        // slider is not mistaken for a bug.
        let text = if enabled {
            label.to_string()
        } else {
            format!("{label} (unsupported)")
        };
        let label_field =
            unsafe { objc2_app_kit::NSTextField::labelWithString(&NSString::from_str(&text), mtm) };
        unsafe {
            label_field.setFrame(rect(14.0, 2.0, 120.0, 18.0));
            view.addSubview(&label_field);
        }

        let slider = unsafe {
            let s = NSSlider::initWithFrame(
                NSSlider::alloc(mtm),
                rect(96.0, 0.0, width - 110.0, height),
            );
            s.setMinValue(0.0);
            s.setMaxValue(max as f64);
            s.setDoubleValue(current as f64);
            s.setTag(tag);
            s.setTarget(Some(self));
            s.setAction(Some(sel!(sliderChanged:)));
            s.setContinuous(true);
            // Detected-ignored controls are shown greyed and non-interactive.
            s.setEnabled(enabled);
            s
        };
        unsafe { view.addSubview(&slider) };

        unsafe { item.setView(Some(&view)) };
        item
    }

    fn install(&self, mtm: MainThreadMarker) {
        let status_bar = unsafe { NSStatusBar::systemStatusBar() };
        let item = unsafe { status_bar.statusItemWithLength(NSVariableStatusItemLength) };
        if let Some(button) = unsafe { item.button(mtm) } {
            let symbol = unsafe {
                NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str("sun.max"),
                    Some(&NSString::from_str("Display Studio")),
                )
            };
            match symbol {
                Some(img) => {
                    unsafe { img.setTemplate(true) };
                    button.setImage(Some(&img));
                }
                None => button.setTitle(&NSString::from_str("☀")),
            }
        }

        let menu = NSMenu::new(mtm);
        unsafe { menu.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(self))) };
        self.populate(mtm, &menu);
        unsafe { item.setMenu(Some(&menu)) };

        *self.ivars().status_item.borrow_mut() = Some(item);
    }
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let handler = Handler::new(mtm);
    handler.install(mtm);

    // App delegate: keeps the app alive when the settings window is closed.
    app.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*handler)));

    // Brightness-key interception (needs Accessibility permission).
    crate::mediakeys::install(&handler);

    // Repeating timer to live-refresh the settings window when the background
    // snapshot changes. Cheap: it early-returns unless settings is open and the
    // generation moved.
    unsafe {
        objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.75,
            &*handler,
            sel!(settingsTick:),
            None,
            true,
        );
    }

    std::mem::forget(handler);
    app.run();
}
