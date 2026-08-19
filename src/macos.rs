#![allow(non_upper_case_globals)]

use std::cell::{Cell, OnceCell};
use std::ffi::c_void;
use std::ptr;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication,
    NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSFont, NSMenu, NSMenuItem, NSScrollView,
    NSStatusBar, NSStatusItem, NSTableColumn, NSTableView, NSTextField, NSVariableStatusItemLength,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSUserDefaults, ns_string,
};

use crate::model::AppSnapshot;
use crate::scheduler::BackgroundScheduler;

const hot_key_signature: u32 = u32::from_be_bytes(*b"EvFl");
const hot_key_id: u32 = 1;
const key_code_space: u32 = 49;
const cmd_key: u32 = 1 << 8;
const shift_key: u32 = 1 << 9;
const option_key: u32 = 1 << 11;
const event_class_keyboard: u32 = u32::from_be_bytes(*b"keyb");
const event_hot_key_pressed: u32 = 6;

type OSStatus = i32;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHotKeyRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

type EventHandlerProc =
    unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerProc,
        count: u32,
        types: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        hot_key_id: EventHotKeyId,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OSStatus;
}

struct AppDelegateIvars {
    window: OnceCell<Retained<NSWindow>>,
    search_field: OnceCell<Retained<NSTextField>>,
    status_item: OnceCell<Retained<NSStatusItem>>,
    scheduler: OnceCell<BackgroundScheduler>,
    hot_key: Cell<EventHotKeyRef>,
    launch: Instant,
}

