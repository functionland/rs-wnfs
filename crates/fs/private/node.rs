use std::{cmp::Ordering, fmt::Debug, io::Cursor, rc::Rc};

use anyhow::{bail, Result};
use async_recursion::async_recursion;
use chrono::{DateTime, Utc};
use libipld::{
    cbor::DagCborCodec,
    prelude::{Decode, Encode},
    serde as ipld_serde, Ipld,
};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use sha3::Sha3_256;
use skip_ratchet::{seek::JumpSize, Ratchet, RatchetSeeker};

use crate::{utils, BlockStore, FsError, HashOutput, Id, NodeType, HASH_BYTE_SIZE};

use super::{
    hamt::Hasher, namefilter::Namefilter, Key, PrivateDirectory, PrivateFile, PrivateForest,
};

//--------------------------------------------------------------------------------------------------
// Type Definitions
//--------------------------------------------------------------------------------------------------

pub type INumber = HashOutput;

/// Represents a node in the WNFS private file system. This can either be a file or a directory.
///
/// # Examples
///
/// ```
/// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
/// use chrono::Utc;
/// use std::rc::Rc;
/// use rand::thread_rng;
///
/// let rng = &mut thread_rng();
/// let dir = Rc::new(PrivateDirectory::new(
///     Namefilter::default(),
///     Utc::now(),
///     rng,
/// ));
///
/// let node = PrivateNode::Dir(dir);
///
/// println!("Node: {:?}", node);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum PrivateNode {
    File(Rc<PrivateFile>),
    Dir(Rc<PrivateDirectory>),
}

/// The key used to encrypt the content of a node.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct ContentKey(pub Key);

/// The key used to encrypt the header section of a node.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct RatchetKey(pub Key);

/// This is the header of a private node. It contains secret information about the node which includes
/// the inumber, the ratchet, and the namefilter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateNodeHeader {
    /// A unique identifier of the node.
    pub(crate) inumber: INumber,
    /// Used both for versioning and deriving keys for that enforces privacy.
    pub(crate) ratchet: Ratchet,
    /// Used for ancestry checks and as a key fot the HAMT.
    pub(crate) bare_name: Namefilter,
}

/// PrivateRef holds the information to fetch associated node from a HAMT and decrypt it if it is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateRef {
    /// Sha3-256 hash of saturated namefilter.
    pub(crate) saturated_name_hash: HashOutput,
    /// Sha3-256 hash of the ratchet key.
    pub(crate) content_key: ContentKey,
    /// Skip-ratchet-derived key.
    pub(crate) ratchet_key: RatchetKey,
}

//--------------------------------------------------------------------------------------------------
// Implementations
//--------------------------------------------------------------------------------------------------

impl PrivateNode {
    /// Creates node with upserted modified time.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let dir = Rc::new(PrivateDirectory::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::Dir(dir);
    ///
    /// let time = Utc::now();
    /// let node = node.upsert_mtime(time);
    ///
    /// assert_eq!(
    ///     time,
    ///     node.as_dir()
    ///         .unwrap()
    ///         .get_metadata()
    ///         .get_modified()
    ///         .unwrap()
    /// );
    /// ```
    pub fn upsert_mtime(&self, time: DateTime<Utc>) -> Self {
        match self {
            Self::File(file) => {
                let mut file = (**file).clone();
                file.metadata.upsert_mtime(time);
                Self::File(Rc::new(file))
            }
            Self::Dir(dir) => {
                let mut dir = (**dir).clone();
                dir.metadata.upsert_mtime(time);
                Self::Dir(Rc::new(dir))
            }
        }
    }

    /// Generates two random set of bytes.
    pub(crate) fn generate_double_random<R: RngCore>(rng: &mut R) -> (HashOutput, HashOutput) {
        const _DOUBLE_SIZE: usize = HASH_BYTE_SIZE * 2;
        let [first, second] = unsafe {
            std::mem::transmute::<[u8; _DOUBLE_SIZE], [[u8; HASH_BYTE_SIZE]; 2]>(
                utils::get_random_bytes::<_DOUBLE_SIZE>(rng),
            )
        };
        (first, second)
    }

