use std::panic::{self, UnwindSafe};
use std::sync::{self, Mutex};
use std::collections::HashMap;
use std::thread::{self, ThreadId};
use std::process;
use std::time::Instant;

use lazy_static::lazy_static;

use wl_expr::Expr;
use wl_lang::forms::{ToExpr, ToPrettyExpr};
use wl_expr_proc_macro::wlexpr;

lazy_static! {
    static ref CAUGHT_PANICS: Mutex<HashMap<ThreadId, (Instant, CaughtPanic)>> = {
        Mutex::new(HashMap::new())
    };
}

#[derive(Clone)]
pub struct CaughtPanic {
    /// Note: In certain circumstances, this message will NOT match the message used
    ///       in panic!(). This can happen when user code changes the panic hook, or when
    ///       the panic occurs in a different thread from the one `call_and_catch_panic()`
    ///       was called in.
    ///
    ///       An inaccurate instance of `CaughtPanic` can also be reported when panic's
    ///       occur in multiple threads at once.
    pub message: Option<String>,
    pub location: Option<String>,
}

impl ToPrettyExpr for CaughtPanic {
    fn to_pretty_expr(&self) -> Expr {
        let CaughtPanic { message, location } = self.clone();

        let message = Expr::string(message.unwrap_or("Rust panic (no message)".into()));
        let location = Expr::string(location.unwrap_or("Unknown".into()));

        wlexpr! {
            Failure["Panic", <|
                "Message" -> 'message,
                "SourceLocation" -> 'location
            |>]
        }
    }
}

/// Call `func` and catch any unwinding panic which occurs during that call, returning
/// information from the caught panic in the form of a `CaughtPanic`.
///
/// NOTE: `func` should not set it's own panic hook, or unset the panic hook set upon
///       calling it. Doing so would likely interfere with the operation of this function.
///
/// NOTE: If `func` contains any `Expr` arguments it will not work (Expr cannot be sent
///       between threads). Change to `ArcExpr` and it will work.
pub unsafe fn call_and_catch_panic<T, F>(func: F) -> Result<T, CaughtPanic>
        where F: FnOnce() -> T + UnwindSafe {
    // Set up the panic hook. If calling `func` triggers a panic, the panic message string
    // and location will be saved into CAUGHT_PANICS.
    //
    // The panic hook is reset to the default handler before we return.
    let prev_hook = panic::take_hook();
    let _: () = panic::set_hook(Box::new(custom_hook));

    // Call `func`, catching any panic's which occur. The `Err` produced by `catch_unwind`
    // is an opaque object we can't get any information from; this is why it's necessary
    // to set the panic hook, which *does* get an inspectable object.
    let result: Result<T, ()> = panic::catch_unwind(|| func()).map_err(|_| ());

    // Return to the previously set hook (will be the default hook if no previous hook was
    // set).
    panic::set_hook(prev_hook);

    // If `result` is an `Err`, meaning a panic occured, read information out of
    // CAUGHT_PANICS.
    let result: Result<T, CaughtPanic> = result.map_err(|()| get_caught_panic());

    result
}

fn get_caught_panic() -> CaughtPanic {
    let id = thread::current().id();
    let mut map = acquire_lock();
    // Remove the `CaughtPanic` which should be associated with `id` now.
    let caught_panic = match map.remove(&id) {
        Some((_time, caught_panic)) => caught_panic.clone(),
        None => {
            match map.len() {
                0 => {
                    // This can occur when the user code sets their own panic hook, but
                    // fails to restore the previous panic hook (i.e., the `custom_hook`
                    // we set above).
                    let message = format!("could not get panic info for current thread. \
                        Operation of custom panic hook was interrupted");
                    CaughtPanic { message: Some(message), location: None }
                },
                // This case can occur when a panic occurs in a thread spawned by the
                // current thread: the ThreadId stored in CAUGHT_PANICS's is not
                // the ThreadId of the current thread, but the panic still
                // "bubbled up" accross thread boundries to the catch_unwind() call
                // above.
                //
                // We simply guess that the only panic in the HashMap is the right one --
                // it's rare that multiple panic's will occur in multiple threads at the
                // same time (meaning there's more than one entry in the map).
                1 => map.values().next().unwrap().1.clone(),
                // Pick the most recent panic, and hope it's the right one.
                _ => map.values()
                    .max_by(|a, b| a.0.cmp(&b.0))
                    .map(|(_time, info)| info)
                    .cloned()
                    .unwrap()
            }
        },
    };
    caught_panic
}

fn custom_hook(info: &panic::PanicInfo) {
    let caught_panic = {
        let message = info.payload().downcast_ref::<&str>();
        let message: Option<String> = if let Some(string) = message {
            Some(string.to_string())
        } else if let Some(fmt_arguments) = info.message() {
            Some(format!("{}", fmt_arguments))
        } else {
            None
        };
        let location: Option<String> = info.location().map(ToString::to_string);
        CaughtPanic { message, location }
    };

    // The `ThreadId` of the thread which is currently panic'ing.
    let thread = thread::current();
    let data = (Instant::now(), caught_panic);

    let mut lock = acquire_lock();

    if let Some(_previous) = lock.insert(thread.id(), data) {
        // This situation is unlikely, but it can happen.
        //
        // This panic hook is used for every panic which occurs while it is set. This
        // includes panic's which are caught before reaching the `panic::catch_unwind()`,
        // above in `call_and_catch_panic()`, which happens when the user code also uses
        // `panic::catch_unwind()`. When that occurs, this hook (assuming the user hasn't
        // also set their own panic hook) will create an entry in CAUGHT_PANICS's. That
        // entry is never cleared, because the panic is caught before reaching the call to
        // `remove()` in `call_and_catch_panic()`.
    }
}

/// Attempt to acquire a lock on CAUGHT_PANIC. Exit the current process if we can not,
/// without panic'ing.
fn acquire_lock() -> sync::MutexGuard<'static, HashMap<ThreadId, (Instant, CaughtPanic)>> {
    let lock = match CAUGHT_PANICS.lock() {
        Ok(lock) => lock,
        Err(_err) => {
            println!("catch_panic: acquire_lock: failed to acquire lock. Exiting process.");
            process::exit(-1);
        },
    };
    lock
}