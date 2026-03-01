#[macro_export]
macro_rules! unwrap_or_ret {
    ($opt:expr) => {
        match $opt {
            Some(val) => val,
            None => return,
        }
    };
    ($expr:expr, $ret:expr) => {
        match $expr {
            Ok(val) => val,
            Err(_) => return $ret,
        }
    };
}

#[macro_export]
macro_rules! try_ret {
    ($expr:expr) => {
        match $expr {
            Ok(val) => val,
            Err(_) => return,
        }
    };
}


#[macro_export]
macro_rules! ret_err {
    ($expr:expr) => {
        match $expr {
            Ok(val) => val,
            Err(err) => return err,
        }
    };
}
