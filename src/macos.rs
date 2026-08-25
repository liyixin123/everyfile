#![allow(non_upper_case_globals)]

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAlertThirdButtonReturn,
    NSAppearance, NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSColor,
    NSControl, NSControlTextEditingDelegate, NSEventModifierFlags, NSFloatingWindowLevel, NSFont,
    NSGlassEffectView, NSGlassEffectViewStyle, NSMenu, NSMenuItem, NSOpenPanel, NSPasteboard,
    NSPasteboardTypeFileURL, NSPasteboardTypeString, NSPopUpButton, NSScrollView, NSStatusBar,
    NSStatusItem, NSTableColumn, NSTableView, NSTableViewDataSource, NSTableViewDelegate,
    NSTextField, NSTextFieldDelegate, NSTextView, NSVariableStatusItemLength, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindow, NSWindowStyleMask, NSWorkspace, NSWorkspaceDidMountNotification,
    NSWorkspaceDidUnmountNotification, NSWorkspaceDidWakeNotification,
};
use objc2_foundation::{
    MainThreadMarker, NSDate, NSDateFormatter, NSDateFormatterStyle, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSProcessInfo, NSProcessInfoThermalState, NSRect, NSSize, NSTimer,
    NSURL, NSUserDefaults, ns_string,
};

use crate::actions::{ResultAction, ResultActionDispatcher};
use crate::coordinator::{
    build_first_index_with_progress, configured_root, default_data_directory,
};
use crate::index::IndexStore;
use crate::indexing_control::{ResourceConditions, WorkAllowance, work_allowance};
use crate::model::{
    AppSnapshot, Coverage, EntryFilter, EntryKind, FileIndexState, Freshness, RootCoverage,
    SearchResult, overall_coverage,
};
use crate::projection::SearchProjection;
use crate::query::{CancellationToken, SortDirection, SortField, SortOrder};
use crate::scheduler::BackgroundScheduler;
use crate::volume::{VolumeKind, discover_mounted_volumes};
use crate::{
    fsevents::{self, EventSource},
    reconciliation::{
        CoalescingPreset, EventBatch, HintCoalescer, RecoveryPlan, plan_batch, plan_stream_start,
        reconcile_committed_root, reconcile_recovery_plan,
    },
};

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
    fn os_proc_available_memory() -> usize;
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
    date_formatter: OnceCell<Retained<NSDateFormatter>>,
    sort_popup: OnceCell<Retained<NSPopUpButton>>,
    direction_button: OnceCell<Retained<NSButton>>,
    hidden_button: OnceCell<Retained<NSButton>>,
    filter_popup: OnceCell<Retained<NSPopUpButton>>,
    result_menu: OnceCell<Retained<NSMenu>>,
    status_item: OnceCell<Retained<NSStatusItem>>,
    status_state_item: OnceCell<Retained<NSMenuItem>>,
    skipped_locations_item: OnceCell<Retained<NSMenuItem>>,
    scheduler: OnceCell<BackgroundScheduler>,
    runtime: Arc<Mutex<RuntimeIndex>>,
    results: RefCell<Vec<SearchResult>>,
    state_title_cache: RefCell<Option<String>>,
    state_detail_cache: RefCell<Option<String>>,
    hot_key: Cell<EventHotKeyRef>,
    launch: Instant,
    sort: Cell<SortOrder>,
    query_generation: Cell<u64>,
    query_cancellation: RefCell<CancellationToken>,
    requested_limit: Cell<usize>,
    exact_total: Cell<usize>,
    event_source: RefCell<Option<EventSource>>,
    event_receiver: RefCell<Option<crossbeam_channel::Receiver<crate::reconciliation::EventBatch>>>,
    event_hints: RefCell<HintCoalescer>,
    external_event_sources: RefCell<HashMap<String, EventSource>>,
    external_event_receivers:
        RefCell<HashMap<String, crossbeam_channel::Receiver<crate::reconciliation::EventBatch>>>,
    external_state_initialized: Cell<bool>,
    coalescing_preset: Cell<CoalescingPreset>,
    show_hidden: Cell<bool>,
    entry_filter: Cell<EntryFilter>,
    user_paused: Arc<AtomicBool>,
    accelerate_pending: Cell<bool>,
    next_reduced_work: Cell<Instant>,
}

struct RuntimeIndex {
    state: FileIndexState,
    projection: Option<Arc<SearchProjection>>,
    error: Option<String>,
    recent_opens: HashMap<u64, u64>,
    pending_query: Option<QueryPublication>,
    freshness: Freshness,
    reconciliation_in_flight: bool,
    query_refresh_needed: bool,
    coverage_reports: Vec<RootCoverage>,
    offline_external_roots: Vec<std::path::PathBuf>,
    recovery_notice: Option<String>,
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
    sort_popup: Retained<NSPopUpButton>,
    direction_button: Retained<NSButton>,
    hidden_button: Retained<NSButton>,
    filter_popup: Retained<NSPopUpButton>,
    result_menu: Retained<NSMenu>,
}

