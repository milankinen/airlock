//! Notification flow on macOS.
//!
//! Asks for the notification permission, registers the "Allow" and "Deny"
//! buttons, shows the notification and waits for the choice of the user.
//! All system calls run on the main thread. The system sends its replies on
//! other threads, so the main thread runs the event loop in short steps and
//! checks shared state between the steps.
//!
//! Two background threads can stop the flow: the time limit, and the end of
//! stdin when airlock stops. The program writes only one answer.
//!
//! After an allow answer, the request can ask for a running notice. Then
//! the notification changes to a quiet notice with an "Abort" button. The
//! notice stays until airlock closes stdin. When the user selects abort,
//! the program writes one abort event line and stops.
//!
//! The request can give details. Then the notification has one more button
//! that opens a window with the details, and a click on the notification
//! does the same. The window has "Allow" and "Deny" buttons that give the
//! same answers as the notification. A close of the window is a dismiss.
//! The window shows the details as plain text only. The window closes when
//! the answer is written, also at the end of the time limit.
//!
//! The answer also tells the alert style of the app. When the style is
//! none, the notification does not show on the screen, so the program does
//! not post it and gives an error answer. When the user did not decide on
//! the permission yet, the system shows the permission prompt only when the
//! app starts through the system app launcher. A direct start then
//! gives an error answer that tells airlock to start the app that way.
//!
//! The program writes short diagnostics to stderr.

