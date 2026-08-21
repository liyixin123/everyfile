#![allow(non_upper_case_globals)]

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication,
    NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSColor, NSControl, NSControlTextEditingDelegate, NSEventModifierFlags,
    NSFloatingWindowLevel, NSFont, NSMenu, NSMenuItem, NSPasteboard, NSPasteboardTypeString,
    NSScrollView, NSStatusBar, NSStatusItem, NSTableColumn, NSTableView, NSTableViewDataSource,
    NSTableViewDelegate, NSTextField, NSTextFieldDelegate, NSTextView, NSVariableStatusItemLength,
    NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
    NSVisualEffectView, NSWindow, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSTimer,
    NSURL, NSUserDefaults, ns_string,
};

use crate::actions::{ResultAction, ResultActionDispatcher};
use crate::coordinator::{
    build_first_index_with_progress, configured_root, default_data_directory,
};
use crate::index::IndexStore;
use crate::model::{AppSnapshot, FileIndexState, SearchResult};
use crate::projection::SearchProjection;
use crate::query::{CancellationToken, SortDirection, SortField, SortOrder};
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
    table: OnceCell<Retained<NSTableView>>,
    state_title: OnceCell<Retained<NSTextField>>,
    state_detail: OnceCell<Retained<NSTextField>>,
    status_item: OnceCell<Retained<NSStatusItem>>,
    status_state_item: OnceCell<Retained<NSMenuItem>>,
    scheduler: OnceCell<BackgroundScheduler>,
    runtime: Arc<Mutex<RuntimeIndex>>,
    results: RefCell<Vec<SearchResult>>,
    hot_key: Cell<EventHotKeyRef>,
    launch: Instant,
    sort: Cell<SortOrder>,
    query_generation: Cell<u64>,
    query_cancellation: RefCell<CancellationToken>,
    requested_limit: Cell<usize>,
    exact_total: Cell<usize>,
}

struct RuntimeIndex {
    state: FileIndexState,
    projection: Option<Arc<SearchProjection>>,
    error: Option<String>,
    recent_opens: HashMap<u64, u64>,
    pending_query: Option<QueryPublication>,
}

struct QueryPublication {
    generation: u64,
    rows: Vec<SearchResult>,
    exact_total: usize,
}

struct SearchWindowParts {
    window: Retained<NSWindow>,
    search_field: Retained<NSTextField>,
    table: Retained<NSTableView>,
    state_title: Retained<NSTextField>,
    state_detail: Retained<NSTextField>,
}

impl Default for AppDelegateIvars {
    fn default() -> Self {
        Self {
            window: OnceCell::new(),
            search_field: OnceCell::new(),
            table: OnceCell::new(),
            state_title: OnceCell::new(),
            state_detail: OnceCell::new(),
            status_item: OnceCell::new(),
            status_state_item: OnceCell::new(),
            scheduler: OnceCell::new(),
            runtime: Arc::new(Mutex::new(RuntimeIndex {
                state: FileIndexState::NotAvailable,
                projection: None,
                error: None,
                recent_opens: HashMap::new(),
                pending_query: None,
            })),
            results: RefCell::new(Vec::new()),
            hot_key: Cell::new(ptr::null_mut()),
            launch: Instant::now(),
            sort: Cell::new(SortOrder::default()),
            query_generation: Cell::new(0),
            query_cancellation: RefCell::new(CancellationToken::default()),
            requested_limit: Cell::new(100),
            exact_total: Cell::new(0),
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
            app.setMainMenu(Some(&build_main_menu(mtm, self)));
            self.restore_sort_order();
            self.ivars()
                .scheduler
                .set(BackgroundScheduler::new(2, 64))
                .ok()
                .expect("scheduler must only initialize once");

            let parts = build_search_window(mtm, &AppSnapshot::default(), self);
            self.ivars().window.set(parts.window).unwrap();
            self.ivars().search_field.set(parts.search_field).unwrap();
            self.ivars().table.set(parts.table).unwrap();
            self.ivars().state_title.set(parts.state_title).unwrap();
            self.ivars().state_detail.set(parts.state_detail).unwrap();
            let (status_item, status_state_item) = build_status_item(mtm, self);
            self.ivars().status_item.set(status_item).unwrap();
            self.ivars()
                .status_state_item
                .set(status_state_item)
                .unwrap();

            unsafe { install_hot_key_handler(self) };
            if !self.register_saved_shortcut() {
                eprintln!("everyfile event=shortcut_registration_failed preset=saved");
            }
            eprintln!(
                "everyfile event=application_ready elapsed_ms={}",
                self.ivars().launch.elapsed().as_millis()
            );
            unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    0.1,
                    self,
                    sel!(refreshIndexState:),
                    None,
                    true,
                )
            };
            self.start_initial_index();
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

