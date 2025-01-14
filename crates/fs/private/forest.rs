use std::rc::Rc;

use anyhow::Result;
use libipld::Cid;
use log::debug;
use rand_core::RngCore;

use crate::{BlockStore, HashOutput};

use super::{hamt::Hamt, namefilter::Namefilter, Key, PrivateNode, PrivateRef};

//--------------------------------------------------------------------------------------------------
// Type Definitions
//--------------------------------------------------------------------------------------------------

/// PrivateForest is a HAMT that stores CIDs of encrypted private nodes keyed by saturated namefilters.
///
/// On insert, nodes are serialized to DAG CBOR and encrypted with their private refs and then stored in
/// an accompanying block store. And on lookup, the nodes are decrypted and deserialized with the same private
/// refs.
///
/// It is called a forest because it is a collection of file trees.
///
/// # Examples
///
/// ```
/// use wnfs::private::PrivateForest;
///
/// let forest = PrivateForest::new();
///
/// println!("{:?}", forest);
/// ```
// TODO(appcypher): Change Cid to PrivateLink<PrivateNode> to BTreeSet<PrivateLink<PrivateNode>>.
pub type PrivateForest = Hamt<Namefilter, Cid>;

//--------------------------------------------------------------------------------------------------
// Implementations
//--------------------------------------------------------------------------------------------------

impl PrivateForest {
    /// Encrypts supplied bytes with a random nonce and AES key.
    pub(crate) fn encrypt<R: RngCore>(key: &Key, data: &[u8], rng: &mut R) -> Result<Vec<u8>> {
        key.encrypt(&Key::generate_nonce(rng), data)
    }

    /// Sets a new value at the given key.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    ///
    /// use chrono::Utc;
    /// use rand::thread_rng;
    ///
    /// use wnfs::{
    ///     private::{PrivateForest, PrivateRef}, PrivateNode,
    ///     BlockStore, MemoryBlockStore, Namefilter, PrivateDirectory, PrivateOpResult,
    /// };
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let store = &mut MemoryBlockStore::default();
    ///     let rng = &mut thread_rng();
    ///     let forest = Rc::new(PrivateForest::new());
    ///     let dir = Rc::new(PrivateDirectory::new(
    ///         Namefilter::default(),
    ///         Utc::now(),
    ///         rng,
    ///     ));
    ///
    ///     let private_ref = &dir.header.get_private_ref().unwrap();
    ///     let name = dir.header.get_saturated_name();
    ///     let node = PrivateNode::Dir(dir);
    ///
    ///     let forest = forest.set(name, private_ref, &node, store, rng).await.unwrap();
    ///     assert_eq!(forest.get(private_ref, store).await.unwrap(), Some(node));
    /// }
    /// ```
    pub async fn set<B: BlockStore, R: RngCore>(
        self: Rc<Self>,
        saturated_name: Namefilter,
        private_ref: &PrivateRef,
        value: &PrivateNode,
        store: &mut B,
        rng: &mut R,
    ) -> Result<Rc<Self>> {
        debug!("Private Forest Set: PrivateRef: {:?}", private_ref);

        // Serialize node to cbor.
        let cbor_bytes = value.serialize_to_cbor(rng)?;

        // Encrypt bytes with content key.
        let enc_bytes = Self::encrypt(&private_ref.content_key.0, &cbor_bytes, rng)?;

        // Store content section in blockstore and get Cid.
        let content_cid = store.put_block(enc_bytes, libipld::IpldCodec::Raw).await?;

        // Store header and Cid in root node.
        self.set_encrypted(saturated_name, content_cid, store).await
    }

    /// Gets the value at the given key.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    ///
    /// use chrono::Utc;
    /// use rand::thread_rng;
    ///
    /// use wnfs::{
    ///     private::{PrivateForest, PrivateRef}, PrivateNode,
    ///     BlockStore, MemoryBlockStore, Namefilter, PrivateDirectory, PrivateOpResult,
    /// };
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let store = &mut MemoryBlockStore::default();
    ///     let rng = &mut thread_rng();
    ///     let forest = Rc::new(PrivateForest::new());
    ///     let dir = Rc::new(PrivateDirectory::new(
    ///         Namefilter::default(),
    ///         Utc::now(),
    ///         rng,
    ///     ));
    ///
    ///     let private_ref = &dir.header.get_private_ref().unwrap();
    ///     let name = dir.header.get_saturated_name();
    ///     let node = PrivateNode::Dir(dir);
    ///
    ///     let forest = forest.set(name, private_ref, &node, store, rng).await.unwrap();
    ///     assert_eq!(forest.get(private_ref, store).await.unwrap(), Some(node));
    /// }
    /// ```
    pub async fn get<B: BlockStore>(
        &self,
        private_ref: &PrivateRef,
        store: &B,
    ) -> Result<Option<PrivateNode>> {
        debug!("Private Forest Get: PrivateRef: {:?}", private_ref);

        // Fetch Cid from root node.
        let cid = match self
            .get_encrypted(&private_ref.saturated_name_hash, store)
            .await?
        {
            Some(value) => value,
            None => return Ok(None),
        };

        // Fetch encrypted bytes from blockstore.
        let enc_bytes = store.get_block(cid).await?;

        // Decrypt bytes
        let cbor_bytes = private_ref.content_key.0.decrypt(&enc_bytes)?;

        // Deserialize bytes.
        Ok(Some(PrivateNode::deserialize_from_cbor(
            &cbor_bytes,
            &private_ref.ratchet_key,
        )?))
    }

