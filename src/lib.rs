//! A safe and convenient wrapper around wl-library-link-sys.
//!
//! # Automatically generating LibraryLink wrappers around Rust functions.
//!
//! See the `generate_wrapper!()` macro.
//!
//! ## Getting backtraces when a panic occurs
//!
//! `generate_wrapper!()` will automatically catch any Rust panic's which occur in the
//! wrapped code, and show an error in the FE with the panic message and source file/line
//! number. It also can optionally show the backtrace. This is configured by the
//! "LIBRARY_LINK_RUST_BACKTRACE" environment variable. Enable it by evaluating:
//!
//! ```wolfram
//! SetEnvironment["LIBRARY_LINK_RUST_BACKTRACE" -> "True"]
//! ```
//!
//! Now the error shown when a panic occurs will include a backtrace.

#![feature(try_trait)]
#![feature(panic_info_message)]
#![feature(proc_macro_hygiene)]

pub mod catch_panic;

use std::ffi::{CStr, CString};
use std::ops::Try;

use wl_expr::{Expr, SymbolTable};
use wl_lang::sym;
use wl_parse;

// TODO: Maybe don't reexport mint? Standardize on using u32/u64/isize? Type aliases
//       aren't always helpful.
pub use wl_library_link_sys::{
    WolframLibraryData, mint, MArgument,
    // Errors
    LIBRARY_NO_ERROR, LIBRARY_FUNCTION_ERROR, LIBRARY_TYPE_ERROR,
};

const BACKTRACE_ENV_VAR: &str = "LIBRARY_LINK_RUST_BACKTRACE";

//======================================
// Engine Interface
//======================================

pub type Engine = Box<dyn EngineInterface>;

pub trait EngineInterface {
    /// Returns `true` if the user has requested that the current evaluation be aborted.
    ///
    /// Programs should finish what they are doing and return control of this thread to
    /// to the kernel as quickly as possible. They should not exit the process or
    /// otherwise terminate execution, simply return up the call stack.
    fn aborted(&self) -> bool;

    // TODO:
    // /// Convenience wrapper around evaluate `Print`.
    // fn print(&self, args: impl Into<PrintArgs>);

    // TODO:
    // /// Evaluate an expression in the current kernel.
    // ///
    // /// TODO: What does Stack[] give in this situation? What does Stack[] give inside any
    // ///       builtin function?
    // fn evaluate(&self, expr: Expr) -> EvaluationData;
}

/// This struct should be considered private.
///
/// It is only public because it appears in the expansion of `generate_wrapper`.
pub struct EngineCallbacks {
    // TODO: Is this function thread safe? Can it be called from a thread other than the
    //       one the LibraryLink wrapper was originally invoked from?
    abortq: Option<unsafe extern "C" fn() -> mint>,
}

impl From<WolframLibraryData> for EngineCallbacks {
    fn from(libdata: WolframLibraryData) -> Self {
        EngineCallbacks {
            // TODO(!): Audit this
            abortq: unsafe { (*libdata) }.AbortQ,
        }
    }
}

impl EngineInterface for EngineCallbacks {
    fn aborted(&self) -> bool {
        let func = match self.abortq {
            Some(func) => func,
            // If the callback is empty, assume no abort has been requested. That this is
            // even an Option is likely just bindgen being conservative with function
            // pointers possibly be null.
            // TODO: Investigate making bindgen treat this as a non-null field?
            None => return false,
        };

        let val: mint = unsafe { func() };
        val == 1
    }
}

// #[library_link::wrap]
// fn my_func(lib: LibraryInterface, arg1: Integer) {

// }

// Because the wrapper is generated by macro, it's not necessary for LibraryLink to have a
// stable ABI?

// pub struct PrintArgs {
//     args: Vec<Expr>,
// }

// impl From<i64> for PrintArgs {

// }

// impl From<Expr> for PrintArgs {

// }

// impl From<&str> for PrintArgs {

// }

// impl From<String> for PrintArgs {

// }