use std::io::{BufRead, BufReader, Read, Stdin, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::FileTypeExt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions,
    NSBackgroundColorAttributeName, NSBackingStoreType, NSButton, NSColor, NSEventMask,
    NSFloatingWindowLevel, NSFont, NSFontWeightRegular, NSScreen, NSTextView, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray, NSBundle, NSDate, NSDefaultRunLoopMode, NSError, NSNotification, NSPoint, NSRange,
    NSRect, NSSet, NSSize, NSString,
};
use objc2_user_notifications::{
    UNAlertStyle, UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent,
    UNNotification, UNNotificationAction, UNNotificationActionOptions, UNNotificationCategory,
    UNNotificationCategoryOptions, UNNotificationDefaultActionIdentifier,
    UNNotificationDismissActionIdentifier, UNNotificationInterruptionLevel,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSettings, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use crate::protocol::{
    AlertStyle, ChildAnswer, ChildDecision, ChildEvent, ChildRequest, Details, DiffKind,
    MAX_REQUEST, RunningNotice, parse_request,
};

/// Category of the grant notification. It has the two buttons.
const CATEGORY: &str = "airlock-grant";
/// Action of the "Allow" button.
const ACTION_ALLOW: &str = "allow";
/// Action of the "Deny" button.
const ACTION_DENY: &str = "deny";
/// Action of the button that opens the details window.
const ACTION_DETAILS: &str = "details";
/// Category of the running notice. It has the abort button.
const CATEGORY_RUNNING: &str = "airlock-running";
/// Action of the "Abort" button.
const ACTION_ABORT: &str = "abort";
/// Length of one event loop step, in seconds.
const STEP: f64 = 0.1;
/// Time for the main thread to stop after the time limit.
const GRACE: Duration = Duration::from_secs(2);

/// Set when the answer is written.
static ANSWERED: AtomicBool = AtomicBool::new(false);
/// Set when the answer is an error.
static FAILED: AtomicBool = AtomicBool::new(false);
/// Set when the answer is allow.
static ALLOWED: AtomicBool = AtomicBool::new(false);
/// Set when the running notice shows. Then the time limit does not apply.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Set when the user selects abort on the running notice.
static ABORTED: AtomicBool = AtomicBool::new(false);
/// Set when the request has details.
static HAS_DETAILS: AtomicBool = AtomicBool::new(false);
/// Set when the user asks for the details window. The main thread opens it.
static SHOW_DETAILS: AtomicBool = AtomicBool::new(false);
/// Set when stdin ends.
static STDIN_CLOSED: AtomicBool = AtomicBool::new(false);
/// Identifier of the notification, after it is posted.
static REQUEST_ID: OnceLock<String> = OnceLock::new();
/// Alert style of the app, after the settings are read.
static ALERT_STYLE: Mutex<Option<AlertStyle>> = Mutex::new(None);

/// Show the notification and write the answer of the user. Does not return.
pub(crate) fn run() {
    let Some(mtm) = MainThreadMarker::new() else {
        fail("not on the main thread");
    };
    let mut input = BufReader::new(std::io::stdin());
    let req = read_request(&mut input).unwrap_or_else(|e| fail(&e));
    start_time_limit(Duration::from_millis(req.timeout_ms));
    // Only airlock closes a pipe, so only then the running notice can go
    // away.
    let watched = stdin_is_pipe();
    if watched {
        watch_stdin(input);
    }
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        fail("not in an app bundle");
    }

    // Set the delegate before the app finishes its launch. Then the system
    // can give a choice that started this app to the delegate.
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let delegate = Delegate::new();
    center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    let pump = Pump::new(mtm);

    let status = read_settings(&center, &pump);
    if let Err(e) = authorize(&center, &pump) {
        let detail = format!("notifications are not allowed for Airlock: {e}");
        // Without a decision of the user, only a start through the app
        // launcher shows the permission prompt.
        pump.fail(&detail, status == UNAuthorizationStatus::NotDetermined);
    }
    // The user can change the style in the permission prompt.
    if status == UNAuthorizationStatus::NotDetermined {
        read_settings(&center, &pump);
    }
    // With no alert style the notification goes only to Notification
    // Center, and the user does not see it in time. Airlock then asks with
    // a dialog.
    if *lock(&ALERT_STYLE) == Some(AlertStyle::None) {
        pump.fail("notifications of Airlock do not show on the screen", false);
    }
    let abort = req.running.as_ref().map_or("Abort", |n| n.abort.as_str());
    let more = req.details.as_ref().map(|d| d.more.as_str());
    HAS_DETAILS.store(more.is_some(), Ordering::SeqCst);
    register_categories(&center, &pump, abort, more);
    if let Err(e) = post(&center, &pump, &req) {
        pump.fail(&format!("could not show the notification: {e}"), false);
    }
    let mut window = None;
    while !ANSWERED.load(Ordering::SeqCst) {
        if let Some(details) = &req.details
            && window.is_none()
            && SHOW_DETAILS.load(Ordering::SeqCst)
        {
            remove_notification();
            window = Some(DetailsWindow::open(mtm, details));
        }
        pump.step();
    }
    // The answer can come from the window, the notification or the time
    // limit. Close the window before the running notice or the stop.
    if let Some(window) = window {
        window.close();
    }
    let Some(notice) = req.running.as_ref().filter(|_| watched) else {
        pump.finish();
    };
    if !ALLOWED.load(Ordering::SeqCst) || STDIN_CLOSED.load(Ordering::SeqCst) {
        pump.finish();
    }
    // Set before the time limit thread checks it. That thread waits a
    // grace time after the answer.
    RUNNING.store(true, Ordering::SeqCst);
    if let Err(e) = post_running(&center, &pump, notice) {
        // Airlock sees the end of the output as no abort.
        log(&format!("could not show the running notice: {e}"));
        pump.finish();
    }
    pump.until(|| STDIN_CLOSED.load(Ordering::SeqCst) || ABORTED.load(Ordering::SeqCst));
    if ABORTED.load(Ordering::SeqCst) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", ChildEvent::Abort.encode());
        let _ = out.flush();
    }
    pump.finish();
}

/// Read and parse the request line from stdin.
fn read_request(input: &mut BufReader<Stdin>) -> Result<ChildRequest, String> {
    let mut line = String::new();
    input
        .by_ref()
        .take(MAX_REQUEST + 1)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if line.is_empty() {
        return Err("no request on stdin".into());
    }
    if line.len() as u64 > MAX_REQUEST {
        return Err("request is too long".into());
    }
    parse_request(&line)
}

/// Write the answer once. Later calls do nothing.
///
/// Returns:
///   True if this call wrote the answer.
fn answer(decision: ChildDecision, detail: &str) -> bool {
    send(decision, detail, false)
}