impl Default for AppDelegateIvars {
    fn default() -> Self {
        Self {
            window: OnceCell::new(),
            search_field: OnceCell::new(),
            table: OnceCell::new(),
            state_title: OnceCell::new(),
            state_detail: OnceCell::new(),
            date_formatter: OnceCell::new(),
            sort_popup: OnceCell::new(),
            direction_button: OnceCell::new(),
            hidden_button: OnceCell::new(),
            filter_popup: OnceCell::new(),
            result_menu: OnceCell::new(),
            status_item: OnceCell::new(),
            status_state_item: OnceCell::new(),
            skipped_locations_item: OnceCell::new(),
            scheduler: OnceCell::new(),
            runtime: Arc::new(Mutex::new(RuntimeIndex {
                state: FileIndexState::NotAvailable,
                projection: None,
                error: None,
                recent_opens: HashMap::new(),
                pending_query: None,
                freshness: Freshness::Rebuilding,
                reconciliation_in_flight: false,
                query_refresh_needed: false,
                coverage_reports: Vec::new(),
                offline_external_roots: Vec::new(),
                recovery_notice: None,
            })),
            results: RefCell::new(Vec::new()),
            state_title_cache: RefCell::new(None),
            state_detail_cache: RefCell::new(None),
            hot_key: Cell::new(ptr::null_mut()),
            launch: Instant::now(),
            sort: Cell::new(SortOrder::default()),
            query_generation: Cell::new(0),
            query_cancellation: RefCell::new(CancellationToken::default()),
            requested_limit: Cell::new(100),
            exact_total: Cell::new(0),
            event_source: RefCell::new(None),
            event_receiver: RefCell::new(None),
            event_hints: RefCell::new(HintCoalescer::default()),
            external_event_sources: RefCell::new(HashMap::new()),
            external_event_receivers: RefCell::new(HashMap::new()),
            external_state_initialized: Cell::new(false),
            coalescing_preset: Cell::new(CoalescingPreset::Balanced),
            show_hidden: Cell::new(true),
            entry_filter: Cell::new(EntryFilter::All),
            user_paused: Arc::new(AtomicBool::new(false)),
            accelerate_pending: Cell::new(false),
            next_reduced_work: Cell::new(Instant::now()),
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
            self.restore_coalescing_preset();
            self.restore_hidden_default();
            self.restore_entry_filter();
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
            self.ivars().sort_popup.set(parts.sort_popup).unwrap();
            self.ivars()
                .direction_button
                .set(parts.direction_button)
                .unwrap();
            self.ivars().hidden_button.set(parts.hidden_button).unwrap();
            self.ivars().filter_popup.set(parts.filter_popup).unwrap();
            self.ivars().result_menu.set(parts.result_menu).unwrap();
            self.sync_search_controls();
            let (status_item, status_state_item, skipped_locations_item) =
                build_status_item(mtm, self);
            self.ivars().status_item.set(status_item).unwrap();
            self.ivars()
                .status_state_item
                .set(status_state_item)
                .unwrap();
            self.ivars()
                .skipped_locations_item
                .set(skipped_locations_item)
                .unwrap();

            unsafe { install_hot_key_handler(self) };
            unsafe {
                NSWorkspace::sharedWorkspace()
                    .notificationCenter()
                    .addObserver_selector_name_object(
                        self,
                        sel!(workspaceDidWake:),
                        Some(NSWorkspaceDidWakeNotification),
                        None,
                    );
                for name in [NSWorkspaceDidMountNotification, NSWorkspaceDidUnmountNotification] {
                    NSWorkspace::sharedWorkspace()
                        .notificationCenter()
                        .addObserver_selector_name_object(
                            self,
                            sel!(workspaceVolumesChanged:),
                            Some(name),
                            None,
                        );
                }
            }
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
            unsafe {
                NSWorkspace::sharedWorkspace()
                    .notificationCenter()
                    .removeObserver(self);
            }
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
        #[unsafe(method(tableView:shouldSelectRow:))]
        fn should_select_row(&self, _table: &NSTableView, _row: isize) -> bool { true }

        #[unsafe(method(tableView:menuForTableColumn:row:))]
        fn menu_for_table_row(
            &self,
            table: &NSTableView,
            _column: Option<&NSTableColumn>,
            row: isize,
        ) -> Option<&NSMenu> {
            if row < 0 || row as usize >= self.ivars().results.borrow().len() {
                return None;
            }
            table.selectRowIndexes_byExtendingSelection(
                &objc2_foundation::NSIndexSet::indexSetWithIndex(row as usize),
                false,
            );
            self.ivars().result_menu.get().map(|menu| &**menu)
        }

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
                "name" => format!("{}  {}", entry_kind_icon(result.kind), result.name),
                "path" => result.path.to_string_lossy().into_owned(),
                "modified" => format_file_time(
                    &self.ivars().date_formatter,
                    result.modified_ns,
                ),
                "created" => format_file_time(
                    &self.ivars().date_formatter,
                    result.created_ns,
                ),
                "size" => human_file_size(result.size),
                _ => String::new(),
            };
            let label = NSTextField::labelWithString(
                &objc2_foundation::NSString::from_str(&value),
                self.mtm(),
            );
            label.setFont(Some(&NSFont::systemFontOfSize(13.0)));
            let color = if identifier == "path" {
                NSColor::secondaryLabelColor()
            } else {
                NSColor::labelColor()
            };
            label.setTextColor(Some(&color));
            label.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
            if identifier == "name" {
                if let Some(cell) = label.cell() {
                    cell.setMenu(self.ivars().result_menu.get().map(|menu| &**menu));
                }
            }
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

        #[unsafe(method(pauseIndexing:))]
        fn pause_indexing_action(&self, _sender: Option<&AnyObject>) {
            self.ivars().user_paused.store(true, Ordering::Release);
        }

        #[unsafe(method(resumeIndexing:))]
        fn resume_indexing_action(&self, _sender: Option<&AnyObject>) {
            self.ivars().user_paused.store(false, Ordering::Release);
            self.ivars().accelerate_pending.set(true);
            self.poll_fsevents();
        }

        #[unsafe(method(processPendingChanges:))]
        fn process_pending_changes_action(&self, _sender: Option<&AnyObject>) {
            self.ivars().accelerate_pending.set(true);
            self.poll_fsevents();
        }

        #[unsafe(method(rebuildConfiguredVolume:))]
        fn rebuild_configured_volume_action(&self, _sender: Option<&AnyObject>) {
            self.request_configured_volume_rebuild();
        }

        #[unsafe(method(copySelectedPath:))]
        fn copy_selected_path_action(&self, _sender: Option<&AnyObject>) {
            self.dispatch_selected(ResultAction::CopyPath);
        }

        #[unsafe(method(openSelected:))]
        fn open_selected_action(&self, _sender: Option<&AnyObject>) {
            self.dispatch_selected(ResultAction::Open);
        }

        #[unsafe(method(openSelectedWith:))]
        fn open_selected_with_action(&self, _sender: Option<&AnyObject>) {
            self.dispatch_selected(ResultAction::OpenWith);
        }

        #[unsafe(method(revealSelected:))]
        fn reveal_selected_action(&self, _sender: Option<&AnyObject>) {
            self.dispatch_selected(ResultAction::Reveal);
        }

        #[unsafe(method(copySelectedItem:))]
        fn copy_selected_item_action(&self, _sender: Option<&AnyObject>) {
            self.dispatch_selected(ResultAction::CopyItem);
        }

        #[unsafe(method(sortByRelevance:))]
        fn sort_by_relevance_action(&self, _sender: Option<&AnyObject>) {
            self.select_sort(SortField::Relevance);
        }

        #[unsafe(method(sortSelectionChanged:))]
        fn sort_selection_changed_action(&self, sender: Option<&AnyObject>) {
            let Some(popup) = sender.and_then(|sender| sender.downcast_ref::<NSPopUpButton>()) else {
                return;
            };
            if let Some(field) = sort_field_for_popup_index(popup.indexOfSelectedItem()) {
                self.choose_sort_field(field);
            }
        }

        #[unsafe(method(filterSelectionChanged:))]
        fn filter_selection_changed_action(&self, sender: Option<&AnyObject>) {
            let Some(popup) = sender.and_then(|sender| sender.downcast_ref::<NSPopUpButton>()) else { return };
            let filter = match popup.indexOfSelectedItem() {
                1 => EntryFilter::Files,
                2 => EntryFilter::Folders,
                _ => EntryFilter::All,
            };
            self.ivars().entry_filter.set(filter);
            self.ivars().requested_limit.set(100);
            self.persist_entry_filter(filter);
            self.run_search();
        }

        #[unsafe(method(sortByCreationTime:))]
        fn sort_by_creation_time_action(&self, _sender: Option<&AnyObject>) {
            self.select_sort(SortField::CreationTime);
        }

        #[unsafe(method(toggleSortDirection:))]
        fn toggle_sort_direction_action(&self, _sender: Option<&AnyObject>) {
            self.select_sort(self.ivars().sort.get().field);
        }

        #[unsafe(method(useResponsiveIndexing:))]
        fn use_responsive_indexing_action(&self, _sender: Option<&AnyObject>) {
            self.select_coalescing_preset(CoalescingPreset::Responsive);
        }

        #[unsafe(method(useBalancedIndexing:))]
        fn use_balanced_indexing_action(&self, _sender: Option<&AnyObject>) {
            self.select_coalescing_preset(CoalescingPreset::Balanced);
        }

        #[unsafe(method(useLowEnergyIndexing:))]
        fn use_low_energy_indexing_action(&self, _sender: Option<&AnyObject>) {
            self.select_coalescing_preset(CoalescingPreset::LowEnergy);
        }

        #[unsafe(method(workspaceDidWake:))]
        fn workspace_did_wake(&self, _notification: &NSNotification) {
            self.ivars().event_receiver.borrow_mut().take();
            self.ivars().event_source.borrow_mut().take();
            self.start_fsevents_if_ready();
        }

        #[unsafe(method(showSkippedLocations:))]
        fn show_skipped_locations_action(&self, _sender: Option<&AnyObject>) {
            self.show_skipped_locations();
        }

        #[unsafe(method(showExternalVolumes:))]
        fn show_external_volumes_action(&self, _sender: Option<&AnyObject>) {
            self.show_external_volumes();
        }

        #[unsafe(method(toggleHiddenResults:))]
        fn toggle_hidden_results_action(&self, _sender: Option<&AnyObject>) {
            self.ivars().show_hidden.set(!self.ivars().show_hidden.get());
            self.ivars().requested_limit.set(100);
            self.sync_search_controls();
            self.run_search();
        }

        #[unsafe(method(workspaceVolumesChanged:))]
        fn workspace_volumes_changed(&self, _notification: &NSNotification) {
            self.refresh_external_volume_state();
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
        self.ivars().accelerate_pending.set(true);
        self.poll_fsevents();
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
        let user_paused = Arc::clone(&self.ivars().user_paused);
        let resource_root = root.clone();
        let data_directory = default_data_directory();
        let has_existing_index = data_directory.join("index.sqlite3").exists()
            && data_directory.join("search.projection").exists();
        if !has_existing_index {
            runtime.lock().unwrap().state = FileIndexState::Rebuilding { scanned_entries: 0 };
        }
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
                        loop {
                            match work_allowance(
                                user_paused.load(Ordering::Acquire),
                                current_resource_conditions(&resource_root),
                            ) {
                                WorkAllowance::Paused => {
                                    std::thread::sleep(Duration::from_millis(250));
                                }
                                WorkAllowance::Reduced => {
                                    if scanned_entries % 256 == 0 {
                                        std::thread::sleep(Duration::from_millis(10));
                                    }
                                    break;
                                }
                                WorkAllowance::Normal => break,
                            }
                        }
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
                        runtime.freshness = Freshness::Current;
                        runtime.query_refresh_needed = true;
                        runtime.coverage_reports = vec![built.coverage_report];
                        runtime.recovery_notice = built.recovery_archive.map(|path| {
                            format!(
                                "Recovered a new File Index; damaged database preserved at {}",
                                path.display()
                            )
                        });
                    }
                    Err(error) => {
                        runtime.state = FileIndexState::NotAvailable;
                        runtime.error = Some(error);
                        runtime.freshness = Freshness::Rebuilding;
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
        self.start_fsevents_if_ready();
        if !self.ivars().external_state_initialized.get()
            && self.ivars().runtime.lock().unwrap().projection.is_some()
        {
            self.ivars().external_state_initialized.set(true);
            self.refresh_external_volume_state();
        }
        self.poll_fsevents();
        self.poll_external_fsevents();
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
        let paused = self.ivars().user_paused.load(Ordering::Acquire);
        let title = if paused {
            "索引已暂停".to_owned()
        } else {
            match runtime.freshness {
                Freshness::CatchingUp => "正在补齐索引 · 结果可用".to_owned(),
                Freshness::Offline => "索引离线".to_owned(),
                Freshness::Current => "●  索引已更新".to_owned(),
                Freshness::Rebuilding => match runtime.state {
                    FileIndexState::Rebuilding { scanned_entries } => {
                        format!("正在建立索引 · 已扫描 {} 项", format_count(scanned_entries))
                    }
                    _ => "正在加载索引".to_owned(),
                },
            }
        };
        let coverage = overall_coverage(&runtime.coverage_reports);
        let detail = runtime.error.clone().unwrap_or_else(|| {
            if paused {
                "Indexing is paused; committed search results remain available.".into()
            } else if runtime.freshness == Freshness::CatchingUp {
                "Applying pending filesystem changes to the committed File Index…".into()
            } else if let Some(coverage) = coverage {
                let status = format!(
                    "Freshness: {:?} · Coverage: {:?} across {} configured root(s) · {} external Offline",
                    runtime.freshness,
                    coverage,
                    runtime.coverage_reports.len(),
                    runtime.offline_external_roots.len()
                );
                runtime
                    .recovery_notice
                    .as_ref()
                    .map_or(status.clone(), |notice| format!("{notice} · {status}"))
            } else {
                runtime.state.detail()
            }
        });
        if self.ivars().state_title_cache.borrow().as_deref() != Some(title.as_str()) {
            if let Some(label) = self.ivars().state_title.get() {
                label.setStringValue(&objc2_foundation::NSString::from_str(&title));
            }
            *self.ivars().state_title_cache.borrow_mut() = Some(title.clone());
        }
        let result_count = format!("{} 个结果", self.ivars().exact_total.get());
        if self.ivars().state_detail_cache.borrow().as_deref() != Some(result_count.as_str()) {
            if let Some(label) = self.ivars().state_detail.get() {
                label.setStringValue(&objc2_foundation::NSString::from_str(&result_count));
            }
            *self.ivars().state_detail_cache.borrow_mut() = Some(result_count);
        }
        if let Some(item) = self.ivars().status_state_item.get() {
            let coverage_title = match coverage {
                Some(Coverage::Complete) => "Overall Coverage: Complete",
                Some(Coverage::Partial) => "Overall Coverage: Partial",
                None => "Overall Coverage: Not Available",
            };
            item.setTitle(&objc2_foundation::NSString::from_str(coverage_title));
            let root_detail = runtime
                .coverage_reports
                .first()
                .map(|report| {
                    format!(
                        "{} — {:?} ({} skipped)",
                        report.root.display(),
                        report.coverage,
                        report.skipped.len()
                    )
                })
                .unwrap_or_else(|| detail.clone());
            item.setSubtitle(Some(&objc2_foundation::NSString::from_str(&root_detail)));
        }
        if let Some(item) = self.ivars().skipped_locations_item.get() {
            let skipped_count: usize = runtime
                .coverage_reports
                .iter()
                .map(|report| report.skipped.len())
                .sum();
            item.setTitle(&objc2_foundation::NSString::from_str(&format!(
                "Skipped Locations… ({skipped_count})"
            )));
            item.setEnabled(skipped_count > 0);
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
        let refresh_query = runtime.query_refresh_needed;
        runtime.query_refresh_needed = false;
        drop(runtime);
        if refresh_query {
            self.run_search();
        }
    }

    fn start_fsevents_if_ready(&self) {
        if self.ivars().event_source.borrow().is_some() {
            return;
        }
        let ready = self.ivars().runtime.lock().unwrap().projection.is_some();
        if !ready {
            return;
        }
        let Some(root) = configured_root().and_then(|root| root.canonicalize().ok()) else {
            return;
        };
        use std::os::unix::fs::MetadataExt;
        let Ok(metadata) = std::fs::metadata(&root) else {
            return;
        };
        let volume_id = metadata.dev();
        let Ok(identity) = fsevents::stream_identity(&root) else {
            return;
        };
        let Some((checkpoint, generation)) =
            IndexStore::open(&default_data_directory().join("index.sqlite3"))
                .ok()
                .and_then(|store| {
                    let committed = store.latest_committed().ok().flatten()?;
                    Some((
                        store.checkpoint(volume_id, &root).ok().flatten(),
                        committed.generation,
                    ))
                })
        else {
            return;
        };
        let start_plan = plan_stream_start(checkpoint.as_ref(), &identity, generation);
        let since_event_id = match &start_plan {
            RecoveryPlan::Replay { since_event_id } => Some(*since_event_id),
            _ => None,
        };
        match EventSource::start(
            &root,
            identity.clone(),
            since_event_id,
            self.ivars().coalescing_preset.get().window().as_secs_f64(),
        ) {
            Ok((source, receiver)) => {
                *self.ivars().event_receiver.borrow_mut() = Some(receiver);
                *self.ivars().event_source.borrow_mut() = Some(source);
                if !matches!(start_plan, RecoveryPlan::Replay { .. }) {
                    self.ivars().event_hints.borrow_mut().push(EventBatch {
                        stream_identity: identity,
                        highest_event_id: fsevents::current_event_id(),
                        paths: vec![root],
                        history_lost: false,
                        ids_wrapped: false,
                        root_changed: true,
                    });
                }
            }
            Err(error) => self.ivars().runtime.lock().unwrap().error = Some(error),
        }
    }

    fn poll_fsevents(&self) {
        let batches: Vec<_> = self
            .ivars()
            .event_receiver
            .borrow()
            .as_ref()
            .map(|receiver| receiver.try_iter().collect())
            .unwrap_or_default();
        let data_directory = default_data_directory();
        for mut batch in batches {
            batch
                .paths
                .retain(|path| !path.starts_with(&data_directory));
            if !batch.paths.is_empty()
                || batch.history_lost
                || batch.ids_wrapped
                || batch.root_changed
            {
                self.ivars().event_hints.borrow_mut().push(batch);
            }
        }
        if !self.ivars().event_hints.borrow().has_pending() {
            return;
        }
        let root = configured_root().and_then(|root| root.canonicalize().ok());
        let Some(root) = root else { return };
        let allowance = work_allowance(
            self.ivars().user_paused.load(Ordering::Acquire),
            current_resource_conditions(&root),
        );
        if allowance == WorkAllowance::Paused {
            return;
        }
        let accelerated = self.ivars().accelerate_pending.replace(false);
        if allowance == WorkAllowance::Reduced
            && !accelerated
            && Instant::now() < self.ivars().next_reduced_work.get()
        {
            return;
        }
        if allowance == WorkAllowance::Reduced {
            self.ivars()
                .next_reduced_work
                .set(Instant::now() + Duration::from_secs(2));
        }
        {
            let mut runtime = self.ivars().runtime.lock().unwrap();
            if runtime.reconciliation_in_flight {
                return;
            }
            runtime.reconciliation_in_flight = true;
            runtime.freshness = Freshness::CatchingUp;
        }
        let Some(mut batch) = self.ivars().event_hints.borrow_mut().take() else {
            return;
        };
        let plan = plan_batch(&root, &batch);
        let rebuilding = matches!(plan, RecoveryPlan::RebuildVolume { .. });
        if rebuilding || batch.highest_event_id == 0 {
            batch.highest_event_id = fsevents::current_event_id();
        }
        self.ivars().runtime.lock().unwrap().freshness = if rebuilding {
            Freshness::Rebuilding
        } else {
            Freshness::CatchingUp
        };
        let data_directory = default_data_directory();
        let runtime = Arc::clone(&self.ivars().runtime);
        let scheduled = self
            .ivars()
            .scheduler
            .get()
            .expect("scheduler initialized")
            .try_schedule(move || {
                let result = reconcile_recovery_plan(&root, &data_directory, &batch, &plan);
                let mut runtime = runtime.lock().unwrap();
                runtime.reconciliation_in_flight = false;
                match result {
                    Ok(reconciled) => {
                        runtime.state = FileIndexState::Current {
                            coverage: reconciled.coverage,
                        };
                        runtime.projection = Some(Arc::new(reconciled.projection));
                        runtime.freshness = Freshness::Current;
                        runtime.error = None;
                        runtime.query_refresh_needed = true;
                        runtime.coverage_reports = vec![reconciled.coverage_report];
                    }
                    Err(error) => {
                        runtime.freshness = if rebuilding {
                            Freshness::Rebuilding
                        } else {
                            Freshness::CatchingUp
                        };
                        runtime.error = Some(error);
                    }
                }
            });
        if scheduled.is_err() {
            let mut runtime = self.ivars().runtime.lock().unwrap();
            runtime.reconciliation_in_flight = false;
            runtime.freshness = if rebuilding {
                Freshness::Rebuilding
            } else {
                Freshness::CatchingUp
            };
            runtime.error = Some("could not schedule FSEvents reconciliation".into());
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
        let show_hidden = self.ivars().show_hidden.get();
        let entry_filter = self.ivars().entry_filter.get();
        let limit = self.ivars().requested_limit.get();
        let _ = self
            .ivars()
            .scheduler
            .get()
            .expect("scheduler initialized")
            .try_schedule(move || {
                if let Ok(ranked) = projection.search_ranked_with_filters(
                    &query,
                    &recent_opens,
                    limit,
                    sort,
                    &cancellation,
                    show_hidden,
                    entry_filter,
                ) && !ranked.cancelled
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
        self.sync_search_controls();
        self.run_search();
    }

    fn choose_sort_field(&self, field: SortField) {
        let current = self.ivars().sort.get();
        let sort = SortOrder {
            field,
            direction: if current.field == field {
                current.direction
            } else {
                SortDirection::Ascending
            },
        };
        self.ivars().sort.set(sort);
        self.ivars().requested_limit.set(100);
        self.persist_sort_order(sort);
        self.sync_search_controls();
        self.run_search();
    }

    fn sync_search_controls(&self) {
        let sort = self.ivars().sort.get();
        if let Some(popup) = self.ivars().sort_popup.get() {
            popup.selectItemAtIndex(sort_popup_index(sort.field));
        }
        if let Some(button) = self.ivars().direction_button.get() {
            button.setTitle(match sort.direction {
                SortDirection::Ascending => ns_string!("升序 ↕"),
                SortDirection::Descending => ns_string!("降序 ↕"),
            });
        }
        if let Some(popup) = self.ivars().filter_popup.get() {
            popup.selectItemAtIndex(match self.ivars().entry_filter.get() {
                EntryFilter::All => 0,
                EntryFilter::Files => 1,
                EntryFilter::Folders => 2,
            });
        }
        if let Some(table) = self.ivars().table.get() {
            for column in table.tableColumns().iter() {
                let identifier = column.identifier().to_string();
                let base = match identifier.as_str() {
                    "name" => "名称",
                    "path" => "路径",
                    "modified" => "修改时间",
                    "created" => "创建时间",
                    "size" => "大小",
                    _ => continue,
                };
                let title = if sort.field == sort_field_for_column(&identifier) {
                    format!(
                        "{base} {}",
                        if sort.direction == SortDirection::Ascending {
                            "↑"
                        } else {
                            "↓"
                        }
                    )
                } else {
                    base.to_owned()
                };
                column.setTitle(&objc2_foundation::NSString::from_str(&title));
            }
        }
        if let Some(button) = self.ivars().hidden_button.get() {
            button.setTitle(if self.ivars().show_hidden.get() {
                ns_string!("隐藏项目：显示  ⌘⇧.")
            } else {
                ns_string!("隐藏项目：隐藏  ⌘⇧.")
            });
        }
    }

    fn persist_entry_filter(&self, filter: EntryFilter) {
        let value = match filter {
            EntryFilter::All => 0,
            EntryFilter::Files => 1,
            EntryFilter::Folders => 2,
        };
        NSUserDefaults::standardUserDefaults()
            .setInteger_forKey(value, ns_string!("EveryfileEntryFilter"));
    }

    fn restore_entry_filter(&self) {
        self.ivars().entry_filter.set(
            match NSUserDefaults::standardUserDefaults()
                .integerForKey(ns_string!("EveryfileEntryFilter"))
            {
                1 => EntryFilter::Files,
                2 => EntryFilter::Folders,
                _ => EntryFilter::All,
            },
        );
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

    fn restore_coalescing_preset(&self) {
        let defaults = NSUserDefaults::standardUserDefaults();
        let preset = match defaults.integerForKey(ns_string!("EveryfileCoalescingPreset")) {
            0 => CoalescingPreset::Responsive,
            2 => CoalescingPreset::LowEnergy,
            _ => CoalescingPreset::Balanced,
        };
        self.ivars().coalescing_preset.set(preset);
    }

    fn restore_hidden_default(&self) {
        let value = NSUserDefaults::standardUserDefaults()
            .integerForKey(ns_string!("EveryfileHiddenDefault"));
        self.ivars().show_hidden.set(value != 1);
    }

    fn select_coalescing_preset(&self, preset: CoalescingPreset) {
        self.ivars().coalescing_preset.set(preset);
        let value = match preset {
            CoalescingPreset::Responsive => 0,
            CoalescingPreset::Balanced => 1,
            CoalescingPreset::LowEnergy => 2,
        };
        NSUserDefaults::standardUserDefaults()
            .setInteger_forKey(value, ns_string!("EveryfileCoalescingPreset"));
        self.ivars().event_receiver.borrow_mut().take();
        self.ivars().event_source.borrow_mut().take();
        self.start_fsevents_if_ready();
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
        if !succeeded {
            if let Some(detail) = self.ivars().state_detail.get() {
                detail.setStringValue(ns_string!("操作失败：请检查项目是否仍存在或权限是否足够"));
            }
        }
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

    fn request_configured_volume_rebuild(&self) {
        let Some(root) = configured_root().and_then(|root| root.canonicalize().ok()) else {
            return;
        };
        let Ok(identity) = fsevents::stream_identity(&root) else {
            return;
        };
        self.ivars().event_hints.borrow_mut().push(EventBatch {
            stream_identity: identity,
            highest_event_id: fsevents::current_event_id(),
            paths: vec![root],
            history_lost: false,
            ids_wrapped: false,
            root_changed: true,
        });
        self.ivars().user_paused.store(false, Ordering::Release);
        self.ivars().accelerate_pending.set(true);
        self.poll_fsevents();
    }

    fn show_skipped_locations(&self) {
        let reports = self
            .ivars()
            .runtime
            .lock()
            .unwrap()
            .coverage_reports
            .clone();
        let mut lines = Vec::new();
        for report in reports {
            for location in report.skipped {
                lines.push(format!(
                    "{}\n  {}",
                    location.path.display(),
                    location.reason
                ));
            }
        }
        let Some(summary) = skipped_locations_summary(&lines, 100) else {
            return;
        };
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(ns_string!("Skipped Locations"));
        alert.setInformativeText(&objc2_foundation::NSString::from_str(&summary));
        // NSAlert does not synthesize a dismiss button. Without one, runModal
        // has no reliable response that ends its nested event loop.
        alert.addButtonWithTitle(ns_string!("关闭"));
        alert.runModal();
    }

    fn show_external_volumes(&self) {
        let Ok(volumes) = discover_mounted_volumes() else {
            return;
        };
        let Some(volume) = volumes
            .into_iter()
            .find(|volume| volume.kind() == VolumeKind::ExternalLocal)
        else {
            let alert = NSAlert::new(self.mtm());
            alert.setMessageText(ns_string!("External Volumes"));
            alert.setInformativeText(ns_string!("No external local volume is currently mounted."));
            alert.runModal();
            return;
        };
        let database = default_data_directory().join("index.sqlite3");
        let Ok(store) = IndexStore::open(&database) else {
            return;
        };
        let _ = store.observe_volume(&volume);
        let enabled = store
            .volume_configurations()
            .ok()
            .and_then(|configs| {
                configs
                    .into_iter()
                    .find(|config| config.identity == volume.identity)
            })
            .is_some_and(|config| config.enabled);
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(ns_string!("External Volume"));
        alert.setInformativeText(&objc2_foundation::NSString::from_str(&format!(
            "{}\n{}",
            volume.mount_path.display(),
            if enabled {
                "Enabled for indexing"
            } else {
                "Not indexed until you explicitly enable it"
            }
        )));
        alert.addButtonWithTitle(if enabled {
            ns_string!("Disable")
        } else {
            ns_string!("Enable")
        });
        alert.addButtonWithTitle(ns_string!("Cancel"));
        if alert.runModal() != NSAlertFirstButtonReturn {
            return;
        }
        if !store
            .set_volume_enabled(&volume.identity, !enabled)
            .unwrap_or(false)
        {
            return;
        }
        if enabled {
            self.ivars()
                .external_event_sources
                .borrow_mut()
                .remove(&volume.identity);
            self.ivars()
                .external_event_receivers
                .borrow_mut()
                .remove(&volume.identity);
            self.rebuild_combined_projection();
        } else {
            self.start_external_fsevents(volume.identity, volume.mount_path.clone());
            self.schedule_external_index(volume.mount_path);
        }
    }

    fn schedule_external_index(&self, root: std::path::PathBuf) {
        let runtime = Arc::clone(&self.ivars().runtime);
        let data_directory = default_data_directory();
        let _ = self
            .ivars()
            .scheduler
            .get()
            .expect("scheduler initialized")
            .try_schedule(move || {
                let result = build_first_index_with_progress(&root, &data_directory, |_| {});
                let mut runtime = runtime.lock().unwrap();
                match result {
                    Ok(built) => {
                        runtime.projection = Some(Arc::new(built.projection));
                        runtime
                            .coverage_reports
                            .retain(|report| report.root != built.coverage_report.root);
                        runtime.coverage_reports.push(built.coverage_report);
                        runtime.query_refresh_needed = true;
                    }
                    Err(error) => runtime.error = Some(error),
                }
            });
    }

    fn start_external_fsevents(&self, identity: String, root: std::path::PathBuf) {
        if self
            .ivars()
            .external_event_sources
            .borrow()
            .contains_key(&identity)
        {
            return;
        }
        use std::os::unix::fs::MetadataExt;
        let Ok(metadata) = std::fs::metadata(&root) else {
            return;
        };
        let checkpoint = IndexStore::open(&default_data_directory().join("index.sqlite3"))
            .ok()
            .and_then(|store| store.checkpoint(metadata.dev(), &root).ok().flatten())
            .filter(|checkpoint| checkpoint.stream_identity == identity)
            .map(|checkpoint| checkpoint.event_id);
        if let Ok((source, receiver)) = EventSource::start(
            &root,
            identity.clone(),
            checkpoint,
            self.ivars().coalescing_preset.get().window().as_secs_f64(),
        ) {
            self.ivars()
                .external_event_sources
                .borrow_mut()
                .insert(identity.clone(), source);
            self.ivars()
                .external_event_receivers
                .borrow_mut()
                .insert(identity, receiver);
        }
    }

    fn poll_external_fsevents(&self) {
        let changed: Vec<_> = self
            .ivars()
            .external_event_receivers
            .borrow()
            .iter()
            .filter_map(|(identity, receiver)| {
                receiver
                    .try_iter()
                    .max_by_key(|batch| batch.highest_event_id)
                    .map(|batch| (identity.clone(), batch))
            })
            .collect();
        if changed.is_empty() {
            return;
        }
        let Ok(store) = IndexStore::open(&default_data_directory().join("index.sqlite3")) else {
            return;
        };
        let Ok(configs) = store.volume_configurations() else {
            return;
        };
        for (identity, batch) in changed {
            if let Some(config) = configs
                .iter()
                .find(|config| config.identity == identity && config.enabled)
            {
                self.schedule_external_reconciliation(config.mount_path.clone(), batch);
            }
        }
    }

    fn schedule_external_reconciliation(&self, root: std::path::PathBuf, batch: EventBatch) {
        let runtime = Arc::clone(&self.ivars().runtime);
        let data_directory = default_data_directory();
        let _ = self
            .ivars()
            .scheduler
            .get()
            .expect("scheduler initialized")
            .try_schedule(move || {
                let result = reconcile_committed_root(&root, &data_directory, &batch);
                let mut runtime = runtime.lock().unwrap();
                match result {
                    Ok(reconciled) => {
                        runtime.projection = Some(Arc::new(reconciled.projection));
                        runtime
                            .coverage_reports
                            .retain(|report| report.root != reconciled.coverage_report.root);
                        runtime.coverage_reports.push(reconciled.coverage_report);
                        runtime.error = None;
                        runtime.query_refresh_needed = true;
                    }
                    Err(error) => runtime.error = Some(error),
                }
            });
    }

    fn rebuild_combined_projection(&self) {
        let runtime = Arc::clone(&self.ivars().runtime);
        let data_directory = default_data_directory();
        let _ = self
            .ivars()
            .scheduler
            .get()
            .expect("scheduler initialized")
            .try_schedule(move || {
                let result = IndexStore::open(&data_directory.join("index.sqlite3"))
                    .map_err(|error| error.to_string())
                    .and_then(|store| {
                        SearchProjection::build_from_store(
                            &data_directory.join("search.projection"),
                            &store,
                        )
                        .map_err(|error| error.to_string())
                    });
                let mut runtime = runtime.lock().unwrap();
                match result {
                    Ok(projection) => {
                        runtime.projection = Some(Arc::new(projection));
                        runtime.query_refresh_needed = true;
                    }
                    Err(error) => runtime.error = Some(error),
                }
            });
    }

    fn refresh_external_volume_state(&self) {
        let Ok(volumes) = discover_mounted_volumes() else {
            return;
        };
        let Ok(store) = IndexStore::open(&default_data_directory().join("index.sqlite3")) else {
            return;
        };
        for volume in &volumes {
            let _ = store.observe_volume(volume);
        }
        let mounted: std::collections::HashMap<_, _> = volumes
            .into_iter()
            .map(|volume| (volume.identity, volume.mount_path))
            .collect();
        if let Ok(configs) = store.volume_configurations() {
            let offline: Vec<_> = configs
                .iter()
                .filter(|config| {
                    config.enabled && !config.internal && !mounted.contains_key(&config.identity)
                })
                .map(|config| config.mount_path.clone())
                .collect();
            self.ivars().runtime.lock().unwrap().offline_external_roots = offline;
            for config in configs
                .into_iter()
                .filter(|config| config.enabled && !config.internal)
            {
                if let Some(path) = mounted.get(&config.identity) {
                    self.start_external_fsevents(config.identity.clone(), path.clone());
                    self.schedule_external_reconciliation(
                        path.clone(),
                        EventBatch {
                            stream_identity: config.identity,
                            highest_event_id: fsevents::current_event_id(),
                            paths: vec![path.clone()],
                            history_lost: false,
                            ids_wrapped: false,
                            root_changed: false,
                        },
                    );
                } else {
                    self.ivars()
                        .external_event_sources
                        .borrow_mut()
                        .remove(&config.identity);
                    self.ivars()
                        .external_event_receivers
                        .borrow_mut()
                        .remove(&config.identity);
                }
            }
        }
    }

    fn show_shortcut_settings(&self) {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(ns_string!("Global Shortcut"));
        alert.setInformativeText(ns_string!(
            "Choose the global shortcut, or change whether hidden entries are shown by default. Settings are saved for future launches."
        ));
        alert.addButtonWithTitle(ns_string!("⌘⌥Space"));
        alert.addButtonWithTitle(ns_string!("⌘⇧Space"));
        alert.addButtonWithTitle(if self.ivars().show_hidden.get() {
            ns_string!("Hide Hidden by Default")
        } else {
            ns_string!("Show Hidden by Default")
        });
        alert.addButtonWithTitle(ns_string!("Cancel"));
        let response = alert.runModal();
        if response == NSAlertThirdButtonReturn {
            let show_hidden = !self.ivars().show_hidden.get();
            self.ivars().show_hidden.set(show_hidden);
            NSUserDefaults::standardUserDefaults().setInteger_forKey(
                if show_hidden { 2 } else { 1 },
                ns_string!("EveryfileHiddenDefault"),
            );
            self.run_search();
            return;
        }
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
            ResultAction::OpenWith => {
                let Some(mtm) = MainThreadMarker::new() else {
                    return false;
                };
                let panel = NSOpenPanel::openPanel(mtm);
                panel.setCanChooseFiles(true);
                panel.setCanChooseDirectories(true);
                panel.setAllowsMultipleSelection(false);
                panel.setTitle(Some(ns_string!("选择打开方式")));
                if panel.runModal() != 1 {
                    return false;
                }
                let Some(application) = panel.URLs().firstObject() else {
                    return false;
                };
                let Some(application_path) = application.path() else {
                    return false;
                };
                NSWorkspace::sharedWorkspace()
                    .openFile_withApplication(&path, Some(&application_path))
            }
            ResultAction::Reveal => NSWorkspace::sharedWorkspace()
                .selectFile_inFileViewerRootedAtPath(Some(&path), ns_string!("")),
            ResultAction::CopyPath => {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                pasteboard.setString_forType(&path, unsafe { NSPasteboardTypeString })
            }
            ResultAction::CopyItem => {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                let url = NSURL::fileURLWithPath(&path);
                let Some(url_string) = url.absoluteString() else {
                    return false;
                };
                unsafe { pasteboard.setPropertyList_forType(&url_string, NSPasteboardTypeFileURL) }
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

fn sort_popup_index(field: SortField) -> isize {
    match field {
        SortField::Relevance => 0,
        SortField::ModificationTime => 1,
        SortField::CreationTime => 2,
        SortField::FileName => 3,
        SortField::FullPath => 4,
        SortField::FileSize => 5,
    }
}

fn sort_field_for_column(identifier: &str) -> SortField {
    match identifier {
        "name" => SortField::FileName,
        "path" => SortField::FullPath,
        "modified" => SortField::ModificationTime,
        "created" => SortField::CreationTime,
        "size" => SortField::FileSize,
        _ => SortField::Relevance,
    }
}

fn sort_field_for_popup_index(index: isize) -> Option<SortField> {
    match index {
        0 => Some(SortField::Relevance),
        1 => Some(SortField::ModificationTime),
        2 => Some(SortField::CreationTime),
        3 => Some(SortField::FileName),
        4 => Some(SortField::FullPath),
        5 => Some(SortField::FileSize),
        _ => None,
    }
}

fn human_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    match bytes {
        0 => "—".into(),
        1..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.0} KB", bytes as f64 / KB),
        1_048_576..=1_073_741_823 => format!("{:.1} MB", bytes as f64 / MB),
        _ => format!("{:.1} GB", bytes as f64 / GB),
    }
}

fn entry_kind_icon(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Directory => "📁",
        EntryKind::File => "📄",
        EntryKind::Symlink => "🔗",
        EntryKind::Other => "◼",
    }
}

fn format_file_time(
    formatter: &OnceCell<Retained<NSDateFormatter>>,
    nanoseconds_since_epoch: Option<i64>,
) -> String {
    let Some(nanoseconds) = nanoseconds_since_epoch else {
        return "—".into();
    };
    let formatter = formatter.get_or_init(|| {
        let formatter = NSDateFormatter::new();
        formatter.setDateStyle(NSDateFormatterStyle::MediumStyle);
        formatter.setTimeStyle(NSDateFormatterStyle::ShortStyle);
        formatter.setDoesRelativeDateFormatting(true);
        formatter
    });
    let date = NSDate::dateWithTimeIntervalSince1970(nanoseconds as f64 / 1_000_000_000.0);
    formatter.stringFromDate(&date).to_string()
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}

fn skipped_locations_summary(lines: &[String], limit: usize) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let shown = lines.len().min(limit.max(1));
    let mut summary = lines[..shown].join("\n\n");
    let remaining = lines.len() - shown;
    if remaining > 0 {
        summary.push_str(&format!(
            "\n\n…另有 {} 个位置未显示",
            format_count(remaining as u64)
        ));
    }
    Some(summary)
}

fn build_search_window(
    mtm: MainThreadMarker,
    snapshot: &AppSnapshot,
    delegate: &Delegate,
) -> SearchWindowParts {
    // The reference is explicitly dark regardless of the system appearance.
    // Fixing the app appearance also keeps native controls and table selection
    // colors consistent with the HTML prototype.
    let dark_appearance = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua });
    NSApplication::sharedApplication(mtm).setAppearance(dark_appearance.as_deref());

    // Variant A is deliberately a wide, dense utility window. Keep these
    // dimensions in points: the prototype's 1060 CSS pixels map 1:1 to AppKit
    // points (and therefore to 2120 pixels on a Retina display).
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1060.0, 610.0));
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
    window.setMinSize(NSSize::new(820.0, 460.0));
    window.center();

    let content = NSView::initWithFrame(NSView::alloc(mtm), frame);
    content.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    let search_frame = NSRect::new(NSPoint::new(0.0, 526.0), NSSize::new(1060.0, 62.0));
    let search_content = NSView::initWithFrame(NSView::alloc(mtm), search_frame);

    let search_icon = NSTextField::labelWithString(ns_string!("⌕"), mtm);
    search_icon.setFrame(NSRect::new(
        NSPoint::new(20.0, 19.0),
        NSSize::new(24.0, 25.0),
    ));
    search_icon.setFont(Some(&NSFont::systemFontOfSize(20.0)));
    search_icon.setTextColor(Some(&NSColor::secondaryLabelColor()));
    search_content.addSubview(&search_icon);

    let search = NSTextField::textFieldWithString(ns_string!(""), mtm);
    search.setFrame(NSRect::new(
        NSPoint::new(44.0, 10.0),
        NSSize::new(900.0, 42.0),
    ));
    search.setPlaceholderString(Some(ns_string!("Search file names and paths")));
    search.setFont(Some(&NSFont::systemFontOfSize(22.0)));
    search.setTextColor(Some(&NSColor::labelColor()));
    search.setBezeled(false);
    search.setBordered(false);
    search.setDrawsBackground(false);
    search.setFocusRingType(objc2_app_kit::NSFocusRingType::None);
    search.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    unsafe { search.setDelegate(Some(ProtocolObject::from_ref(delegate))) };
    search_content.addSubview(&search);

    let shortcut = NSTextField::labelWithString(ns_string!("⌥ Space"), mtm);
    shortcut.setFrame(NSRect::new(
        NSPoint::new(970.0, 21.0),
        NSSize::new(72.0, 21.0),
    ));
    shortcut.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    shortcut.setTextColor(Some(&NSColor::secondaryLabelColor()));
    shortcut.setAlignment(objc2_app_kit::NSTextAlignment::Center);
    search_content.addSubview(&shortcut);

    let toolbar = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 476.0), NSSize::new(1060.0, 50.0)),
    );
    let sort_popup = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(NSPoint::new(12.0, 9.0), NSSize::new(92.0, 32.0)),
        false,
    );
    for title in [
        "相关性",
        "修改时间",
        "创建时间",
        "文件名",
        "完整路径",
        "文件大小",
    ] {
        sort_popup.addItemWithTitle(&objc2_foundation::NSString::from_str(title));
    }
    unsafe {
        sort_popup.setTarget(Some(delegate));
        sort_popup.setAction(Some(sel!(sortSelectionChanged:)));
    }
    toolbar.addSubview(&sort_popup);
    let direction_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            ns_string!("降序 ↕"),
            Some(delegate),
            Some(sel!(toggleSortDirection:)),
            mtm,
        )
    };
    direction_button.setFrame(NSRect::new(
        NSPoint::new(112.0, 9.0),
        NSSize::new(78.0, 32.0),
    ));
    toolbar.addSubview(&direction_button);
    let hidden_button = unsafe {
        NSButton::buttonWithTitle_target_action(
            ns_string!("隐藏项目：显示  ⌘⇧."),
            Some(delegate),
            Some(sel!(toggleHiddenResults:)),
            mtm,
        )
    };
    hidden_button.setFrame(NSRect::new(
        NSPoint::new(198.0, 9.0),
        NSSize::new(170.0, 32.0),
    ));
    toolbar.addSubview(&hidden_button);
    let filter_popup = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(NSPoint::new(376.0, 9.0), NSSize::new(112.0, 32.0)),
        false,
    );
    for title in ["全部", "文件", "文件夹"] {
        filter_popup.addItemWithTitle(&objc2_foundation::NSString::from_str(title));
    }
    unsafe {
        filter_popup.setTarget(Some(delegate));
        filter_popup.setAction(Some(sel!(filterSelectionChanged:)));
    }
    toolbar.addSubview(&filter_popup);

    let table_frame = NSRect::new(NSPoint::new(0.0, 34.0), NSSize::new(1060.0, 442.0));
    let table_content = NSView::initWithFrame(NSView::alloc(mtm), table_frame);
    let table = NSTableView::initWithFrame(
        NSTableView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), table_frame.size),
    );
    table.setRowHeight(31.0);
    table.setUsesAlternatingRowBackgroundColors(false);
    table.setBackgroundColor(&NSColor::clearColor());
    table.setGridStyleMask(objc2_app_kit::NSTableViewGridLineStyle::empty());
    table.setIntercellSpacing(NSSize::new(0.0, 0.0));
    unsafe {
        table.setTarget(Some(delegate));
        table.setDoubleAction(Some(sel!(openSelected:)));
    }
    add_table_column(mtm, &table, "name", "名称", 270.0);
    add_table_column(mtm, &table, "path", "路径", 570.0);
    add_table_column(mtm, &table, "modified", "修改时间", 130.0);
    add_table_column(mtm, &table, "size", "大小", 90.0);
    unsafe {
        table.setDataSource(Some(ProtocolObject::from_ref(delegate)));
        table.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    }

    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), table_frame.size),
    );
    scroll.setDrawsBackground(false);
    scroll.setBackgroundColor(&NSColor::clearColor());
    scroll.setHasVerticalScroller(false);
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    scroll.setDocumentView(Some(&table));
    table_content.addSubview(&scroll);
    let status_title = NSTextField::labelWithString(
        objc2_foundation::NSString::from_str(snapshot.file_index.title()).as_ref(),
        mtm,
    );
    status_title.setFrame(NSRect::new(
        NSPoint::new(830.0, 9.0),
        NSSize::new(210.0, 17.0),
    ));
    status_title.setFont(Some(&NSFont::systemFontOfSize(12.0)));
    status_title.setTextColor(Some(&NSColor::secondaryLabelColor()));
    status_title.setAlignment(objc2_app_kit::NSTextAlignment::Right);
    status_title.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );

    let status_detail = NSTextField::labelWithString(
        objc2_foundation::NSString::from_str(&snapshot.file_index.detail()).as_ref(),
        mtm,
    );
    status_detail.setFrame(NSRect::new(
        NSPoint::new(12.0, 9.0),
        NSSize::new(300.0, 17.0),
    ));
    status_detail.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    status_detail.setTextColor(Some(&NSColor::tertiaryLabelColor()));
    status_detail.setAlignment(objc2_app_kit::NSTextAlignment::Left);
    status_detail.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
    );

    content.addSubview(&search_content);
    content.addSubview(&toolbar);
    content.addSubview(&table_content);
    content.addSubview(&status_title);
    content.addSubview(&status_detail);
    let outer_surface =
        build_glass_surface(mtm, frame, NSGlassEffectViewStyle::Regular, 12.0, &content);
    outer_surface.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    window.setContentView(Some(&outer_surface));
    SearchWindowParts {
        window,
        search_field: search,
        table,
        state_title: status_title,
        state_detail: status_detail,
        sort_popup,
        direction_button,
        hidden_button,
        filter_popup,
        result_menu: build_result_menu(mtm, delegate),
    }
}

