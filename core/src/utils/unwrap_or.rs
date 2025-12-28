#[macro_export]
macro_rules! unwrap_or_ret {
    ($expr:expr) => {
        match $expr {
            Ok(val) => val,
            Err(_) => return,
        }
    };
    ($expr:expr, $ret:expr) => {
        match $expr {
            Ok(val) => val,
            Err(_) => return $ret,
        }
    };
}