/// Write the answer once, with the alert style if it is known.
/// Args:
///  - `decision`: Answer kind
///  - `detail`: Error text for `Error`
///  - `needs_open`: True if airlock must start the app through the app
///    launcher, so that the system can ask for the permission
///
/// Returns:
///   True if this call wrote the answer.
fn send(decision: ChildDecision, detail: &str, needs_open: bool) -> bool {
    if ANSWERED.swap(true, Ordering::SeqCst) {
        return false;
    }
    if decision == ChildDecision::Error {
        FAILED.store(true, Ordering::SeqCst);
    }
    if decision == ChildDecision::Allow {
        ALLOWED.store(true, Ordering::SeqCst);
    }
    let line = ChildAnswer {
        alert_style: *lock(&ALERT_STYLE),
        needs_open,
        ..ChildAnswer::new(decision, detail)
    }
    .encode();
    let mut out = std::io::stdout().lock();
    // Airlock can be gone. Then nobody reads the answer.
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    true
}

/// Write the error answer and stop. Use only before the event loop starts.
fn fail(detail: &str) -> ! {
    log(detail);
    answer(ChildDecision::Error, detail);
    std::process::exit(1);
}

/// Write a diagnostic line to stderr.
fn log(text: &str) {
    let _ = writeln!(std::io::stderr(), "airlock-macos-notifier: {text}");
}

/// Lock the mutex. A panic in another thread does not stop the flow.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Start the thread that writes the timeout answer at the end of the time
/// limit.
fn start_time_limit(limit: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(limit);
        if answer(ChildDecision::Timeout, "") {
            log("time limit ended");
        }
        // The main thread removes the notification and stops. Stop here if
        // it does not do that in time. The running notice has no time
        // limit: it stays until airlock closes stdin.
        std::thread::sleep(GRACE);
        if RUNNING.load(Ordering::SeqCst) {
            return;
        }
        std::process::exit(i32::from(FAILED.load(Ordering::SeqCst)));
    });
}

/// Check if stdin is a pipe. Airlock keeps the pipe open while it waits, so
/// the end of the pipe tells that airlock stopped. A file on stdin (manual
/// tests) ends at once, so do not watch it.
fn stdin_is_pipe() -> bool {
    std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map(std::fs::File::from)
        .and_then(|f| f.metadata())
        .is_ok_and(|m| m.file_type().is_fifo())
}

/// Start the thread that stops the flow at the end of stdin.
fn watch_stdin(mut input: BufReader<Stdin>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        loop {
            match input.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        STDIN_CLOSED.store(true, Ordering::SeqCst);
        if answer(ChildDecision::Error, "stdin closed") {
            log("stdin closed");
        }
    });
}

/// Get the text of an error from the system.
fn error_text(error: *mut NSError) -> String {
    // SAFETY: The system gives null or a valid error for the time of the
    // call.
    unsafe { error.as_ref() }.map_or_else(
        || "unknown error".to_owned(),
        |e| e.localizedDescription().to_string(),
    )
}

/// Application event loop of the main thread. The app is an accessory app:
/// it has no Dock icon and no menu bar.
struct Pump(Retained<NSApplication>);

impl Pump {
    /// Start the application and make the event loop.
    fn new(mtm: MainThreadMarker) -> Self {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();
        Self(app)
    }

    /// Run the event loop for one step. The step also runs the blocks that
    /// the system puts on the main queue.
    fn step(&self) {
        let until = NSDate::dateWithTimeIntervalSinceNow(STEP);
        // SAFETY: Foundation sets this constant before main starts.
        let mode = unsafe { NSDefaultRunLoopMode };
        let event = self.0.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&until),
            mode,
            true,
        );
        if let Some(event) = event {
            self.0.sendEvent(&event);
        }
    }

    /// Run the event loop until a value is in the slot.
    /// Args:
    ///  - `slot`: Place where a system reply puts its value
    ///
    /// Returns:
    ///   The value. If the answer is written first, removes the
    ///   notification and stops the program.
    fn wait<T>(&self, slot: &Mutex<Option<T>>) -> T {
        loop {
            if let Some(value) = lock(slot).take() {
                return value;
            }
            if ANSWERED.load(Ordering::SeqCst) {
                self.finish();
            }
            self.step();
        }
    }

    /// Run the event loop until the condition is true.
    fn until(&self, done: impl Fn() -> bool) {
        while !done() {
            self.step();
        }
    }

    /// Write the error answer, remove the notification and stop.
    /// Args:
    ///  - `detail`: Error text
    ///  - `needs_open`: True if airlock must start the app through the app
    ///    launcher
    fn fail(&self, detail: &str, needs_open: bool) -> ! {
        log(detail);
        send(ChildDecision::Error, detail, needs_open);
        self.finish();
    }

    /// Remove the notification and stop.
    fn finish(&self) -> ! {
        if remove_notification() {
            // The removal is asynchronous. Give it time to go out before
            // the program stops.
            for _ in 0..3 {
                self.step();
            }
        }
        std::process::exit(i32::from(FAILED.load(Ordering::SeqCst)));
    }
}