fn build_result_menu(mtm: MainThreadMarker, delegate: &Delegate) -> Retained<NSMenu> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), ns_string!("结果操作"));
    for (title, selector) in [
        ("打开", sel!(openSelected:)),
        ("打开方式…", sel!(openSelectedWith:)),
        ("在 Finder 中显示", sel!(revealSelected:)),
        ("拷贝项目", sel!(copySelectedItem:)),
        ("拷贝路径", sel!(copySelectedPath:)),
    ] {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &objc2_foundation::NSString::from_str(title),
                Some(selector),
                ns_string!(""),
            )
        };
        unsafe {
            item.setTarget(Some(delegate));
        }
        menu.addItem(&item);
    }
    menu
}

fn build_glass_surface(
    mtm: MainThreadMarker,
    frame: NSRect,
    style: NSGlassEffectViewStyle,
    corner_radius: f64,
    content_view: &NSView,
) -> Retained<NSView> {
    // NSGlassEffectView is macOS 26's native Liquid Glass surface. Resolve the
    // class dynamically so the existing macOS 15 deployment target keeps its
    // visual-effect fallback instead of taking a hard class-link dependency.
    if let Some(class) = AnyClass::get(c"NSGlassEffectView") {
        let glass: Retained<NSGlassEffectView> = unsafe {
            let allocated: objc2::rc::Allocated<NSGlassEffectView> = msg_send![class, alloc];
            NSGlassEffectView::initWithFrame(allocated, frame)
        };
        glass.setStyle(style);
        glass.setCornerRadius(corner_radius);
        let tint = if style == NSGlassEffectViewStyle::Clear {
            NSColor::colorWithSRGBRed_green_blue_alpha(0.20, 0.28, 0.46, 0.18)
        } else {
            NSColor::colorWithSRGBRed_green_blue_alpha(0.12, 0.18, 0.32, 0.28)
        };
        glass.setTintColor(Some(&tint));
        glass.setContentView(Some(content_view));
        return glass.into_super();
    }

    let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
    effect.setMaterial(if style == NSGlassEffectViewStyle::Clear {
        NSVisualEffectMaterial::Popover
    } else {
        NSVisualEffectMaterial::HUDWindow
    });
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    effect.setState(NSVisualEffectState::FollowsWindowActiveState);
    effect.setAlphaValue(0.96);
    effect.addSubview(content_view);
    effect.into_super()
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
    let header = column.headerCell();
    header.setStringValue(&objc2_foundation::NSString::from_str(title));
    header.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    header.setTextColor(Some(&NSColor::secondaryLabelColor()));
    header.setBackgroundColor(Some(&NSColor::clearColor()));
    header.setDrawsBackground(false);
    header.setBezeled(false);
    header.setBordered(false);
    column.setWidth(width);
    column.setMinWidth(60.0);
    table.addTableColumn(&column);
}