// impl From<Vec<Expr>> for PrintArgs {

// }

// pub struct EvaluationData {
//     pub value: Expr,
//     /// Message that were generated during the evaluation.
//     pub message: Vec<forms::Message>,
// }

//======================================
// LibraryLinkStatus
//======================================

#[derive(Copy, Clone, Debug)]
pub enum LibraryLinkStatus {
    NoError,
    FunctionError,
    TypeError,
}

impl From<LibraryLinkStatus> for u32 {
    fn from(status: LibraryLinkStatus) -> u32 {
        match status {
            LibraryLinkStatus::NoError => LIBRARY_NO_ERROR,
            LibraryLinkStatus::FunctionError => LIBRARY_FUNCTION_ERROR,
            LibraryLinkStatus::TypeError => LIBRARY_TYPE_ERROR,
        }
    }
}

impl Try for LibraryLinkStatus {
    type Ok = ();
    type Error = Self;

    fn into_result(self) -> Result<Self::Ok, Self::Error> {
        match self {
            LibraryLinkStatus::NoError => Ok(()),
            s @ LibraryLinkStatus::FunctionError => Err(s),
            s @LibraryLinkStatus::TypeError => Err(s),
        }
    }

    fn from_error(err: Self) -> Self {
        match err {
            LibraryLinkStatus::NoError =>
                panic!("Try::from_error for LibraryLinkStatus: got NoError"),
            LibraryLinkStatus::FunctionError | LibraryLinkStatus::TypeError => err,
        }
    }

    fn from_ok(_ok: ()) -> Self {
        LibraryLinkStatus::NoError
    }
}

//======================================
// Utilities
//======================================

/// Set `res` to a "UTF8String" which is the printed form of a
/// `Failure[$kind, <| "Message" -> $err |>]`.
pub unsafe fn failure_msg(res: MArgument, kind: &str, msg: String) -> LibraryLinkStatus {
    let failure = failure_expr(kind, msg);
    write_expr(failure, res);
    LibraryLinkStatus::NoError
}

// TODO: Rename `err` to `message`.
pub fn failure_expr(kind: &str, err: String) -> Expr {
    let assoc = {
        let msg_rule = Expr::normal(&*sym::Rule, vec![
            Expr::string("Message"), Expr::string(err)]);
        Expr::normal(&*sym::Association, vec![msg_rule])
    };
    Expr::normal(&*sym::Failure, vec![Expr::string(kind), assoc])
}

pub unsafe fn write_expr(expr: Expr, arg: MArgument) {
    // `Display for Expr` handles escaping any special characters which need it. This
    // string is therefore safe for consumption by ToExpression.
    let string = format!("{}", expr);
    // FIXME: This string is never freed
    let string = CString::new(string).unwrap();
    // FIXME: What happens if LibraryLink tries to free this string (which is
    //        currently leaked)?
    *arg.utf8string = string.into_raw();
}

/// Convert a "UTF8String" argument to `&str`.
pub unsafe fn marg_str(arg: &MArgument) -> Result<&str, LibraryLinkStatus> {
    let string: *const i8 = *arg.utf8string;
    let string = CStr::from_ptr(string);
    let string: &str = match string.to_str() {
        Ok(s) => s,
        Err(_) => return Err(LibraryLinkStatus::TypeError),
    };
    Ok(string)
}

/// Parse an `Expr` from an `MArgument` of type `"UTF8String"`.
///
/// Will return a `Failure["ParseError", _]` if parsing is unsuccesful. This should be
/// extremely rare, however, assuming the function is properly used.
pub fn marg_str_expr(string: &str) -> Result<Expr, Expr> {
    let mut st = SymbolTable::new("Global`", &[] as &[String]);
    match wl_parse::parse(&mut st, string) {
        Ok(expr) => Ok(expr),
        // TODO: Possible to show a message through LibraryLink?
        Err(err) => Err(failure_expr("ParseError", format!("{:?}", err))),
    }
}

//======================================
// Macros
//======================================

