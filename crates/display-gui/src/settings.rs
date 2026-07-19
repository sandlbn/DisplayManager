//! Settings window: Displays, Profiles, General tabs.
//!
//! Built imperatively from the current snapshot each time it opens, with every
//! control targeting the shared `Handler` (`settingsControl:`). Layout uses
//! simple top-down absolute frames — a scroll view backs each tab so a long
//! monitor or profile list stays reachable.

use objc2::rc::Retained;
use objc2::{define_class, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSBackingStoreType, NSButton, NSFont, NSScrollView, NSTabView, NSTabViewItem,
    NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};
use std::cell::RefCell;

use crate::statusbar::{Handler, SettingsAction};

// A top-left-origin NSView. Without this, a scroll view's document view uses
// AppKit's default bottom-left origin, so content lays out from the bottom and
// scrolls up from there — which is what made the tabs look bottom-aligned.
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "DSFlippedView"]
    struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

thread_local! {
    static WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

const W: f64 = 460.0;
const H: f64 = 420.0;

/// Whether the settings window is currently on screen.
pub fn is_open() -> bool {
    WINDOW.with(|w| {
        w.borrow()
            .as_ref()
            .map(|win| win.isVisible())
            .unwrap_or(false)
    })
}

/// Replace the window's content with a freshly-built version, without stealing
/// focus — used by the live-refresh timer.
pub fn rebuild_content(mtm: MainThreadMarker, handler: &Handler) {
    // Preserve the selected tab: a blind rebuild would snap the user back to the
    // first tab every time the background data changed.
    let selected = current_tab_index();

    handler.clear_settings_actions();
    let content = build_tabs(mtm, handler);
    if selected >= 0 {
        content.selectTabViewItemAtIndex(selected);
    }
    WINDOW.with(|w| {
        if let Some(win) = w.borrow().as_ref() {
            win.setContentView(Some(&content));
        }
    });
    handler.mark_settings_built();
}

/// Index of the currently-selected settings tab, or -1 if unavailable.
fn current_tab_index() -> isize {
    WINDOW.with(|w| {
        let slot = w.borrow();
        let Some(win) = slot.as_ref() else { return -1 };
        let Some(view) = win.contentView() else {
            return -1;
        };
        match view.downcast::<NSTabView>() {
            Ok(tabs) => match tabs.selectedTabViewItem() {
                Some(item) => tabs.indexOfTabViewItem(&item),
                None => -1,
            },
            Err(_) => -1,
        }
    })
}

pub fn open(mtm: MainThreadMarker, handler: &Handler) {
    // Rebuild the control table from scratch each open.
    handler.clear_settings_actions();

    let content = build_tabs(mtm, handler);
    handler.mark_settings_built();

    WINDOW.with(|w| {
        let mut slot = w.borrow_mut();
        if let Some(existing) = slot.as_ref() {
            // Replace content so it reflects current state, then refocus.
            existing.setContentView(Some(&content));
            existing.makeKeyAndOrderFront(None);
            activate(mtm);
            return;
        }

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, W, H),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // We keep our own `Retained<NSWindow>` in WINDOW, so the window must NOT
        // release itself on close (its default). Otherwise closing it frees the
        // window while our reference dangles, and the next timer tick that calls
        // `isVisible()` on it crashes the app with a trace trap. With this off,
        // closing merely hides the window and reopening re-shows the same one.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Display Studio"));
        window.center();
        window.setContentView(Some(&content));
        window.makeKeyAndOrderFront(None);
        activate(mtm);
        *slot = Some(window);
    });
}

fn activate(mtm: MainThreadMarker) {
    // A non-activating accessory app must come forward explicitly.
    unsafe { objc2_app_kit::NSApplication::sharedApplication(mtm).activate() };
}

