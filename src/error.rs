// https://github.com/dtolnay/thiserror/issues/142

use std::panic::Location;

#[derive(Debug, thiserror::Error)]
pub enum ErrorKind {
    #[error("GenericError")]
    GenericError(String),
    #[error("StdIoError")]
    StdIoError(std::io::Error),
    //#[error("BorshDeserializeError")]
    //BorshDeserializeError(std::io::Error, Vec<u8>),
    //#[error("RonDeserializeError")]
    //RonDeserializeError(ron::Error, Vec<u8>),
    //#[error("SerdeDeserializeError")]
    //SerdeDeserializeError(serde_json::Error, Vec<u8>),
}

impl ErrorKind {
    /// Converts all error kinds into a `GenericError`. Clones the string if
    /// `self` is already a `GenericError`, uses `format!("{self:?}")`
    /// otherwise. If `extra` is nonempty, also prefixes the error string with
    /// it.
    pub fn generic_error(&self, extra: &str) -> Self {
        Self::GenericError(if extra.is_empty() {
            if let Self::GenericError(e) = self {
                e.to_owned()
            } else {
                format!("{self:?}")
            }
        } else if let Self::GenericError(e) = self {
            format!("{extra}{e}")
        } else {
            format!("{extra}{self:?}")
        })
    }
}

/// # Note
///
/// Use the `locate` function to regenerate location information when it
/// would be in the wrong place. Only the `From` implementation on `Error` works
/// as expected, `Into` and any closure based manipulations like
/// `.map_err(From::from)` need to have a `locate` call at the end.
#[derive(Debug)]
pub struct Error {
    pub error: ErrorKind,
    pub location: &'static Location<'static>,
}

impl Error {
    /// Converts all error kinds into a `GenericError`. Clones the string if
    /// `self` is already a `GenericError`, uses `format!("{self:?}")`
    /// otherwise. If `extra` is nonempty, also prefixes the error string with
    /// it.
    pub fn generic_error(&self, extra: &str) -> Self {
        Self {
            error: self.error.generic_error(extra),
            location: self.location,
        }
    }

    /// Regenerates the location information, replacing `self.location` with the
    /// location that this function is called at
    #[track_caller]
    pub fn locate(mut self) -> Self {
        self.location = Location::caller();
        self
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// Can't be automated by macro_rules because of mod paths and special cases.
// Collisions from multiple ways and lack of specialization are preventing me
// from implementing over stuff like `AsRef<T>` and from implementing `Into`
// with `#[track_caller]` at the right level

impl From<String> for ErrorKind {
    fn from(e: String) -> Self {
        Self::GenericError(e)
    }
}

impl From<String> for Error {
    #[track_caller]
    fn from(e: String) -> Self {
        Self {
            error: ErrorKind::from(e),
            location: Location::caller(),
        }
    }
}

impl From<&str> for ErrorKind {
    fn from(e: &str) -> Self {
        Self::GenericError(e.to_owned())
    }
}

impl From<&str> for Error {
    #[track_caller]
    fn from(e: &str) -> Self {
        Self {
            error: ErrorKind::from(e),
            location: Location::caller(),
        }
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
        Self {
            error: ErrorKind::from(e),
            location: Location::caller(),
        }
    }
}