// TODO: Expose the LibraryLink printing function to users of this library. This will help
//       with debugging the compiler from the FE significantly.

// #[macro_export]
// macro_rules! link_wrapper {
//     (fn $name:ident($lib_data:ident, $args:ident, $res:ident) -> LibraryLinkStatus $body:block) => {
//         #[no_mangle]
//         #[allow(non_snake_case)]
//         pub unsafe fn $name($lib_data: $crate::WolframLibraryData, arg_count: $crate::mint,
//                             $args: *const $crate::MArgument, $res: $crate::MArgument)
//                          -> u32 {
//             use std::convert::TryFrom;

//             let arg_count = match usize::try_from(arg_count) {
//                 Ok(count) => count,
//                 // NOTE: This will never happen as long as LibraryLink doesn't give us a
//                 //       negative argument count. If that happens, something else has
//                 //       gone seriously wrong, so let's do the least unhelpful thing.
//                 // TODO: Is there a better error we could return here?
//                 Err(_) => return $crate::LIBRARY_FUNCTION_ERROR,
//             };
//             let $args: &[$crate::MArgument] = ::std::slice::from_raw_parts($args, arg_count);
//             let closure = || $body;
//             let status: LibraryLinkStatus = closure();
//             u32::from(status)
//         }
//     }
// }