fn build_tabs(mtm: MainThreadMarker, handler: &Handler) -> Retained<NSTabView> {
    let tabs = unsafe { NSTabView::initWithFrame(NSTabView::alloc(mtm), rect(0.0, 0.0, W, H)) };

    add_tab(&tabs, "Displays", displays_view(mtm, handler));
    add_tab(&tabs, "Capabilities", capabilities_view(mtm, handler));
    add_tab(&tabs, "Profiles", profiles_view(mtm, handler));
    add_tab(&tabs, "General", general_view(mtm, handler));

    tabs
}

fn add_tab(tabs: &NSTabView, label: &str, view: Retained<NSView>) {
    let item = unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), None) };
    item.setLabel(&NSString::from_str(label));
    item.setView(Some(&view));
    tabs.addTabViewItem(&item);
}

/// A scroll view wrapping a document view of the given height, so long content
/// scrolls. Returns (outer scroll view as NSView, document view to fill).
fn scrolling(mtm: MainThreadMarker, doc_height: f64) -> (Retained<NSView>, Retained<NSView>) {
    let outer_h = H - 40.0; // leave room for the tab strip
    let scroll = unsafe {
        NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, W, outer_h))
    };
    unsafe {
        scroll.setHasVerticalScroller(true);
        scroll.setDrawsBackground(false);
    }
    let height = doc_height.max(outer_h);
    // Flipped doc view: origin top-left, so callers place content with y growing
    // downward from the top.
    let doc: Retained<NSView> = {
        let v = mtm.alloc::<FlippedView>();
        let v: Retained<FlippedView> =
            unsafe { objc2::msg_send![v, initWithFrame: rect(0.0, 0.0, W, height)] };
        v.into_super()
    };
    unsafe { scroll.setDocumentView(Some(&doc)) };
    // Safe upcast NSScrollView -> NSView (its direct superclass).
    let outer: Retained<NSView> = scroll.into_super();
    (outer, doc)
}

// ── Displays tab ─────────────────────────────────────────────────────────────

