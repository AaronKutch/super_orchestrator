// https://github.com/dtolnay/thiserror/issues/142

use std::{
    fmt::{self, Debug},
    panic::Location,
};

/// In the future we plan on having almost every kind of error here under
/// different feature gates. Please file an issue if you would like to include
/// something.
///
/// The intention with `TimeoutError` is that if it is in the error stack, a
/// timeout occured. When other timeout structs are used, this should be added
/// on.
#[derive(Debug, thiserror::Error)]
pub enum ErrorKind {
    // used for special cases where we need something
    #[error("UnitError")]
    UnitError,
    #[error("TimeoutError")]
    TimeoutError,
    #[error("StrError")]
    StrError(&'static str),
    #[error("StringError")]
    StringError(String),
    #[error("BoxedError")]
    BoxedError(Box<dyn std::error::Error>),
    #[error("TryFromIntError")]
    TryFromIntError(std::num::TryFromIntError),
    #[error("StdIoError")]
    StdIoError(std::io::Error),
    #[error("FromUtf8Error")]
    FromUtf8Error(std::string::FromUtf8Error),
    // this is more obscure but I think we should keep it because it deals with bad strings, and
    // we don't want that in our generic string errors.
    #[error("FromUtf16Error")]
    FromUtf16Error(std::string::FromUtf16Error),
    #[error("ParseIntError")]
    ParseIntError(std::num::ParseIntError),
    #[error("ParseFloatError")]
    ParseFloatError(std::num::ParseFloatError),
    #[error("TokioJoinError")]
    TokioJoinError(tokio::task::JoinError),
    // Borsh effecively uses `std::io::Error`
    #[cfg(feature = "ron_support")]
    #[error("RonError")]
    RonError(ron::error::Error),
    #[cfg(feature = "serde_json_support")]
    #[error("SerdeJsonError")]
    SerdeJsonError(serde_json::Error),
    #[cfg(feature = "ctrlc_support")]
    #[error("CtrlcError")]
    CtrlcError(ctrlc::Error),
}

/// An experimental error struct that has an internal stack for different kinds
/// of errors and a stack for locations. This is a replacement for the bad
/// information you get from backtraces within `tokio` tasks.
///
/// # Note
///
/// Import the `MapAddError` trait and use `.map_add_err` instead of `map_err`
/// or other such functions.
pub struct Error {
    pub error_stack: Vec<ErrorKind>,
    pub location_stack: Vec<&'static Location<'static>>,
}

impl Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // in reverse order of a typical stack, I don't want to have to scroll up to see
        // the more specific errors
        f.write_fmt(format_args!("Error {{ location_stack: [\n"))?;
        for location in self.location_stack.iter().rev() {
            f.write_fmt(format_args!("{location:?},\n"))?;
        }
        f.write_fmt(format_args!("], error_stack: [\n"))?;
        for (i, error) in self.error_stack.iter().enumerate().rev() {
            match error {
                ErrorKind::UnitError => (),
                ErrorKind::StrError(s) => {
                    if i == 0 {
                        f.write_fmt(format_args!("{s}\n"))?;
                    } else {
                        f.write_fmt(format_args!("{s} ->\n"))?;
                    }
                }
                ErrorKind::StringError(s) => {
                    if i == 0 {
                        f.write_fmt(format_args!("{s}\n"))?;
                    } else {
                        f.write_fmt(format_args!("{s} ->\n"))?;
                    }
                }
                _ => {
                    f.write_fmt(format_args!("{error:?},\n"))?;
                }
            }
        }
        f.write_fmt(format_args!("] }}"))
    }
}

impl Error {
    #[track_caller]
    fn from_kind<K: Into<ErrorKind>>(kind: K) -> Self {
        let l = Location::caller();
        Self {
            error_stack: vec![kind.into()],
            location_stack: vec![l],
        }
    }

    /// Returns a base timeout error
    #[track_caller]
    pub fn timeout() -> Self {
        Self::from_kind(ErrorKind::TimeoutError)
    }

    /// Can handle anything implementing `std::error::Error`. Most often called
    /// like `Err(Error::boxed(Box::new(e)))`.
    #[track_caller]
    pub fn boxed(e: Box<dyn std::error::Error>) -> Self {
        Self::from_kind(ErrorKind::BoxedError(e))
    }

    /// The same as [Error::add_err] but without pushing location to stack
    #[track_caller]
    pub fn add_err_no_location<K: Into<ErrorKind>>(mut self, kind: K) -> Self {
        self.error_stack.push(kind.into());
        self.location_stack.push(Location::caller());
        self
    }

    /// Converts all error kinds into a `GenericError`. Clones the string if
    /// `self` is already a `GenericError`, uses `format!("{self:?}")`
    /// otherwise. If `extra` is nonempty, also prefixes the error string with
    /// it. Adds `track_caller` location to the stack
    #[track_caller]
    pub fn add_err<K: Into<ErrorKind>>(mut self, kind: K) -> Self {
        self.error_stack.push(kind.into());
        self.location_stack.push(Location::caller());
        self
    }

