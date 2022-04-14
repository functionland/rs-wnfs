mod common;
pub mod public;

pub use common::*;
pub use utils::*;

//--------------------------------------------------------------------------------------------------
// Re-exports
//--------------------------------------------------------------------------------------------------

pub use libipld::{
    cbor::DagCborCodec,
    codec::Codec,
    codec::{Decode, Encode},
    Cid, IpldCodec,
};

//--------------------------------------------------------------------------------------------------
// Utils
//--------------------------------------------------------------------------------------------------

mod utils {
    use std::{cell::RefCell, rc::Rc};

    pub type Shared<T> = Rc<RefCell<T>>;

    pub fn shared<T>(t: T) -> Shared<T> {
        Rc::new(RefCell::new(t))
    }
}