/// Remove the notification from the screen and from Notification Center.
///
/// Returns:
///   True if the notification was posted.
fn remove_notification() -> bool {
    let Some(id) = REQUEST_ID.get() else {
        return false;
    };
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let ids = NSArray::from_retained_slice(&[NSString::from_str(id)]);
    center.removeDeliveredNotificationsWithIdentifiers(&ids);
    center.removePendingNotificationRequestsWithIdentifiers(&ids);
    true
}

/// Read the notification settings of the app. Keeps the alert style for
/// the answer.
///
/// Returns:
///   The authorization status of the app.
fn read_settings(center: &UNUserNotificationCenter, pump: &Pump) -> UNAuthorizationStatus {
    let slot = Arc::new(Mutex::new(None));
    let reply = Arc::clone(&slot);
    let block: RcBlock<dyn Fn(NonNull<UNNotificationSettings>)> =
        RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: The system gives a valid object for the time of the
            // call.
            let settings = unsafe { settings.as_ref() };
            *lock(&reply) = Some((settings.authorizationStatus(), settings.alertStyle()));
        });
    center.getNotificationSettingsWithCompletionHandler(&block);
    let (status, style) = pump.wait(&slot);
    log(&format!(
        "authorization status {}, alert style {}",
        status.0, style.0
    ));
    let style = match style {
        UNAlertStyle::None => Some(AlertStyle::None),
        UNAlertStyle::Banner => Some(AlertStyle::Banner),
        UNAlertStyle::Alert => Some(AlertStyle::Alert),
        _ => None,
    };
    *lock(&ALERT_STYLE) = style;
    status
}

/// Ask for the notification permission. The system shows the prompt only
/// on the first run.
///
/// Returns:
///   Error text if the user or the system did not give the permission.
fn authorize(center: &UNUserNotificationCenter, pump: &Pump) -> Result<(), String> {
    let slot = Arc::new(Mutex::new(None));
    let reply = Arc::clone(&slot);
    let block: RcBlock<dyn Fn(Bool, *mut NSError)> =
        RcBlock::new(move |granted: Bool, error: *mut NSError| {
            let result = if granted.as_bool() {
                Ok(())
            } else {
                Err(error_text(error))
            };
            *lock(&reply) = Some(result);
        });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &block,
    );
    pump.wait(&slot)
}

/// Register the categories: the request with the "Allow" and "Deny"
/// buttons, and the running notice with the abort button.
/// Args:
///  - `abort`: Label of the abort button
///  - `more`: Label of the details button, if the request has details
fn register_categories(
    center: &UNUserNotificationCenter,
    pump: &Pump,
    abort: &str,
    more: Option<&str>,
) {
    let allow = UNNotificationAction::actionWithIdentifier_title_options(
        &NSString::from_str(ACTION_ALLOW),
        &NSString::from_str("Allow"),
        UNNotificationActionOptions::empty(),
    );
    let deny = UNNotificationAction::actionWithIdentifier_title_options(
        &NSString::from_str(ACTION_DENY),
        &NSString::from_str("Deny"),
        UNNotificationActionOptions::Destructive,
    );
    // The details window needs the app in the foreground.
    let details = more.map(|more| {
        UNNotificationAction::actionWithIdentifier_title_options(
            &NSString::from_str(ACTION_DETAILS),
            &NSString::from_str(more),
            UNNotificationActionOptions::Foreground,
        )
    });
    let actions = NSArray::from_retained_slice(
        &[Some(allow), details, Some(deny)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
    );
    let intents = NSArray::<NSString>::new();
    // The custom dismiss option makes the system tell when the user closes
    // the notification.
    let category = UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &NSString::from_str(CATEGORY),
        &actions,
        &intents,
        UNNotificationCategoryOptions::CustomDismissAction,
    );
    let abort = UNNotificationAction::actionWithIdentifier_title_options(
        &NSString::from_str(ACTION_ABORT),
        &NSString::from_str(abort),
        UNNotificationActionOptions::Destructive,
    );
    let running = UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &NSString::from_str(CATEGORY_RUNNING),
        &NSArray::from_retained_slice(&[abort]),
        &intents,
        UNNotificationCategoryOptions::empty(),
    );
    // One call replaces all categories of the app, so register both here.
    center.setNotificationCategories(&NSSet::from_retained_slice(&[category, running]));

    // The registration is asynchronous. A read of the categories waits for
    // it, so the notification does not show without the buttons.
    let slot = Arc::new(Mutex::new(None));
    let reply = Arc::clone(&slot);
    let block: RcBlock<dyn Fn(NonNull<NSSet<UNNotificationCategory>>)> =
        RcBlock::new(move |_: NonNull<NSSet<UNNotificationCategory>>| {
            *lock(&reply) = Some(());
        });
    center.getNotificationCategoriesWithCompletionHandler(&block);
    pump.wait(&slot);
}

