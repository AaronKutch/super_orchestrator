// https://github.com/dtolnay/thiserror/issues/142

use std::panic::Location;

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
#[derive(Debug)]
pub struct Error {
    pub error_stack: Vec<ErrorKind>,
    pub location_stack: Vec<&'static Location<'static>>,
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

    /// The same as [add_error] but without pushing location to stack
    #[track_caller]
    pub fn add_error_no_location<K: Into<ErrorKind>>(mut self, kind: K) -> Self {
        self.error_stack.push(kind.into());
        self.location_stack.push(Location::caller());
        self
    }

    /// Converts all error kinds into a `GenericError`. Clones the string if
    /// `self` is already a `GenericError`, uses `format!("{self:?}")`
    /// otherwise. If `extra` is nonempty, also prefixes the error string with
    /// it. Adds `track_caller` location to the stack
    #[track_caller]
    pub fn add_error<K: Into<ErrorKind>>(mut self, kind: K) -> Self {
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

    fn map_add_err<K: Into<ErrorKind>>(self, kind: K) -> Self::Output;
}

impl<T> MapAddError for core::result::Result<T, Error> {
    type Output = core::result::Result<T, Error>;

    #[track_caller]
    fn map_add_err<K: Into<ErrorKind>>(self, kind: K) -> Self::Output {
        match self {
            Ok(o) => Ok(o),
            Err(e) => Err(e.add_error(kind)),
        }
    }
}

impl<T> MapAddError for Option<T> {
    type Output = core::result::Result<T, Error>;

    #[track_caller]
    fn map_add_err<K: Into<ErrorKind>>(self, kind: K) -> Self::Output {
        match self {
            Some(o) => Ok(o),
            None => Err(Error::from_kind(kind)),
        }
    }
}

impl<T, K0: Into<ErrorKind>> MapAddError for core::result::Result<T, K0> {
    type Output = core::result::Result<T, Error>;

    /// Transforms `Result<T, K0>` into `Result<T, Error>` while adding location
    /// information and a second kind of error. The `second_kind` can just
    /// be a unit struct if it is not needed
    #[track_caller]
    fn map_add_err<K1: Into<ErrorKind>>(self, second_kind: K1) -> Self::Output {
        match self {
            Ok(o) => Ok(o),
            Err(kind) => Err(Error::from_kind(kind).add_error_no_location(second_kind)),
        }
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