fn build_status_item(
    mtm: MainThreadMarker,
    delegate: &Delegate,
) -> (
    Retained<NSStatusItem>,
    Retained<NSMenuItem>,
    Retained<NSMenuItem>,
) {
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
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Pause Indexing"),
        sel!(pauseIndexing:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Resume Indexing"),
        sel!(resumeIndexing:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Process Pending Changes"),
        sel!(processPendingChanges:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Rebuild Configured Volume"),
        sel!(rebuildConfiguredVolume:),
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
    let skipped_locations = add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Skipped Locations… (0)"),
        sel!(showSkippedLocations:),
        ns_string!(""),
        false,
    );
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
        ns_string!("Open Selected"),
        sel!(openSelected:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Open Selected With…"),
        sel!(openSelectedWith:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Reveal Selected in Finder"),
        sel!(revealSelected:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Copy Selected Item"),
        sel!(copySelectedItem:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("External Volumes…"),
        sel!(showExternalVolumes:),
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
        ns_string!("Indexing: Responsive (~1 s)"),
        sel!(useResponsiveIndexing:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Indexing: Balanced (~5 s)"),
        sel!(useBalancedIndexing:),
        ns_string!(""),
        true,
    );
    add_menu_item(
        mtm,
        &menu,
        delegate,
        ns_string!("Indexing: Low Energy (~15 s)"),
        sel!(useLowEnergyIndexing:),
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
    (status_item, state, skipped_locations)
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
    let hidden_item = add_menu_item(
        mtm,
        &edit_menu,
        delegate,
        ns_string!("Temporarily Toggle Hidden Results"),
        sel!(toggleHiddenResults:),
        ns_string!("."),
        true,
    );
    hidden_item
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Shift);
    add_menu_item(
        mtm,
        &edit_menu,
        delegate,
        ns_string!("External Volumes…"),
        sel!(showExternalVolumes:),
        ns_string!(""),
        true,
    );
    for (title, action) in [
        ("Pause Indexing", sel!(pauseIndexing:)),
        ("Resume Indexing", sel!(resumeIndexing:)),
        ("Process Pending Changes", sel!(processPendingChanges:)),
        ("Rebuild Configured Volume", sel!(rebuildConfiguredVolume:)),
        ("Skipped Locations…", sel!(showSkippedLocations:)),
        ("Settings…", sel!(showSettings:)),
    ] {
        add_menu_item(
            mtm,
            &edit_menu,
            delegate,
            &objc2_foundation::NSString::from_str(title),
            action,
            ns_string!(""),
            true,
        );
    }
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

fn current_resource_conditions(root: &std::path::Path) -> ResourceConditions {
    let process = NSProcessInfo::processInfo();
    let available_memory = unsafe { os_proc_available_memory() };
    ResourceConditions {
        low_power_mode: process.isLowPowerModeEnabled(),
        on_battery: false,
        battery_percent: None,
        severe_thermal_state: matches!(
            process.thermalState(),
            NSProcessInfoThermalState::Serious | NSProcessInfoThermalState::Critical
        ),
        memory_pressure: available_memory > 0 && available_memory < 128 * 1024 * 1024,
        volume_available: root.exists(),
    }
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("Everyfile must start on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let delegate = Delegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
}

#[cfg(test)]
mod ui_format_tests {
    use super::{
        SortField, format_count, skipped_locations_summary, sort_field_for_popup_index,
        sort_popup_index,
    };

    #[test]
    fn index_progress_groups_scanned_entry_count() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(12_345), "12,345");
        assert_eq!(format_count(1_234_567), "1,234,567");
    }

    #[test]
    fn skipped_locations_summary_is_bounded_and_reports_remainder() {
        let lines = (1..=105)
            .map(|value| format!("location {value}"))
            .collect::<Vec<_>>();
        let summary = skipped_locations_summary(&lines, 100).unwrap();

        assert!(summary.contains("location 100"));
        assert!(!summary.contains("location 101"));
        assert!(summary.ends_with("…另有 5 个位置未显示"));
        assert_eq!(skipped_locations_summary(&[], 100), None);
    }

    #[test]
    fn sort_popup_round_trips_every_sort_field() {
        for field in [
            SortField::Relevance,
            SortField::ModificationTime,
            SortField::CreationTime,
            SortField::FileName,
            SortField::FullPath,
            SortField::FileSize,
        ] {
            assert_eq!(
                sort_field_for_popup_index(sort_popup_index(field)),
                Some(field)
            );
        }
        assert_eq!(sort_field_for_popup_index(-1), None);
    }
}