/// Post the notification.
///
/// Returns:
///   Error text if the system refused the notification.
fn post(center: &UNUserNotificationCenter, pump: &Pump, req: &ChildRequest) -> Result<(), String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let id = format!("airlock-{}-{nanos}", std::process::id());

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&req.title));
    content.setBody(&NSString::from_str(&req.body));
    content.setCategoryIdentifier(&NSString::from_str(CATEGORY));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    // Time-sensitive needs an entitlement. Active is the highest level
    // without it.
    content.setInterruptionLevel(UNNotificationInterruptionLevel::Active);
    // Set the identifier first: the choice of the user can come before the
    // reply of the add.
    let _ = REQUEST_ID.set(id.clone());
    add(center, pump, &id, &content)
}

/// Post the running notice in place of the request notification. It uses
/// the same identifier, so it replaces the request.
///
/// Returns:
///   Error text if the system refused the notice.
fn post_running(
    center: &UNUserNotificationCenter,
    pump: &Pump,
    notice: &RunningNotice,
) -> Result<(), String> {
    let id = REQUEST_ID.get().ok_or("no notification identifier")?;
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&notice.title));
    content.setBody(&NSString::from_str(&notice.body));
    content.setCategoryIdentifier(&NSString::from_str(CATEGORY_RUNNING));
    // No sound: the notice only tells that the access is active.
    content.setInterruptionLevel(UNNotificationInterruptionLevel::Passive);
    add(center, pump, id, &content)
}

/// Add the notification request and wait for the reply of the system.
///
/// Returns:
///   Error text if the system refused the notification.
fn add(
    center: &UNUserNotificationCenter,
    pump: &Pump,
    id: &str,
    content: &UNMutableNotificationContent,
) -> Result<(), String> {
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(id),
        content,
        None,
    );

    let slot = Arc::new(Mutex::new(None));
    let reply = Arc::clone(&slot);
    let block: RcBlock<dyn Fn(*mut NSError)> = RcBlock::new(move |error: *mut NSError| {
        let result = if error.is_null() {
            Ok(())
        } else {
            Err(error_text(error))
        };
        *lock(&reply) = Some(result);
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&*block));
    // In the running phase the answer is already written. Wait without
    // the stop on the answer.
    if RUNNING.load(Ordering::SeqCst) {
        pump.until(|| lock(&slot).is_some());
        return lock(&slot).take().unwrap_or(Ok(()));
    }
    pump.wait(&slot)
}

/// Get the answer for an action of the notification.
fn decision_for(action: &NSString) -> ChildDecision {
    // SAFETY: The framework sets these constants before main starts.
    let (dismiss, default) = unsafe {
        (
            UNNotificationDismissActionIdentifier,
            UNNotificationDefaultActionIdentifier,
        )
    };
    let action = action.to_string();
    if action == ACTION_ALLOW {
        ChildDecision::Allow
    } else if action == ACTION_DENY {
        ChildDecision::Deny
    } else if action == dismiss.to_string() || action == default.to_string() {
        // A click on the notification body is not a choice.
        ChildDecision::Dismissed
    } else {
        log(&format!("unknown action: {action}"));
        ChildDecision::Dismissed
    }
}