    /// Checks that a value with the given saturated name hash key exists.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    ///
    /// use chrono::Utc;
    /// use rand::thread_rng;
    /// use sha3::Sha3_256;
    ///
    ///
    /// use wnfs::{
    ///     private::{PrivateForest, PrivateRef}, PrivateNode,
    ///     BlockStore, MemoryBlockStore, Namefilter, PrivateDirectory, PrivateOpResult, Hasher
    /// };
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let store = &mut MemoryBlockStore::default();
    ///     let rng = &mut thread_rng();
    ///     let forest = Rc::new(PrivateForest::new());
    ///     let dir = Rc::new(PrivateDirectory::new(
    ///         Namefilter::default(),
    ///         Utc::now(),
    ///         rng,
    ///     ));
    ///
    ///     let private_ref = &dir.header.get_private_ref().unwrap();
    ///     let name = dir.header.get_saturated_name();
    ///     let node = PrivateNode::Dir(dir);
    ///     let forest = forest.set(name.clone(), private_ref, &node, store, rng).await.unwrap();
    ///
    ///     let name_hash = &Sha3_256::hash(&name.as_bytes());
    ///
    ///     assert!(forest.has(name_hash, store).await.unwrap());
    /// }
    /// ```
    pub async fn has<B: BlockStore>(
        &self,
        saturated_name_hash: &HashOutput,
        store: &B,
    ) -> Result<bool> {
        Ok(self
            .root
            .get_by_hash(saturated_name_hash, store)
            .await?
            .is_some())
    }

    /// Sets a new encrypted value at the given key.
    pub async fn set_encrypted<B: BlockStore>(
        self: Rc<Self>,
        name: Namefilter,
        value: Cid,
        store: &mut B,
    ) -> Result<Rc<Self>> {
        let mut cloned = (*self).clone();
        cloned.root = self.root.set(name, value, store).await?;
        Ok(Rc::new(cloned))
    }

    /// Gets the encrypted value at the given key.
    #[inline]
    pub async fn get_encrypted<'b, B: BlockStore>(
        &'b self,
        name_hash: &HashOutput,
        store: &B,
    ) -> Result<Option<&'b Cid>> {
        self.root.get_by_hash(name_hash, store).await
    }

    /// Removes the encrypted value at the given key.
    pub async fn remove_encrypted<B: BlockStore>(
        self: Rc<Self>,
        name_hash: &HashOutput,
        store: &mut B,
    ) -> Result<(Rc<Self>, Option<Cid>)> {
        let mut cloned = (*self).clone();
        let (root, pair) = cloned.root.remove_by_hash(name_hash, store).await?;
        cloned.root = root;
        Ok((Rc::new(cloned), pair.map(|p| p.value)))
    }
}

// //--------------------------------------------------------------------------------------------------
// // Tests
// //--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod hamt_store_tests {
    use proptest::test_runner::{RngAlgorithm, TestRng};
    use std::rc::Rc;
    use test_log::test;

    use chrono::Utc;

    use super::*;
    use crate::{private::PrivateDirectory, MemoryBlockStore};

    #[test(async_std::test)]
    async fn inserted_items_can_be_fetched() {
        let store = &mut MemoryBlockStore::new();
        let hamt = Rc::new(PrivateForest::new());
        let rng = &mut TestRng::deterministic_rng(RngAlgorithm::ChaCha);

        let dir = Rc::new(PrivateDirectory::new(
            Namefilter::default(),
            Utc::now(),
            rng,
        ));

        let private_ref = dir.header.get_private_ref().unwrap();
        let saturated_name = dir.header.get_saturated_name();
        let private_node = PrivateNode::Dir(dir.clone());

        let hamt = hamt
            .set(saturated_name, &private_ref, &private_node, store, rng)
            .await
            .unwrap();

        let retrieved = hamt.get(&private_ref, store).await.unwrap().unwrap();

        assert_eq!(retrieved, private_node);
    }
}
