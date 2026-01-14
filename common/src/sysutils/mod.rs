mod system_load;

pub trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&Self)) -> Self {
        f(&self);
        self
    }
}
impl<T> Tap for T {}

pub use system_load::{SystemLoad, system_load};
