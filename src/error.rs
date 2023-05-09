// https://github.com/dtolnay/thiserror/issues/142

use std::{
    fmt::{self, Debug},
    panic::Location,
};

#[derive(Debug, thiserror::Error)]
pub enum ErrorKind {
    #[error("UnitError")]
    UnitError,
    #[error("StrError")]
    StrError(&'static str),
    #[error("StringError")]
    StringError(String),
    #[error("StdIoError")]
    StdIoError(std::io::Error),
    #[error("FromUtf8Error")]
    FromUtf8Error(std::string::FromUtf8Error),
    // this is more obscure but I think we should keep it because it deals with bad strings, and
    // we don't want that in our generic string errors.
    #[error("FromUtf16Error")]
    FromUtf16Error(std::string::FromUtf16Error),
    #[error("TokioJoinError")]
    TokioJoinError(tokio::task::JoinError),
    //#[error("BorshDeserializeError")]
    //BorshDeserializeError(std::io::Error, Vec<u8>),
    //#[error("RonDeserializeError")]
    //RonDeserializeError(ron::Error, Vec<u8>),
    //#[error("SerdeDeserializeError")]
    //SerdeDeserializeError(serde_json::Error, Vec<u8>),
}

/// Error struct for `super_orchestrator`
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

    /// The same as [add_err] but without pushing location to stack
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
}

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

// Can't be automated by macro_rules because of mod paths and special cases.
// Collisions from multiple ways and lack of specialization are preventing me
// from implementing over stuff like `AsRef<T>`

impl From<()> for ErrorKind {
    fn from(_e: ()) -> Self {
        Self::UnitError
    }
}

impl From<()> for Error {
    #[track_caller]
    fn from(e: ()) -> Self {
        Self::from_kind(e)
    }
}

impl From<&'static str> for ErrorKind {
    fn from(e: &'static str) -> Self {
        Self::StrError(e)
    }
}

impl From<&'static str> for Error {
    #[track_caller]
    fn from(e: &'static str) -> Self {
        Self::from_kind(e)
    }
}

impl From<String> for ErrorKind {
    fn from(e: String) -> Self {
        Self::StringError(e)
    }
}

impl From<String> for Error {
    #[track_caller]
    fn from(e: String) -> Self {
        Self::from_kind(e)
    }
}

impl From<std::io::Error> for ErrorKind {
    fn from(e: std::io::Error) -> Self {
        Self::StdIoError(e)
    }
}

impl From<std::io::Error> for Error {
    #[track_caller]
    fn from(e: std::io::Error) -> Self {
        Self::from_kind(e)
    }
}

impl From<std::string::FromUtf8Error> for ErrorKind {
    fn from(e: std::string::FromUtf8Error) -> Self {
        Self::FromUtf8Error(e)
    }
}

impl From<std::string::FromUtf8Error> for Error {
    #[track_caller]
    fn from(e: std::string::FromUtf8Error) -> Self {
        Self::from_kind(e)
    }
}

impl From<std::string::FromUtf16Error> for ErrorKind {
    fn from(e: std::string::FromUtf16Error) -> Self {
        Self::FromUtf16Error(e)
    }
}

impl From<std::string::FromUtf16Error> for Error {
    #[track_caller]
    fn from(e: std::string::FromUtf16Error) -> Self {
        Self::from_kind(e)
    }
}

impl From<tokio::task::JoinError> for ErrorKind {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::TokioJoinError(e)
    }
}

impl From<tokio::task::JoinError> for Error {
    #[track_caller]
    fn from(e: tokio::task::JoinError) -> Self {
        Self::from_kind(e)
    }
}