    /// Updates bare name ancestry of private sub tree.
    #[async_recursion(?Send)]
    pub(crate) async fn update_ancestry<B: BlockStore, R: RngCore>(
        &mut self,
        parent_bare_name: Namefilter,
        hamt: Rc<PrivateForest>,
        store: &mut B,
        rng: &mut R,
    ) -> Result<Rc<PrivateForest>> {
        let hamt = match self {
            Self::File(file) => {
                let mut file = (**file).clone();

                file.header.update_bare_name(parent_bare_name);
                file.header.reset_ratchet(rng);

                *self = Self::File(Rc::new(file));

                hamt
            }
            Self::Dir(old_dir) => {
                let mut dir = (**old_dir).clone();

                let mut working_hamt = Rc::clone(&hamt);
                for (name, private_ref) in &old_dir.entries {
                    let mut node = hamt
                        .get(private_ref, store)
                        .await?
                        .ok_or(FsError::NotFound)?;

                    working_hamt = node
                        .update_ancestry(dir.header.bare_name.clone(), working_hamt, store, rng)
                        .await?;

                    dir.entries
                        .insert(name.clone(), node.get_header().get_private_ref()?);
                }

                dir.header.update_bare_name(parent_bare_name);
                dir.header.reset_ratchet(rng);

                *self = Self::Dir(Rc::new(dir));

                working_hamt
            }
        };

        let header = self.get_header();

        hamt.set(
            header.get_saturated_name(),
            &header.get_private_ref()?,
            self,
            store,
            rng,
        )
        .await
    }

    /// Gets the header of the node.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let dir = Rc::new(PrivateDirectory::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::Dir(Rc::clone(&dir));
    ///
    /// assert_eq!(&dir.header, node.get_header());
    /// ```
    pub fn get_header(&self) -> &PrivateNodeHeader {
        match self {
            Self::File(file) => &file.header,
            Self::Dir(dir) => &dir.header,
        }
    }

    /// Casts a node to a directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let dir = Rc::new(PrivateDirectory::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::Dir(Rc::clone(&dir));
    ///
    /// assert_eq!(node.as_dir().unwrap(), dir);
    /// ```
    pub fn as_dir(&self) -> Result<Rc<PrivateDirectory>> {
        Ok(match self {
            Self::Dir(dir) => Rc::clone(dir),
            _ => bail!(FsError::NotADirectory),
        })
    }

    /// Casts a node to a file.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateFile, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let file = Rc::new(PrivateFile::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     b"hello world".to_vec(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::File(Rc::clone(&file));
    ///
    /// assert_eq!(node.as_file().unwrap(), file);
    /// ```
    pub fn as_file(&self) -> Result<Rc<PrivateFile>> {
        Ok(match self {
            Self::File(file) => Rc::clone(file),
            _ => bail!(FsError::NotAFile),
        })
    }

    /// Returns true if underlying node is a directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let dir = Rc::new(PrivateDirectory::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::Dir(dir);
    ///
    /// assert!(node.is_dir());
    /// ```
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    /// Returns true if the underlying node is a file.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateFile, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let file = Rc::new(PrivateFile::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     b"hello world".to_vec(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::File(file);
    ///
    /// assert!(node.is_file());
    /// ```
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// Gets the latest version of the node using exponential search.
    pub(crate) async fn search_latest<B: BlockStore>(
        &self,
        forest: &PrivateForest,
        store: &B,
    ) -> Result<PrivateNode> {
        let header = self.get_header();

        let private_ref = &header.get_private_ref()?;
        if !forest.has(&private_ref.saturated_name_hash, store).await? {
            return Ok(self.clone());
        }

        // Start an exponential search w/ configurable `Small` JumpSize.
        let mut search = RatchetSeeker::new(header.ratchet.clone(), JumpSize::Small);
        let mut current_header = header.clone();

        loop {
            let current = search.current();
            current_header.ratchet = current.clone();

            let has_curr = forest
                .has(
                    &current_header.get_private_ref()?.saturated_name_hash,
                    store,
                )
                .await?;

            let ord = if has_curr {
                Ordering::Less
            } else {
                Ordering::Greater
            };

            if !search.step(ord) {
                break;
            }
        }

        current_header.ratchet = search.current().clone();

        let latest_private_ref = current_header.get_private_ref()?;

        match forest.get(&latest_private_ref, store).await? {
            Some(node) => Ok(node),
            None => unreachable!(),
        }
    }

    /// Serializes the node with provided Serde serialilzer.
    pub(crate) fn serialize<S, R: RngCore>(
        &self,
        serializer: S,
        rng: &mut R,
    ) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            PrivateNode::File(file) => file.serialize(serializer, rng),
            PrivateNode::Dir(dir) => dir.serialize(serializer, rng),
        }
    }

    /// Serializes the node to dag-cbor bytes.
    pub(crate) fn serialize_to_cbor<R: RngCore>(&self, rng: &mut R) -> Result<Vec<u8>> {
        let ipld = self.serialize(ipld_serde::Serializer, rng)?;
        let mut bytes = Vec::new();
        ipld.encode(DagCborCodec, &mut bytes)?;
        Ok(bytes)
    }

    /// Deserializes the node from dag-cbor bytes.
    pub(crate) fn deserialize_from_cbor(bytes: &[u8], key: &RatchetKey) -> Result<Self> {
        let ipld = Ipld::decode(DagCborCodec, &mut Cursor::new(bytes))?;
        (ipld, key).try_into()
    }
}

