use stacked_errors::{Result, StackableErr};

/// First, this splits by `separate`, trims outer whitespace, sees if `key` is
/// prefixed, if so it also strips `inter_key_val` and returns the stripped and
/// trimmed value.
///
///```
/// use super_orchestrator::get_separated_val;
///
/// let s = r#"
///     address: 0x2b4e4d79e3e9dBBB170CCD78419520d1DCBb4B3f
///     public: 0x04b141241511b1
///     private := "hello world"
/// "#;
/// assert_eq!(
///     &get_separated_val(s, "\n", "address", ":").unwrap(),
///     "0x2b4e4d79e3e9dBBB170CCD78419520d1DCBb4B3f"
/// );
/// assert_eq!(
///     &get_separated_val(s, "\n", "public", ":").unwrap(),
///     "0x04b141241511b1"
/// );
/// assert_eq!(
///     &get_separated_val(s, "\n", "private", ":=").unwrap(),
///     "\"hello world\""
/// );
/// ```
pub fn get_separated_val(
    input: &str,
    separate: &str,
    key: &str,
    inter_key_val: &str,
) -> Result<String> {
    let mut value = None;
    for line in input.split(separate) {
        if let Some(x) = line.trim().strip_prefix(key) {
            if let Some(y) = x.trim().strip_prefix(inter_key_val) {
                value = Some(y.trim().to_owned());
                break
            }
        }
    }
    value.stack_err(|| format!("get_separated_val() -> key \"{key}\" not found"))
}

/// Applies `get` and `stack_err(...)?` in a chain
///
/// ```
/// use serde_json::Value;
/// use super_orchestrator::{
///     stacked_errors::{ensure_eq, Result, StackableErr},
///     stacked_get,
/// };
///
/// let s = r#"{
///     "Id": "id example",
///     "Created": 2023,
///     "Args": [
///         "--entry-name",
///         "--uuid"
///     ],
///     "State": {
///         "Status": "running",
///         "Running": true
///     }
/// }"#;
///
/// fn ex0(s: &str) -> Result<()> {
///     let value: Value = serde_json::from_str(s).stack()?;
///
///     // the normal `Index`ing of `Values` panics, this
///     // returns a formatted error
///     ensure_eq!(stacked_get!(value["Id"]), "id example");
///     ensure_eq!(stacked_get!(value["Created"]), 2023);
///     ensure_eq!(stacked_get!(value["Args"][1]), "--uuid");
///     ensure_eq!(stacked_get!(value["State"]["Status"]), "running");
///     ensure_eq!(stacked_get!(value["State"]["Running"]), true);
///
///     Ok(())
/// }
///
/// ex0(s).unwrap();
///
/// fn ex1(s: &str) -> Result<()> {
///     let value: Value = serde_json::from_str(s).stack()?;
///
///     let _ = stacked_get!(value["State"]["nonexistent"]);
///
///     Ok(())
/// }
///
/// assert!(ex1(s).is_err());
/// ```
#[macro_export]
macro_rules! stacked_get {
    ($value:ident $([$inx:expr])*) => {{
        let mut tmp = &$value;
        $(
            tmp = $crate::stacked_errors::StackableErr::stack_err(
                tmp.get($inx),
                || format!(
                    "stacked_get({} ... [{:?}] ...) -> indexing failed",
                    $crate::stacked_errors::__private::stringify!($value),
                    $inx
                )
            )?;
        )+
        tmp
    }};
}

/// Applies `get_mut` and `stack_err(...)?` in a chain
///
/// ```
/// use serde_json::Value;
/// use super_orchestrator::{
///     stacked_errors::{ensure_eq, Result, StackableErr},
///     stacked_get, stacked_get_mut,
/// };
///
/// let s = r#"{
///     "Id": "id example",
///     "Created": 2023,
///     "Args": [
///         "--entry-name",
///         "--uuid"
///     ],
///     "State": {
///         "Status": "running",
///         "Running": true
///     }
/// }"#;
///
/// fn ex0(s: &str) -> Result<()> {
///     let mut value: Value = serde_json::from_str(s).stack()?;
///
///     *stacked_get_mut!(value["Id"]) = "other".into();
///     *stacked_get_mut!(value["Created"]) = 0.into();
///     *stacked_get_mut!(value["Args"][1]) = "--other".into();
///     *stacked_get_mut!(value["State"]["Status"]) = "stopped".into();
///     *stacked_get_mut!(value["State"]["Running"]) = false.into();
///
///     ensure_eq!(stacked_get!(value["Id"]), "other");
///     ensure_eq!(stacked_get!(value["Created"]), 0);
///     ensure_eq!(stacked_get!(value["Args"][1]), "--other");
///     ensure_eq!(stacked_get!(value["State"]["Status"]), "stopped");
///     ensure_eq!(stacked_get!(value["State"]["Running"]), false);
///
///     Ok(())
/// }
///
/// ex0(s).unwrap();
///
/// fn ex1(s: &str) -> Result<()> {
///     let mut value: Value = serde_json::from_str(s).stack()?;
///
///     let _ = stacked_get_mut!(value["State"]["nonexistent"]);
///
///     Ok(())
/// }
///
/// assert!(ex1(s).is_err());
/// ```
#[macro_export]
macro_rules! stacked_get_mut {
    ($value:ident $([$inx:expr])*) => {{
        let mut tmp = &mut $value;
        $(
            tmp = $crate::stacked_errors::StackableErr::stack_err(
                tmp.get_mut($inx),
                || format!(
                    "stacked_get_mut({} ... [{:?}] ...) -> indexing failed",
                    $crate::stacked_errors::__private::stringify!($value),
                    $inx
                )
            )?;
        )+
        tmp
    }};
}
