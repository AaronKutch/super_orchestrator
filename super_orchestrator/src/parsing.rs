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
                break;
            }
        }
    }
    value.stack_err_with(|| format!("get_separated_val() -> key \"{key}\" not found"))
}