impl Default for AppDelegateIvars {
    fn default() -> Self {
        Self {
            window: OnceCell::new(),
            search_field: OnceCell::new(),
            status_item: OnceCell::new(),
            scheduler: OnceCell::new(),
            hot_key: Cell::new(ptr::null_mut()),
            launch: Instant::now(),
        }
    }
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppDelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, notification: &NSNotification) {
            let mtm = self.mtm();
            let app = notification
                .object()
                .and_then(|object| object.downcast::<NSApplication>().ok())
                .expect("launch notification must belong to NSApplication");

            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            self.ivars()
                .scheduler
                .set(BackgroundScheduler::new(2, 64))
                .ok()
                .expect("scheduler must only initialize once");

            let (window, search_field) = build_search_window(mtm, &AppSnapshot::default());
            self.ivars().window.set(window).unwrap();
            self.ivars().search_field.set(search_field).unwrap();
            self.ivars().status_item.set(build_status_item(mtm, self)).unwrap();

            unsafe { install_hot_key_handler(self) };
            if !self.register_saved_shortcut() {
                eprintln!("everyfile event=shortcut_registration_failed preset=saved");
            }
            eprintln!(
                "everyfile event=application_ready elapsed_ms={}",
                self.ivars().launch.elapsed().as_millis()
            );
            self.show_search_window();
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            let hot_key = self.ivars().hot_key.replace(ptr::null_mut());
            if !hot_key.is_null() {
                unsafe { UnregisterEventHotKey(hot_key) };
            }
        }
    }

    impl Delegate {
        #[unsafe(method(showSearchWindow:))]
        fn show_search_window_action(&self, _sender: Option<&AnyObject>) {
            self.show_search_window();
        }

        #[unsafe(method(showSettings:))]
        fn show_settings_action(&self, _sender: Option<&AnyObject>) {
            self.show_shortcut_settings();
        }

        #[unsafe(method(quitEveryfile:))]
        fn quit_action(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppDelegateIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn show_search_window(&self) {
        let started = Instant::now();
        let Some(window) = self.ivars().window.get() else {
            return;
        };
        window.center();
        window.makeKeyAndOrderFront(None);
        if let Some(search_field) = self.ivars().search_field.get() {
            let _ = window.makeFirstResponder(Some(search_field));
        }
        let app = NSApplication::sharedApplication(self.mtm());
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        eprintln!(
            "everyfile event=quick_search_interactive elapsed_us={}",
            started.elapsed().as_micros()
        );
    }

    fn show_shortcut_settings(&self) {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(ns_string!("Global Shortcut"));
        alert.setInformativeText(ns_string!(
            "Choose the shortcut used to open Everyfile. The selection is saved for future launches."
        ));
        alert.addButtonWithTitle(ns_string!("⌘⌥Space"));
        alert.addButtonWithTitle(ns_string!("⌘⇧Space"));
        alert.addButtonWithTitle(ns_string!("Cancel"));
        let response = alert.runModal();
        let preset = if response == NSAlertFirstButtonReturn {
            Some(1)
        } else if response == NSAlertSecondButtonReturn {
            Some(2)
        } else {
            None
        };
        if let Some(preset) = preset {
            if self.register_shortcut(preset) {
                let defaults = NSUserDefaults::standardUserDefaults();
                defaults.setInteger_forKey(preset, ns_string!("EveryfileShortcutPreset"));
            } else {
                let error = NSAlert::new(self.mtm());
                error.setMessageText(ns_string!("Shortcut unavailable"));
                error.setInformativeText(ns_string!(
                    "Another application has reserved that shortcut. The last working shortcut remains active."
                ));
                error.runModal();
            }
        }
    }

    fn register_saved_shortcut(&self) -> bool {
        let defaults = NSUserDefaults::standardUserDefaults();
        let preset = match defaults.integerForKey(ns_string!("EveryfileShortcutPreset")) {
            2 => 2,
            _ => 1,
        };
        self.register_shortcut(preset)
    }

    fn register_shortcut(&self, preset: isize) -> bool {
        let modifiers = if preset == 2 {
            cmd_key | shift_key
        } else {
            cmd_key | option_key
        };
        let mut replacement = ptr::null_mut();
        let status = unsafe {
            RegisterEventHotKey(
                key_code_space,
                modifiers,
                EventHotKeyId {
                    signature: hot_key_signature,
                    id: hot_key_id,
                },
                GetApplicationEventTarget(),
                0,
                &mut replacement,
            )
        };
        if status != 0 || replacement.is_null() {
            return false;
        }
        let previous = self.ivars().hot_key.replace(replacement);
        if !previous.is_null() {
            unsafe { UnregisterEventHotKey(previous) };
        }
        true
    }
}

unsafe extern "C" fn hot_key_handler(
    _next: EventHandlerCallRef,
    _event: EventRef,
    user_data: *mut c_void,
) -> OSStatus {
    if !user_data.is_null() {
        let delegate = unsafe { &*(user_data.cast::<Delegate>()) };
        delegate.show_search_window();
    }
    0
}

unsafe fn install_hot_key_handler(delegate: &Delegate) {
    let event_type = EventTypeSpec {
        event_class: event_class_keyboard,
        event_kind: event_hot_key_pressed,
    };
    let mut handler_ref = ptr::null_mut();
    let status = unsafe {
        InstallEventHandler(
            GetApplicationEventTarget(),
            hot_key_handler,
            1,
            &event_type,
            (delegate as *const Delegate).cast_mut().cast(),
            &mut handler_ref,
        )
    };
    assert_eq!(status, 0, "failed to install global hot-key handler");
}

fn build_search_window(
    mtm: MainThreadMarker,
    snapshot: &AppSnapshot,
) -> (Retained<NSWindow>, Retained<NSTextField>) {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(760.0, 460.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::FullSizeContentView,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(ns_string!("Everyfile"));
    window.setTitlebarAppearsTransparent(true);
    window.setMovableByWindowBackground(true);
    window.setLevel(NSFloatingWindowLevel);
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.center();

    let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
    effect.setMaterial(NSVisualEffectMaterial::UnderWindowBackground);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    effect.setState(NSVisualEffectState::FollowsWindowActiveState);
    effect.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    let search = NSTextField::textFieldWithString(ns_string!(""), mtm);
    search.setFrame(NSRect::new(
        NSPoint::new(24.0, 380.0),
        NSSize::new(712.0, 42.0),
    ));
    search.setPlaceholderString(Some(ns_string!("Search file names and paths")));
    search.setFont(Some(&NSFont::systemFontOfSize(22.0)));
    search.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);

    let table_frame = NSRect::new(NSPoint::new(24.0, 24.0), NSSize::new(712.0, 330.0));
    let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), table_frame);
    table.setRowHeight(24.0);
    table.setUsesAlternatingRowBackgroundColors(false);
    table.setBackgroundColor(&NSColor::clearColor());
    add_table_column(mtm, &table, "name", "Name", 180.0);
    add_table_column(mtm, &table, "path", "Path", 320.0);
    add_table_column(mtm, &table, "modified", "Modified", 120.0);
    add_table_column(mtm, &table, "size", "Size", 80.0);

    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), table_frame);
    scroll.setDrawsBackground(false);
    scroll.setHasVerticalScroller(false);
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    scroll.setDocumentView(Some(&table));

    let empty_title = NSTextField::labelWithString(
        objc2_foundation::NSString::from_str(snapshot.file_index.title()).as_ref(),
        mtm,
    );
    empty_title.setFrame(NSRect::new(
        NSPoint::new(24.0, 210.0),
        NSSize::new(712.0, 32.0),
    ));
    empty_title.setFont(Some(&NSFont::systemFontOfSize(20.0)));
    empty_title.setAlignment(objc2_app_kit::NSTextAlignment::Center);
    empty_title.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);

    let empty_detail = NSTextField::labelWithString(
        objc2_foundation::NSString::from_str(snapshot.file_index.detail()).as_ref(),
        mtm,
    );
    empty_detail.setFrame(NSRect::new(
        NSPoint::new(24.0, 180.0),
        NSSize::new(712.0, 24.0),
    ));
    empty_detail.setTextColor(Some(&NSColor::secondaryLabelColor()));
    empty_detail.setAlignment(objc2_app_kit::NSTextAlignment::Center);
    empty_detail.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);

    effect.addSubview(&search);
    effect.addSubview(&scroll);
    effect.addSubview(&empty_title);
    effect.addSubview(&empty_detail);
    window.setContentView(Some(&effect));
    (window, search)
}