impl TryFrom<(Ipld, &RatchetKey)> for PrivateNode {
    type Error = anyhow::Error;

    fn try_from(pair: (Ipld, &RatchetKey)) -> Result<Self> {
        match pair {
            (Ipld::Map(map), key) => {
                let r#type: NodeType = map
                    .get("type")
                    .ok_or(FsError::MissingNodeType)?
                    .try_into()?;

                Ok(match r#type {
                    NodeType::PrivateFile => {
                        PrivateNode::from(PrivateFile::deserialize(Ipld::Map(map), key)?)
                    }
                    NodeType::PrivateDirectory => {
                        PrivateNode::from(PrivateDirectory::deserialize(Ipld::Map(map), key)?)
                    }
                    other => bail!(FsError::UnexpectedNodeType(other)),
                })
            }
            other => bail!("Expected `Ipld::Map` got {:#?}", other),
        }
    }
}

impl Id for PrivateNode {
    fn get_id(&self) -> String {
        match self {
            Self::File(file) => file.get_id(),
            Self::Dir(dir) => dir.get_id(),
        }
    }
}

impl From<PrivateFile> for PrivateNode {
    fn from(file: PrivateFile) -> Self {
        Self::File(Rc::new(file))
    }
}

impl From<PrivateDirectory> for PrivateNode {
    fn from(dir: PrivateDirectory) -> Self {
        Self::Dir(Rc::new(dir))
    }
}

impl PrivateNodeHeader {
    /// Creates a new PrivateNodeHeader.
    pub fn new<R: RngCore>(parent_bare_name: Namefilter, rng: &mut R) -> Self {
        let (inumber, ratchet_seed) = PrivateNode::generate_double_random(rng);
        Self {
            bare_name: {
                let mut namefilter = parent_bare_name;
                namefilter.add(&inumber);
                namefilter
            },
            ratchet: Ratchet::zero(ratchet_seed),
            inumber,
        }
    }

    /// Advances the ratchet.
    pub fn advance_ratchet(&mut self) {
        self.ratchet.inc();
    }

    /// Gets the private ref of the current header.
    pub fn get_private_ref(&self) -> Result<PrivateRef> {
        let ratchet_key = Key::new(self.ratchet.derive_key());
        let saturated_name_hash = Sha3_256::hash(&self.get_saturated_name_with_key(&ratchet_key));

        Ok(PrivateRef {
            saturated_name_hash,
            content_key: ContentKey(Key::new(Sha3_256::hash(&ratchet_key.as_bytes()))),
            ratchet_key: RatchetKey(ratchet_key),
        })
    }

    /// Gets the saturated namefilter for this node using the provided ratchet key.
    pub fn get_saturated_name_with_key(&self, ratchet_key: &Key) -> Namefilter {
        let mut name = self.bare_name.clone();
        name.add(&ratchet_key.as_bytes());
        name.saturate();
        name
    }

    /// Gets the saturated namefilter for this node.
    #[inline]
    pub fn get_saturated_name(&self) -> Namefilter {
        let ratchet_key = Key::new(self.ratchet.derive_key());
        self.get_saturated_name_with_key(&ratchet_key)
    }

    /// Updates the bare name of the node.
    pub fn update_bare_name(&mut self, parent_bare_name: Namefilter) {
        self.bare_name = {
            let mut namefilter = parent_bare_name;
            namefilter.add(&self.inumber);
            namefilter
        };
    }

    /// Resets the ratchet.
    pub fn reset_ratchet<R: RngCore>(&mut self, rng: &mut R) {
        self.ratchet = Ratchet::zero(utils::get_random_bytes(rng))
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod private_node_tests {
    use proptest::test_runner::{RngAlgorithm, TestRng};

    use super::*;

    #[test]
    fn serialized_private_node_can_be_deserialized() {
        let rng = &mut TestRng::deterministic_rng(RngAlgorithm::ChaCha);
        let original_file = PrivateNode::File(Rc::new(PrivateFile::new(
            Namefilter::default(),
            Utc::now(),
            b"Lorem ipsum dolor sit amet".to_vec(),
            rng,
        )));
        let private_ref = original_file.get_header().get_private_ref().unwrap();

        let bytes = original_file.serialize_to_cbor(rng).unwrap();
        let deserialized_node =
            PrivateNode::deserialize_from_cbor(&bytes, &private_ref.ratchet_key).unwrap();

        assert_eq!(original_file, deserialized_node);
    }
}
