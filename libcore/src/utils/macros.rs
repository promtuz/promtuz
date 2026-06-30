/// `Ok(v) => v`, `Err(e) => return e`. Used in the relay loop where the
/// error value itself is the function's return (not a `Result`).
#[macro_export]
macro_rules! ret_err {
    ($expr:expr) => {
        match $expr {
            Ok(val) => val,
            Err(err) => return err,
        }
    };
}
