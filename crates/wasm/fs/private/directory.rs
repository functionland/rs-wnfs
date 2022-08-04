//! The bindgen API for PublicDirectory.

use wnfs::{PrivateDirectory as WnfsPrivateDirectory, PrivateNode as WnfsPrivateNode};

//--------------------------------------------------------------------------------------------------
// Type Definitions
//--------------------------------------------------------------------------------------------------

/// A directory in a WNFS public file system.
#[wasm_bindgen]
pub struct PrivateDirectory(pub(crate) Rc<WnfsPrivateDirectory>);

//--------------------------------------------------------------------------------------------------
// Implementations
//--------------------------------------------------------------------------------------------------

#[wasm_bindgen]
impl PublicDirectory {}

//--------------------------------------------------------------------------------------------------
// Utilities
//--------------------------------------------------------------------------------------------------