fn displays_view(mtm: MainThreadMarker, handler: &Handler) -> Retained<NSView> {
    let snap = handler.snapshot();

    // Candidate controls per monitor: the advertised, adjustable (continuous)
    // codes the user can choose to show. Checked = shown in the menu.
    let candidates = |m: &crate::client::MonitorView| -> Vec<(u8, String)> {
        if m.is_ddc {
            snap.caps
                .get(&m.id)
                .map(|cv| {
                    cv.codes
                        .iter()
                        .filter(|(code, _)| display_ddc::vcp::is_adjustable(*code))
                        .map(|(code, name)| (*code, name.clone()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![(0x10, "Brightness".to_string())]
        }
    };

    let rows: usize = snap
        .monitors
        .iter()
        .map(|m| 1 + candidates(m).len().max(1))
        .sum();
    let doc_h = 40.0 + rows as f64 * 26.0;
    let (outer, doc) = scrolling(mtm, doc_h);

    let mut y = 14.0;

    if !snap.daemon_up {
        place_label(
            mtm,
            &doc,
            "displayd is not running.",
            20.0,
            y,
            W - 40.0,
            false,
        );
        return outer;
    }
    if snap.monitors.is_empty() {
        place_label(
            mtm,
            &doc,
            "No controllable displays.",
            20.0,
            y,
            W - 40.0,
            false,
        );
        return outer;
    }

    for m in &snap.monitors {
        place_label(mtm, &doc, &m.name, 16.0, y, W - 32.0, true);
        y += 24.0;

        let controls = candidates(m);
        if controls.is_empty() {
            // Capabilities not read yet — the worker fetches them shortly, and
            // the live-refresh timer rebuilds this tab when they arrive.
            place_label(
                mtm,
                &doc,
                "  reading capabilities…",
                28.0,
                y,
                W - 48.0,
                false,
            );
            y += 24.0;
            continue;
        }

        for (code, name) in controls {
            let shown = handler.is_visible(&m.key, code);
            let tag = handler.register_settings_action(SettingsAction::ToggleVisible {
                key: m.key.clone(),
                code,
            });
            place_checkbox(
                mtm,
                &doc,
                handler,
                &format!("Show {name} in menu"),
                shown,
                tag,
                40.0,
                y,
            );
            y += 24.0;
        }
        y += 6.0;
    }

    outer
}

// ── Capabilities tab ─────────────────────────────────────────────────────────

fn capabilities_view(mtm: MainThreadMarker, handler: &Handler) -> Retained<NSView> {
    let snap = handler.snapshot();
    let ddc: Vec<_> = snap.monitors.iter().filter(|m| m.is_ddc).collect();

    // Height: per monitor, a header + fetch button + a line per advertised code
    // if already fetched.
    let lines: usize = ddc
        .iter()
        .map(|m| 2 + snap.caps.get(&m.id).map(|c| c.codes.len() + 2).unwrap_or(1))
        .sum();
    let doc_h = 40.0 + lines as f64 * 22.0;
    let (outer, doc) = scrolling(mtm, doc_h);
    let mut y = 14.0;

    if ddc.is_empty() {
        place_label(
            mtm,
            &doc,
            "No DDC displays. Capabilities are read over DDC/CI.",
            20.0,
            y,
            W - 40.0,
            false,
        );
        return outer;
    }

    for m in ddc {
        place_label(mtm, &doc, &m.name, 16.0, y, W - 140.0, true);

        let fetch_tag =
            handler.register_settings_action(SettingsAction::FetchCaps(m.id.to_string()));
        let title = if snap.caps.contains_key(&m.id) {
            "Refresh"
        } else {
            "Fetch"
        };
        place_button(
            mtm,
            &doc,
            handler,
            title,
            fetch_tag,
            W - 110.0,
            y - 2.0,
            88.0,
        );
        y += 26.0;

        match snap.caps.get(&m.id) {
            None => {
                place_label(
                    mtm,
                    &doc,
                    "  Click Fetch to read this display's capabilities.",
                    28.0,
                    y,
                    W - 48.0,
                    false,
                );
                y += 24.0;
            }
            Some(caps) => {
                let header = match &caps.mccs {
                    Some(v) => format!("  MCCS {v} · {} VCP code(s) advertised", caps.codes.len()),
                    None => format!("  {} VCP code(s) advertised", caps.codes.len()),
                };
                place_label(mtm, &doc, &header, 28.0, y, W - 48.0, false);
                y += 22.0;

                for (code, name) in &caps.codes {
                    let line = format!("  0x{code:02X}  {name}");
                    place_label(mtm, &doc, &line, 40.0, y, W - 56.0, false);
                    y += 20.0;
                }
                if !caps.unknown_sections.is_empty() {
                    let u = format!("  vendor sections: {}", caps.unknown_sections.join(", "));
                    place_label(mtm, &doc, &u, 40.0, y, W - 56.0, false);
                    y += 20.0;
                }
            }
        }
        y += 10.0;
    }

    outer
}

// ── Profiles tab ─────────────────────────────────────────────────────────────

fn profiles_view(mtm: MainThreadMarker, handler: &Handler) -> Retained<NSView> {
    let snap = handler.snapshot();
    let doc_h = 80.0 + snap.profiles.len().max(1) as f64 * 30.0;
    let (outer, doc) = scrolling(mtm, doc_h);
    let mut y = 14.0;

    // "Save current settings as profile…" — always available.
    let save_tag = handler.register_settings_action(SettingsAction::SaveProfilePrompt);
    place_button(
        mtm,
        &doc,
        handler,
        "Save current settings as profile…",
        save_tag,
        16.0,
        y,
        260.0,
    );
    y += 40.0;

    if snap.profiles.is_empty() {
        place_label(
            mtm,
            &doc,
            "No saved profiles yet.",
            20.0,
            y,
            W - 40.0,
            false,
        );
        return outer;
    }

    for name in &snap.profiles {
        place_label(mtm, &doc, name, 16.0, y + 4.0, 200.0, true);

        let apply_tag =
            handler.register_settings_action(SettingsAction::ApplyProfile(name.clone()));
        place_button(mtm, &doc, handler, "Apply", apply_tag, W - 180.0, y, 72.0);

        let del_tag = handler.register_settings_action(SettingsAction::DeleteProfile(name.clone()));
        place_button(mtm, &doc, handler, "Delete", del_tag, W - 100.0, y, 78.0);

        y += 30.0;
    }

    outer
}

/// Modal name prompt. Returns the trimmed name, or None if cancelled/empty.
pub fn prompt_name(mtm: MainThreadMarker) -> Option<String> {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Save profile"));
    alert.setInformativeText(&NSString::from_str(
        "Name for a profile of the current settings:",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Save"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let field = unsafe {
        let f = NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(0.0, 0.0, 220.0, 24.0));
        f.setStringValue(&NSString::from_str(""));
        f
    };
    alert.setAccessoryView(Some(&field));

    // NSAlertFirstButtonReturn == 1000 (the "Save" button).
    let resp = alert.runModal();
    if resp != 1000 {
        return None;
    }
    let name = unsafe { field.stringValue() }.to_string();
    let name = name.trim().to_string();
    // Match the daemon's profile-name rules so the save doesn't silently fail.
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then_some(name)
}

// ── General tab ──────────────────────────────────────────────────────────────

fn general_view(mtm: MainThreadMarker, handler: &Handler) -> Retained<NSView> {
    let (outer, doc) = scrolling(mtm, H - 40.0);

    let login_tag = handler.register_settings_action(SettingsAction::ToggleLogin);
    place_checkbox(
        mtm,
        &doc,
        handler,
        "Launch Display Studio at login",
        handler.launch_at_login(),
        login_tag,
        20.0,
        18.0,
    );

    let version = format!("Display Studio {}", env!("CARGO_PKG_VERSION"));
    place_label(mtm, &doc, &version, 20.0, 60.0, W - 40.0, false);

    let daemon = if handler.snapshot().daemon_up {
        "displayd: connected"
    } else {
        "displayd: not running"
    };
    place_label(mtm, &doc, daemon, 20.0, 84.0, W - 40.0, false);

    outer
}

// ── Control builders ─────────────────────────────────────────────────────────

fn place_label(
    mtm: MainThreadMarker,
    parent: &NSView,
    text: &str,
    x: f64,
    y: f64,
    w: f64,
    bold: bool,
) {
    let field = unsafe { NSTextField::labelWithString(&NSString::from_str(text), mtm) };
    unsafe {
        field.setFrame(rect(x, y, w, 18.0));
        if bold {
            field.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        }
        parent.addSubview(&field);
    }
}

fn place_checkbox(
    mtm: MainThreadMarker,
    parent: &NSView,
    handler: &Handler,
    title: &str,
    checked: bool,
    tag: isize,
    x: f64,
    y: f64,
) {
    let button = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str(title),
            Some(handler),
            Some(sel!(settingsControl:)),
            mtm,
        )
    };
    unsafe {
        button.setFrame(rect(x, y, W - x - 20.0, 20.0));
        button.setTag(tag);
        button.setState(if checked { 1 } else { 0 });
        parent.addSubview(&button);
    }
}

fn place_button(
    mtm: MainThreadMarker,
    parent: &NSView,
    handler: &Handler,
    title: &str,
    tag: isize,
    x: f64,
    y: f64,
    w: f64,
) {
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(handler),
            Some(sel!(settingsControl:)),
            mtm,
        )
    };
    unsafe {
        button.setFrame(rect(x, y, w, 24.0));
        button.setTag(tag);
        parent.addSubview(&button);
    }
}