    unsafe impl NSTableViewDataSource for Delegate {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table_view: &NSTableView) -> isize {
            self.ivars().results.borrow().len() as isize
        }

    }

    unsafe impl NSControlTextEditingDelegate for Delegate {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &NSNotification) {
            self.ivars().requested_limit.set(100);
            self.run_search();
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        unsafe fn control_command(
            &self,
            _control: &NSControl,
            _text_view: &NSTextView,
            command_selector: objc2::runtime::Sel,
        ) -> bool {
            if command_selector == sel!(insertNewline:) {
                let modifiers = NSApplication::sharedApplication(self.mtm())
                    .currentEvent()
                    .map(|event| event.modifierFlags())
                    .unwrap_or_else(NSEventModifierFlags::empty);
                let action = if modifiers.contains(NSEventModifierFlags::Command) {
                    ResultAction::Reveal
                } else {
                    ResultAction::Open
                };
                self.dispatch_selected(action)
            } else if command_selector == sel!(copy:) {
                self.dispatch_selected(ResultAction::CopyPath)
            } else {
                false
            }
        }
    }

    unsafe impl NSTextFieldDelegate for Delegate {}

    unsafe impl NSTableViewDelegate for Delegate {
        #[unsafe(method(tableView:didClickTableColumn:))]
        fn did_click_table_column(&self, _table: &NSTableView, column: &NSTableColumn) {
            let field = match column.identifier().to_string().as_str() {
                "name" => SortField::FileName,
                "path" => SortField::FullPath,
                "modified" => SortField::ModificationTime,
                "created" => SortField::CreationTime,
                "size" => SortField::FileSize,
                _ => return,
            };
            self.select_sort(field);
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_did_change(&self, _notification: &NSNotification) {
            let Some(table) = self.ivars().table.get() else { return };
            let selected = usize::try_from(table.selectedRow()).unwrap_or(0);
            let current = self.ivars().results.borrow().len();
            if current < self.ivars().exact_total.get() && selected.saturating_add(20) >= current {
                self.ivars().requested_limit.set(current.saturating_add(100));
                self.run_search();
            }
        }

        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn table_view(
            &self,
            _table_view: &NSTableView,
            table_column: Option<&NSTableColumn>,
            row: isize,
        ) -> Option<Retained<NSView>> {
            let results = self.ivars().results.borrow();
            let result = &results[row as usize];
            let table_column = table_column.expect("table view requests a known column");
            let identifier = table_column.identifier().to_string();
            let value = match identifier.as_str() {
                "name" => result.name.clone(),
                "path" => result.path.to_string_lossy().into_owned(),
                "modified" => result
                    .modified_ns
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                "created" => result
                    .created_ns
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                "size" => result.size.to_string(),
                _ => String::new(),
            };
            let label = NSTextField::labelWithString(
                &objc2_foundation::NSString::from_str(&value),
                self.mtm(),
            );
            Some(label.into_super().into_super())
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

        #[unsafe(method(runSearch:))]
        fn run_search_action(&self, _sender: Option<&AnyObject>) {
            self.run_search();
        }

        #[unsafe(method(refreshIndexState:))]
        fn refresh_index_state_action(&self, _timer: Option<&AnyObject>) {
            self.refresh_index_state();
        }

        #[unsafe(method(clearOpenHistory:))]
        fn clear_open_history_action(&self, _sender: Option<&AnyObject>) {
            self.clear_open_history();
        }

        #[unsafe(method(copySelectedPath:))]
        fn copy_selected_path_action(&self, _sender: Option<&AnyObject>) {
            self.dispatch_selected(ResultAction::CopyPath);
        }

        #[unsafe(method(sortByRelevance:))]
        fn sort_by_relevance_action(&self, _sender: Option<&AnyObject>) {
            self.select_sort(SortField::Relevance);
        }

        #[unsafe(method(sortByCreationTime:))]
        fn sort_by_creation_time_action(&self, _sender: Option<&AnyObject>) {
            self.select_sort(SortField::CreationTime);
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

    fn start_initial_index(&self) {
        let Some(root) = configured_root() else {
            return;
        };
        let runtime = Arc::clone(&self.ivars().runtime);
        runtime.lock().unwrap().state = FileIndexState::Rebuilding { scanned_entries: 0 };
        let data_directory = default_data_directory();
        let schedule_result = self
            .ivars()
            .scheduler
            .get()
            .expect("scheduler initialized")
            .try_schedule(move || {
                let progress_runtime = Arc::clone(&runtime);
                let result = build_first_index_with_progress(
                    &root,
                    &data_directory,
                    move |scanned_entries| {
                        progress_runtime.lock().unwrap().state =
                            FileIndexState::Rebuilding { scanned_entries };
                    },
                );
                let recent_opens = IndexStore::open(&data_directory.join("index.sqlite3"))
                    .and_then(|store| store.recent_opens())
                    .unwrap_or_default();
                let mut runtime = runtime.lock().unwrap();
                match result {
                    Ok(built) => {
                        runtime.state = built.state;
                        runtime.projection = Some(Arc::new(built.projection));
                        runtime.error = None;
                        runtime.recent_opens = recent_opens;
                    }
                    Err(error) => {
                        runtime.state = FileIndexState::NotAvailable;
                        runtime.error = Some(error);
                    }
                }
            });
        if let Err(error) = schedule_result {
            let mut runtime = self.ivars().runtime.lock().unwrap();
            runtime.state = FileIndexState::NotAvailable;
            runtime.error = Some(format!("could not schedule initial scan: {error:?}"));
        }
    }

    fn refresh_index_state(&self) {
        let mut runtime = self.ivars().runtime.lock().unwrap();
        if let Some(publication) = runtime.pending_query.take()
            && publication.generation == self.ivars().query_generation.get()
        {
            *self.ivars().results.borrow_mut() = publication.rows;
            self.ivars().exact_total.set(publication.exact_total);
            if let Some(table) = self.ivars().table.get() {
                table.reloadData();
            }
        }
        let title = runtime.state.title();
        let detail = runtime
            .error
            .clone()
            .unwrap_or_else(|| runtime.state.detail());
        if let Some(label) = self.ivars().state_title.get() {
            label.setStringValue(&objc2_foundation::NSString::from_str(title));
        }
        if let Some(label) = self.ivars().state_detail.get() {
            label.setStringValue(&objc2_foundation::NSString::from_str(&detail));
        }
        if let Some(item) = self.ivars().status_state_item.get() {
            item.setTitle(&objc2_foundation::NSString::from_str(title));
            item.setSubtitle(Some(&objc2_foundation::NSString::from_str(&detail)));
        }
        if let Some(button) = self
            .ivars()
            .status_item
            .get()
            .and_then(|item| item.button(self.mtm()))
        {
            button.setToolTip(Some(&objc2_foundation::NSString::from_str(&format!(
                "Everyfile — {title}"
            ))));
        }
    }

    fn run_search(&self) {
        let query = self
            .ivars()
            .search_field
            .get()
            .map(|field| field.stringValue().to_string())
            .unwrap_or_default();
        self.ivars().query_cancellation.borrow().cancel();
        let cancellation = CancellationToken::default();
        *self.ivars().query_cancellation.borrow_mut() = cancellation.clone();
        let generation = self.ivars().query_generation.get().wrapping_add(1);
        self.ivars().query_generation.set(generation);
        let runtime = self.ivars().runtime.lock().unwrap();
        let projection = runtime.projection.clone();
        let recent_opens = runtime.recent_opens.clone();
        drop(runtime);
        let Some(projection) = projection else { return };
        let runtime = Arc::clone(&self.ivars().runtime);
        let sort = self.ivars().sort.get();
        let limit = self.ivars().requested_limit.get();
        let _ = self
            .ivars()
            .scheduler
            .get()
            .expect("scheduler initialized")
            .try_schedule(move || {
                if let Ok(ranked) =
                    projection.search_ranked(&query, &recent_opens, limit, sort, &cancellation)
                    && !ranked.cancelled
                {
                    runtime.lock().unwrap().pending_query = Some(QueryPublication {
                        generation,
                        rows: ranked.rows,
                        exact_total: ranked.exact_total,
                    });
                }
            });
    }

    fn select_sort(&self, field: SortField) {
        let current = self.ivars().sort.get();
        let direction = if current.field == field {
            match current.direction {
                SortDirection::Ascending => SortDirection::Descending,
                SortDirection::Descending => SortDirection::Ascending,
            }
        } else {
            SortDirection::Ascending
        };
        let sort = SortOrder { field, direction };
        self.ivars().sort.set(sort);
        self.ivars().requested_limit.set(100);
        self.persist_sort_order(sort);
        self.run_search();
    }

    fn restore_sort_order(&self) {
        let defaults = NSUserDefaults::standardUserDefaults();
        let field = match defaults.integerForKey(ns_string!("EveryfileSortField")) {
            1 => SortField::ModificationTime,
            2 => SortField::CreationTime,
            3 => SortField::FileName,
            4 => SortField::FullPath,
            5 => SortField::FileSize,
            _ => SortField::Relevance,
        };
        let direction = if defaults.integerForKey(ns_string!("EveryfileSortDirection")) == 1 {
            SortDirection::Descending
        } else {
            SortDirection::Ascending
        };
        self.ivars().sort.set(SortOrder { field, direction });
    }

    fn persist_sort_order(&self, sort: SortOrder) {
        let defaults = NSUserDefaults::standardUserDefaults();
        let field = match sort.field {
            SortField::Relevance => 0,
            SortField::ModificationTime => 1,
            SortField::CreationTime => 2,
            SortField::FileName => 3,
            SortField::FullPath => 4,
            SortField::FileSize => 5,
        };
        let direction = usize::from(sort.direction == SortDirection::Descending);
        defaults.setInteger_forKey(field, ns_string!("EveryfileSortField"));
        defaults.setInteger_forKey(direction as isize, ns_string!("EveryfileSortDirection"));
    }

    fn dispatch_selected(&self, action: ResultAction) -> bool {
        let results = self.ivars().results.borrow();
        if results.is_empty() {
            return false;
        }
        let selected_row = self
            .ivars()
            .table
            .get()
            .map(|table| table.selectedRow())
            .unwrap_or(-1);
        let index = usize::try_from(selected_row)
            .unwrap_or(0)
            .min(results.len() - 1);
        let result = results[index].clone();
        drop(results);

        let succeeded = MacResultActionDispatcher.dispatch(action, &result);
        if succeeded
            && action == ResultAction::Open
            && let Ok(store) = IndexStore::open(&default_data_directory().join("index.sqlite3"))
            && store.record_successful_open(result.entry_id).is_ok()
        {
            self.ivars()
                .runtime
                .lock()
                .unwrap()
                .recent_opens
                .insert(result.entry_id, current_time_ns());
        }
        succeeded
    }

    fn clear_open_history(&self) {
        if let Ok(store) = IndexStore::open(&default_data_directory().join("index.sqlite3")) {
            let _ = store.clear_open_history();
        }
        self.ivars().runtime.lock().unwrap().recent_opens.clear();
        self.run_search();
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

struct MacResultActionDispatcher;

impl ResultActionDispatcher for MacResultActionDispatcher {
    fn dispatch(&mut self, action: ResultAction, result: &SearchResult) -> bool {
        let path = objc2_foundation::NSString::from_str(&result.path.to_string_lossy());
        match action {
            ResultAction::Open => {
                let url = NSURL::fileURLWithPath(&path);
                NSWorkspace::sharedWorkspace().openURL(&url)
            }
            ResultAction::Reveal => NSWorkspace::sharedWorkspace()
                .selectFile_inFileViewerRootedAtPath(Some(&path), ns_string!("")),
            ResultAction::CopyPath => {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                pasteboard.setString_forType(&path, unsafe { NSPasteboardTypeString })
            }
        }
    }
}

fn current_time_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

fn build_search_window(
    mtm: MainThreadMarker,
    snapshot: &AppSnapshot,
    delegate: &Delegate,
) -> SearchWindowParts {
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
    unsafe { search.setDelegate(Some(ProtocolObject::from_ref(delegate))) };

    let table_frame = NSRect::new(NSPoint::new(24.0, 24.0), NSSize::new(712.0, 330.0));
    let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), table_frame);
    table.setRowHeight(24.0);
    table.setUsesAlternatingRowBackgroundColors(false);
    table.setBackgroundColor(&NSColor::clearColor());
    add_table_column(mtm, &table, "name", "Name", 150.0);
    add_table_column(mtm, &table, "path", "Path", 260.0);
    add_table_column(mtm, &table, "modified", "Modified", 100.0);
    add_table_column(mtm, &table, "created", "Created", 100.0);
    add_table_column(mtm, &table, "size", "Size", 70.0);
    unsafe {
        table.setDataSource(Some(ProtocolObject::from_ref(delegate)));
        table.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    }

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
        objc2_foundation::NSString::from_str(&snapshot.file_index.detail()).as_ref(),
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
    SearchWindowParts {
        window,
        search_field: search,
        table,
        state_title: empty_title,
        state_detail: empty_detail,
    }
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

fn build_status_item(
    mtm: MainThreadMarker,
    delegate: &Delegate,
) -> (Retained<NSStatusItem>, Retained<NSMenuItem>) {
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
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Clear Open History"),
        sel!(clearOpenHistory:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Sort by Relevance"),
        sel!(sortByRelevance:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Sort by Creation Time"),
        sel!(sortByCreationTime:),
        ns_string!(""),
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
    (status_item, state)
}

fn build_main_menu(mtm: MainThreadMarker, delegate: &Delegate) -> Retained<NSMenu> {
    let main_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Everyfile"));
    let edit_item = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("Edit"));
    add_menu_item(
        mtm,
        &edit_menu,
        delegate,
        ns_string!("Copy Path"),
        sel!(copySelectedPath:),
        ns_string!("c"),
        true,
    );
    edit_item.setSubmenu(Some(&edit_menu));
    main_menu.addItem(&edit_item);
    main_menu
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