define_class!(
    // SAFETY:
    // - NSObject has no subclass requirements.
    // - The delegate does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "AirlockNotifierDelegate"]
    struct Delegate;

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: The method signatures agree with the protocol.
    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        // The system asks this when the app is in the foreground. Show the
        // notification as a banner also then.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &DynBlock<dyn Fn()>,
        ) {
            let id = response.notification().request().identifier().to_string();
            let action = response.actionIdentifier();
            // The system can give a choice for a notification of an earlier
            // run. Ignore it.
            if REQUEST_ID.get().is_some_and(|own| *own == id) {
                // SAFETY: The framework sets this constant before main starts.
                let default = unsafe { UNNotificationDefaultActionIdentifier };
                if action.to_string() == ACTION_ABORT {
                    if RUNNING.load(Ordering::SeqCst) {
                        ABORTED.store(true, Ordering::SeqCst);
                    }
                } else if !RUNNING.load(Ordering::SeqCst)
                    && (action.to_string() == ACTION_DETAILS
                        || (HAS_DETAILS.load(Ordering::SeqCst) && &*action == default))
                {
                    // Only the main thread can open a window.
                    SHOW_DETAILS.store(true, Ordering::SeqCst);
                } else {
                    answer(decision_for(&action), "");
                }
            }
            completion_handler.call(());
        }
    }
);

impl Delegate {
    /// Make the delegate.
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        // SAFETY: `init` of NSObject has no requirements.
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    // SAFETY:
    // - NSObject has no subclass requirements.
    // - The target does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AirlockNotifierWindowTarget"]
    struct WindowTarget;

    impl WindowTarget {
        #[unsafe(method(allow:))]
        fn allow(&self, _sender: Option<&AnyObject>) {
            answer(ChildDecision::Allow, "");
        }

        #[unsafe(method(deny:))]
        fn deny(&self, _sender: Option<&AnyObject>) {
            answer(ChildDecision::Deny, "");
        }
    }

    // SAFETY: `NSObjectProtocol` has no safety requirements.
    unsafe impl NSObjectProtocol for WindowTarget {}

    // SAFETY: The method signature agrees with the protocol.
    unsafe impl NSWindowDelegate for WindowTarget {
        // A close after the answer does nothing, because only the first
        // answer is written.
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            answer(ChildDecision::Dismissed, "");
        }
    }
);

/// Window that shows the details of the request with "Allow" and "Deny"
/// buttons.
struct DetailsWindow {
    window: Retained<NSWindow>,
    // The window and the buttons do not keep their target, so keep it here.
    _target: Retained<WindowTarget>,
}

impl DetailsWindow {
    /// Open the window in front of other apps.
    fn open(mtm: MainThreadMarker, details: &Details) -> Self {
        const W: f64 = 520.0;
        const H: f64 = 420.0;
        const PAD: f64 = 16.0;
        const BUTTON: NSSize = NSSize::new(96.0, 32.0);
        let rect = |x, y, w, h| NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));

        let target: Retained<WindowTarget> =
            // SAFETY: `init` of NSObject has no requirements.
            unsafe { msg_send![super(mtm.alloc::<WindowTarget>().set_ivars(())), init] };
        // SAFETY: The window rect and style are valid.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect(0.0, 0.0, W, H),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: This struct keeps the window, so a close must not free it.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(&details.title));
        window.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        window.setLevel(NSFloatingWindowLevel);

        let content = NSView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, W, H));
        let top = PAD * 2.0 + BUTTON.height;
        let scroll = NSTextView::scrollableTextView(mtm);
        scroll.setFrame(rect(PAD, top, W - PAD * 2.0, H - top - PAD));
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        if let Some(text) = scroll
            .documentView()
            .and_then(|v| v.downcast::<NSTextView>().ok())
        {
            fill_text(&text, details);
        }
        content.addSubview(&scroll);

        // Return selects deny, the same as in the dialog of airlock.
        let buttons = [
            (
                "Allow",
                sel!(allow:),
                "",
                W - PAD * 2.0 - BUTTON.width * 2.0,
            ),
            ("Deny", sel!(deny:), "\r", W - PAD - BUTTON.width),
        ];
        for (title, action, key, x) in buttons {
            let button = button(mtm, &target, title, action);
            button.setFrame(rect(x, PAD, BUTTON.width, BUTTON.height));
            button.setKeyEquivalent(&NSString::from_str(key));
            content.addSubview(&button);
        }

        window.setContentView(Some(&content));
        place_top_right(mtm, &window);
        // The app is an accessory app. It must come forward, else the window
        // does not get the keyboard focus.
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        window.makeKeyAndOrderFront(None);
        Self {
            window,
            _target: target,
        }
    }

    /// Close the window.
    fn close(self) {
        self.window.setDelegate(None);
        self.window.close();
    }
}