    /// Only adds `track_caller` location to the stack
    #[track_caller]
    pub fn add_location(mut self) -> Self {
        self.location_stack.push(Location::caller());
        self
    }

    /// Returns if a `TimeoutError` is in the error stack
    pub fn is_timeout(&self) -> bool {
        for error in &self.error_stack {
            if matches!(error, ErrorKind::TimeoutError) {
                return true
            }
        }
        false
    }
}

/// The intention of this trait is to convert `Option<T>`s, `Result<T, Error>`s,
/// and `Result<T, impl Into<ErrorKind>>`s into `Result<T, Error>`s with the
/// error having an `ErrorKind` and a location pushed onto its stacks
/// (`map_add_err` should have `#[track_caller]` on it and push on the
/// `Location::caller()`). You can also call `map_add_err` on plain `Error`s and
/// `impl Into<ErrorKind>`s to get a `Result<(), Error>`.
pub trait MapAddError {
    type Output;

    fn map_add_err<K: Into<ErrorKind>, F: FnOnce() -> K>(self, f: F) -> Self::Output;
}

impl<T> MapAddError for core::result::Result<T, Error> {
    type Output = core::result::Result<T, Error>;

    #[track_caller]
    fn map_add_err<K: Into<ErrorKind>, F: FnOnce() -> K>(self, f: F) -> Self::Output {
        match self {
            Ok(o) => Ok(o),
            Err(e) => Err(e.add_err(f())),
        }
    }
}

impl<T> MapAddError for Option<T> {
    type Output = core::result::Result<T, Error>;

    #[track_caller]
    fn map_add_err<K: Into<ErrorKind>, F: FnOnce() -> K>(self, f: F) -> Self::Output {
        match self {
            Some(o) => Ok(o),
            None => Err(Error::from_kind(f())),
        }
    }
}

impl<T, K0: Into<ErrorKind>> MapAddError for core::result::Result<T, K0> {
    type Output = core::result::Result<T, Error>;

    /// Transforms `Result<T, K0>` into `Result<T, Error>` while adding location
    /// information and a second kind of error.
    #[track_caller]
    fn map_add_err<K1: Into<ErrorKind>, F: FnOnce() -> K1>(self, f: F) -> Self::Output {
        match self {
            Ok(o) => Ok(o),
            Err(kind) => Err(Error::from_kind(kind).add_err_no_location(f())),
        }
    }
}

impl MapAddError for Error {
    type Output = core::result::Result<(), Error>;

    #[track_caller]
    fn map_add_err<K: Into<ErrorKind>, F: FnOnce() -> K>(self, f: F) -> Self::Output {
        Err(self.add_err(f()))
    }
}

impl<K0: Into<ErrorKind>> MapAddError for K0 {
    type Output = core::result::Result<(), Error>;

    #[track_caller]
    fn map_add_err<K1: Into<ErrorKind>, F: FnOnce() -> K1>(self, f: F) -> Self::Output {
        Err(Error::from_kind(self).add_err(f()))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

macro_rules! unit_x {
    ($kind:ident $x:ty) => {
        impl From<$x> for ErrorKind {
            fn from(_e: $x) -> Self {
                Self::$kind
            }
        }

        impl From<$x> for Error {
            #[track_caller]
            fn from(e: $x) -> Self {
                Self::from_kind(e)
            }
        }
    };
}

macro_rules! x {
    ($kind:ident $x:ty) => {
        impl From<$x> for ErrorKind {
            fn from(e: $x) -> Self {
                Self::$kind(e)
            }
        }

        impl From<$x> for Error {
            #[track_caller]
            fn from(e: $x) -> Self {
                Self::from_kind(e)
            }
        }
    };
}

type X0 = ();
unit_x!(UnitError X0);
type X1 = &'static str;
x!(StrError X1);
type X2 = String;
x!(StringError X2);
type X3 = std::io::Error;
x!(StdIoError X3);
type X4 = std::string::FromUtf8Error;
x!(FromUtf8Error X4);
type X5 = std::string::FromUtf16Error;
x!(FromUtf16Error X5);
type X6 = tokio::task::JoinError;
x!(TokioJoinError X6);
#[cfg(feature = "serde_json_support")]
type X7 = serde_json::Error;
#[cfg(feature = "serde_json_support")]
x!(SerdeJsonError X7);
#[cfg(feature = "ron_support")]
type X8 = ron::error::Error;
#[cfg(feature = "ron_support")]
x!(RonError X8);
#[cfg(feature = "ctrlc_support")]
type X9 = ctrlc::Error;
#[cfg(feature = "ctrlc_support")]
x!(CtrlcError X9);
type X10 = std::num::ParseIntError;
x!(ParseIntError X10);
type X11 = std::num::ParseFloatError;
x!(ParseFloatError X11);
type X12 = std::num::TryFromIntError;
x!(TryFromIntError X12);
type X13 = Box<dyn std::error::Error>;
x!(BoxedError X13);

/*
type X = ;
x!(Error X);
*/