// TODO: Make this a procedural macro.
//       This has a number of benefits:
//         1) Could ensure the function is `pub` and `#[no_mangle]`
//         2) Wouldn't have to duplicate parameter types in function definition and
//            generate_wrapper!() invokation.
// TODO: Allow any type which implements FromExpr in wrapper parameter lists?
#[macro_export]
macro_rules! generate_wrapper {
    ($wrapper:ident # $func:ident ( $($arg:ident : Expr),* ) -> Expr) => {
        #[no_mangle]
        #[allow(non_snake_case)]
        #[doc="Auto-generated LibraryLink function, generated by \
               [generate_wrapper]"]
        pub unsafe extern "C" fn $wrapper(lib_data: $crate::WolframLibraryData,
                               arg_count: $crate::mint,
                               args: *const $crate::MArgument,
                               res: $crate::MArgument) -> u32 {
            use std::convert::TryFrom;
            use std::panic;
            use wl_expr::ArcExpr;
            use wl_lang::forms::ToPrettyExpr;
            use $crate::{
                marg_str, marg_str_expr, write_expr, LibraryLinkStatus,
                catch_panic::{self, CaughtPanic},
                // Re-exported from wl-library-link-sys
                MArgument,
                LIBRARY_NO_ERROR, LIBRARY_FUNCTION_ERROR,
            };

            let arg_count = match usize::try_from(arg_count) {
                Ok(count) => count,
                // NOTE: This will never happen as long as LibraryLink doesn't give us a
                //       negative argument count. If that happens, something else has
                //       gone seriously wrong, so let's do the least unhelpful thing.
                // TODO: Is there a better error we could return here?
                Err(_) => return LIBRARY_FUNCTION_ERROR,
            };
            let margs: &[MArgument] = ::std::slice::from_raw_parts(args, arg_count);

            // Keep track of how many times $arg repeats, so we known which index in
            // `margs` to access.
            let mut arg_idx = 0;
            $(
                let marg = match margs.get(arg_idx) {
                    Some(marg) => marg,
                    /// This implies that the LibraryFunction wrapper in top-level does
                    /// not have enough arguments.
                    None => return LIBRARY_FUNCTION_ERROR,
                };
                let string = match marg_str(marg) {
                    Ok(s) => s,
                    Err(status) => return u32::from(status),
                };
                let $arg: Expr = match marg_str_expr(string) {
                    Ok(expr) => expr,
                    Err(expr) => {
                        write_expr(expr, res);
                        return LibraryLinkStatus::NoError.into()
                    },
                };

                arg_idx += 1;
            );*

            let func: fn($($arg: Expr),*) -> Expr = $func;

            let arc_expr_wrapper = |$($arg: ArcExpr),*| -> Expr {
                $(
                    let $arg: Expr = $arg.to_rc_expr();
                )*
                func($($arg,)*)
            };

            let res_expr: Result<Expr, CaughtPanic> = {
                $(
                    let $arg: ArcExpr = $arg.to_arc_expr();
                )*

                catch_panic::call_and_catch_panic(panic::AssertUnwindSafe(
                    || arc_expr_wrapper($($arg),*)
                ))
            };
            let res_expr: Expr = match res_expr {
                Ok(res_expr) => res_expr,
                Err(caught_panic) => caught_panic.to_pretty_expr(),
            };

            write_expr(res_expr, res);
            LIBRARY_NO_ERROR
        }
    };

    ($wrapper:ident # $func:ident ( $engine:ident : Engine $(, $arg:ident : Expr)* ) -> Expr) => {
        #[no_mangle]
        #[allow(non_snake_case)]
        #[doc="Auto-generated LibraryLink function, generated by \
               wl_library_link::generate_wrapper!()"]
        pub unsafe extern "C" fn $wrapper(lib_data: $crate::WolframLibraryData,
                               arg_count: $crate::mint,
                               args: *const $crate::MArgument,
                               res: $crate::MArgument) -> u32 {
            use std::convert::TryFrom;
            use std::panic;
            use wl_expr::ArcExpr;
            use wl_lang::forms::ToPrettyExpr;
            use $crate::{
                marg_str, marg_str_expr, write_expr, LibraryLinkStatus,
                catch_panic::{self, CaughtPanic},
                // Re-exported from wl-library-link-sys
                MArgument,
                LIBRARY_NO_ERROR, LIBRARY_FUNCTION_ERROR,
                EngineInterface,
            };

            let arg_count = match usize::try_from(arg_count) {
                Ok(count) => count,
                // NOTE: This will never happen as long as LibraryLink doesn't give us a
                //       negative argument count. If that happens, something else has
                //       gone seriously wrong, so let's do the least unhelpful thing.
                // TODO: Is there a better error we could return here?
                Err(_) => return LIBRARY_FUNCTION_ERROR,
            };
            let margs: &[MArgument] = ::std::slice::from_raw_parts(args, arg_count);

            // Keep track of how many times $arg repeats, so we known which index in
            // `margs` to access.
            let mut arg_idx = 0;
            $(
                let marg = match margs.get(arg_idx) {
                    Some(marg) => marg,
                    /// This implies that the LibraryFunction wrapper in top-level does
                    /// not have enough arguments.
                    None => return LIBRARY_FUNCTION_ERROR,
                };
                let string = match marg_str(marg) {
                    Ok(s) => s,
                    Err(status) => return u32::from(status),
                };
                let $arg: Expr = match marg_str_expr(string) {
                    Ok(expr) => expr,
                    Err(expr) => {
                        write_expr(expr, res);
                        return LibraryLinkStatus::NoError.into()
                    },
                };

                arg_idx += 1;
            )*

            // Contruct the engine
            let engine: $crate::Engine = Box::new($crate::EngineCallbacks::from(lib_data));

            let func: fn($engine: $crate::Engine $(, $arg: Expr)*) -> Expr = $func;

            let arc_expr_wrapper = |$($arg: ArcExpr),*| -> Expr {
                $(
                    let $arg: Expr = $arg.to_rc_expr();
                )*
                func(engine, $($arg,)*)
            };

            let res_expr: Result<Expr, CaughtPanic> = {
                $(
                    let $arg: ArcExpr = $arg.to_arc_expr();
                )*

                catch_panic::call_and_catch_panic(panic::AssertUnwindSafe(
                    || arc_expr_wrapper($($arg),*)
                ))
            };
            let res_expr: Expr = match res_expr {
                Ok(res_expr) => res_expr,
                Err(caught_panic) => caught_panic.to_pretty_expr(),
            };

            write_expr(res_expr, res);
            LIBRARY_NO_ERROR
        }
    };
}