/// Put the window at the top right of the main screen, below the menu bar.
/// Put it at the center if there is no screen.
fn place_top_right(mtm: MainThreadMarker, window: &NSWindow) {
    const MARGIN: f64 = 20.0;
    let Some(screen) = NSScreen::mainScreen(mtm) else {
        window.center();
        return;
    };
    // The visible frame excludes the menu bar and the Dock. The y axis
    // goes up from the bottom of the screen.
    let area = screen.visibleFrame();
    let width = window.frame().size.width;
    window.setFrameTopLeftPoint(NSPoint::new(
        area.origin.x + area.size.width - width - MARGIN,
        area.origin.y + area.size.height - MARGIN,
    ));
}

/// Make a push button that sends the action to the target.
fn button(
    mtm: MainThreadMarker,
    target: &WindowTarget,
    title: &str,
    action: Sel,
) -> Retained<NSButton> {
    // SAFETY: The target has a method for the action.
    unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action),
            mtm,
        )
    }
}

/// Put the details into the text view: each label in bold with its value
/// below it. The lines of a change show with a mark and a color for each
/// change kind.
fn fill_text(text: &NSTextView, details: &Details) {
    let label_font = NSFont::boldSystemFontOfSize(13.0);
    let plain_font = NSFont::systemFontOfSize(13.0);
    // SAFETY: AppKit sets this constant before main starts.
    let mono_font = NSFont::monospacedSystemFontOfSize_weight(12.0, unsafe { NSFontWeightRegular });
    // Semantic colors change with the light and dark appearance.
    let plain = NSColor::textColor();
    let green = NSColor::systemGreenColor();
    let red = NSColor::systemRedColor();
    let green_tint = green.colorWithAlphaComponent(0.12);
    let red_tint = red.colorWithAlphaComponent(0.12);

    let mut all = String::new();
    let mut parts: Vec<(NSRange, &NSFont, &NSColor, Option<&NSColor>)> = Vec::new();
    let mut push = |part: String, font, color, tint| {
        // Text ranges count UTF-16 units.
        let start = all.encode_utf16().count();
        let len = part.encode_utf16().count();
        all.push_str(&part);
        parts.push((NSRange::new(start, len), font, color, tint));
    };
    for item in &details.items {
        push(format!("{}\n", item.label), &*label_font, &*plain, None);
        if let Some(lines) = &item.diff {
            for line in lines {
                let (mark, color, tint) = match line.kind {
                    DiffKind::Add => ("+ ", &*green, Some(&*green_tint)),
                    DiffKind::Remove => ("- ", &*red, Some(&*red_tint)),
                    DiffKind::Same => ("  ", &*plain, None),
                };
                push(format!("{mark}{}\n", line.text), &*mono_font, color, tint);
            }
            push("\n".to_owned(), &*plain_font, &*plain, None);
        } else {
            let font = if item.mono { &*mono_font } else { &*plain_font };
            push(format!("{}\n\n", item.value), font, &*plain, None);
        }
    }
    text.setString(&NSString::from_str(all.trim_end()));
    let total = text.string().length();
    // SAFETY: The text view keeps its text storage while this function runs.
    let storage = unsafe { text.textStorage() };
    for (range, font, color, tint) in parts {
        // The trim makes the last ranges shorter or empty.
        let len = range.length.min(total.saturating_sub(range.location));
        let range = NSRange::new(range.location.min(total), len);
        text.setFont_range(font, range);
        text.setTextColor_range(Some(color), range);
        if let (Some(tint), Some(storage)) = (tint, &storage) {
            // SAFETY: The background attribute takes a color.
            unsafe {
                storage.addAttribute_value_range(NSBackgroundColorAttributeName, tint, range);
            }
        }
    }
    text.setEditable(false);
    text.setSelectable(true);
    text.setTextContainerInset(NSSize::new(6.0, 8.0));
}