fn add_table_column(
    mtm: MainThreadMarker,
    table: &NSTableView,
    identifier: &str,
    title: &str,
    width: f64,
) {
    let identifier = objc2_foundation::NSString::from_str(identifier);
    let column = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &identifier);
    column
        .headerCell()
        .setStringValue(&objc2_foundation::NSString::from_str(title));
    column.setWidth(width);
    column.setMinWidth(60.0);
    table.addTableColumn(&column);
}

fn build_status_item(mtm: MainThreadMarker, delegate: &Delegate) -> Retained<NSStatusItem> {
    let status_item =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = status_item.button(mtm) {
        button.setTitle(ns_string!("Everyfile"));
        button.setToolTip(Some(ns_string!("Everyfile — No File Index")));
    }

    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Everyfile"));
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Open Quick Search"),
        sel!(showSearchWindow:),
        ns_string!(""),
        true,
    );
    let state = add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("File Index: Not Available"),
        sel!(showSearchWindow:),
        ns_string!(""),
        false,
    );
    state.setSubtitle(Some(ns_string!(
        "Coverage and Freshness are not available yet"
    )));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Settings…"),
        sel!(showSettings:),
        ns_string!(","),
        true,
    );
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Quit Everyfile"),
        sel!(quitEveryfile:),
        ns_string!("q"),
        true,
    );
    status_item.setMenu(Some(&menu));
    status_item
}

fn add_menu_item(
    mtm: MainThreadMarker,
    menu: &NSMenu,
    delegate: &Delegate,
    title: &objc2_foundation::NSString,
    action: objc2::runtime::Sel,
    key: &objc2_foundation::NSString,
    enabled: bool,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            title,
            Some(action),
            key,
        )
    };
    unsafe { item.setTarget(Some(delegate)) };
    item.setEnabled(enabled);
    menu.addItem(&item);
    item
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("Everyfile must start on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let delegate = Delegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
}
