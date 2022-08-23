//! Public fs directory node.

use std::{cmp, collections::BTreeMap, rc::Rc};

use crate::{
    error, utils, AsyncSerialize, BlockStore, FsError, Id, Metadata, PathNodes, PathNodesResult,
    ReferenceableStore, UnixFsNodeKind,
};
use anyhow::{bail, ensure, Result};
use async_recursion::async_recursion;
use async_stream::try_stream;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use libipld::Cid;
use serde::{ser::Error as SerError, Deserialize, Deserializer, Serialize, Serializer};

use super::{PublicFile, PublicLink, PublicNode};

//--------------------------------------------------------------------------------------------------
// Type Definitions
//--------------------------------------------------------------------------------------------------

pub type PublicPathNodes = PathNodes<PublicDirectory>;
pub type PublicPathNodesResult = PathNodesResult<PublicDirectory>;

/// A directory in a WNFS public file system.
///
/// # Examples
///
/// ```
/// use wnfs::{public::PublicDirectory, Id};
/// use chrono::Utc;
///
/// let dir = PublicDirectory::new(Utc::now());
///
/// println!("id = {}", dir.get_id());
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct PublicDirectory {
    pub metadata: Metadata,
    pub(crate) userland: BTreeMap<String, PublicLink>,
    pub(crate) previous: Option<Cid>,
}

#[derive(Serialize, Deserialize)]
struct PublicDirectorySerde {
    metadata: Metadata,
    userland: BTreeMap<String, Cid>,
    previous: Option<Cid>,
}

/// The result of an operation applied to a directory.
#[derive(Debug, Clone, PartialEq)]
pub struct PublicOpResult<T> {
    /// The root directory.
    pub root_dir: Rc<PublicDirectory>,
    /// Implementation dependent but it usually the last leaf node operated on.
    pub result: T,
}

//--------------------------------------------------------------------------------------------------
// Implementations
//--------------------------------------------------------------------------------------------------

impl PublicDirectory {
    /// Creates a new directory with provided time.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::PublicDirectory, Id};
    /// use chrono::Utc;
    ///
    /// let dir = PublicDirectory::new(Utc::now());
    ///
    /// println!("id = {}", dir.get_id());
    /// ```
    pub fn new(time: DateTime<Utc>) -> Self {
        Self {
            metadata: Metadata::new(time, UnixFsNodeKind::Dir),
            userland: BTreeMap::new(),
            previous: None,
        }
    }

    /// Gets the previous value of the directory.
    #[inline]
    pub fn get_previous(self: &Rc<Self>) -> Option<Cid> {
        self.previous
    }

    /// Gets the metadata of the directory
    pub fn get_metadata<'a>(self: &'a Rc<Self>) -> &'a Metadata {
        &self.metadata
    }

    /// Creates a new `PublicPathNodes` that is not based on an existing file tree.
    pub(crate) fn create_path_nodes(
        path_segments: &[String],
        time: DateTime<Utc>,
    ) -> PublicPathNodes {
        let path: Vec<(Rc<PublicDirectory>, String)> = path_segments
            .iter()
            .map(|segment| (Rc::new(PublicDirectory::new(time)), segment.clone()))
            .collect();

        PublicPathNodes {
            path,
            tail: Rc::new(PublicDirectory::new(time)),
        }
    }

    /// Uses specified path segments and their existence in the file tree to generate `PathNodes`.
    ///
    /// Supports cases where the entire path does not exist.
    pub(crate) async fn get_path_nodes<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        store: &B,
    ) -> Result<PublicPathNodesResult> {
        use PathNodesResult::*;
        let mut working_node = self;
        let mut path_nodes = Vec::with_capacity(path_segments.len());

        for segment in path_segments.iter() {
            match working_node.lookup_node(segment, store).await? {
                Some(PublicNode::Dir(ref directory)) => {
                    path_nodes.push((Rc::clone(&working_node), segment.clone()));
                    working_node = Rc::clone(directory);
                }
                Some(_) => {
                    let path_nodes = PathNodes {
                        path: path_nodes,
                        tail: Rc::clone(&working_node),
                    };

                    return Ok(NotADirectory(path_nodes, segment.clone()));
                }
                None => {
                    let path_nodes = PathNodes {
                        path: path_nodes,
                        tail: Rc::clone(&working_node),
                    };

                    return Ok(MissingLink(path_nodes, segment.clone()));
                }
            }
        }

        Ok(Complete(PublicPathNodes {
            path: path_nodes,
            tail: Rc::clone(&working_node),
        }))
    }

    /// Uses specified path segments to generate `PathNodes`. Creates missing directories as needed.
    pub(crate) async fn get_or_create_path_nodes<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        time: DateTime<Utc>,
        store: &B,
    ) -> Result<PublicPathNodes> {
        use PathNodesResult::*;
        match self.get_path_nodes(path_segments, store).await? {
            Complete(path_nodes) => Ok(path_nodes),
            NotADirectory(_, _) => error(FsError::InvalidPath),
            MissingLink(path_so_far, missing_link) => {
                let missing_path = path_segments.split_at(path_so_far.path.len() + 1).1;
                let missing_path_nodes = Self::create_path_nodes(missing_path, time);

                Ok(PublicPathNodes {
                    path: [
                        path_so_far.path,
                        vec![(path_so_far.tail, missing_link)],
                        missing_path_nodes.path,
                    ]
                    .concat(),
                    tail: missing_path_nodes.tail,
                })
            }
        }
    }

    /// Fix up `PathNodes` so that parents refer to the newly updated children.
    fn fix_up_path_nodes(path_nodes: PublicPathNodes) -> Rc<Self> {
        if path_nodes.path.is_empty() {
            return path_nodes.tail;
        }

        let mut working_dir = path_nodes.tail;
        for (dir, segment) in path_nodes.path.iter().rev() {
            let mut dir = (**dir).clone();
            let link = PublicLink::with_dir(working_dir);
            dir.userland.insert(segment.clone(), link);
            working_dir = Rc::new(dir);
        }

        working_dir
    }

    /// Follows a path and fetches the node at the end of the path.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, MemoryBlockStore};
    /// use std::rc::Rc;
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let time = Utc::now();
    ///     let dir = Rc::new(PublicDirectory::new(time));
    ///     let store = MemoryBlockStore::default();
    ///
    ///     let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .mkdir(&["pictures".into(), "cats".into()], Utc::now(), &store)
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir, result } = root_dir
    ///         .get_node(&["pictures".into()], &store)
    ///         .await
    ///         .unwrap();
    ///
    ///     assert!(result.is_some());
    /// }
    /// ```
    pub async fn get_node<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        store: &B,
    ) -> Result<PublicOpResult<Option<PublicNode>>> {
        use PathNodesResult::*;
        let root_dir = Rc::clone(&self);

        Ok(match path_segments.split_last() {
            Some((path_segment, parent_path)) => {
                match self.get_path_nodes(parent_path, store).await? {
                    Complete(parent_path_nodes) => PublicOpResult {
                        root_dir,
                        result: parent_path_nodes
                            .tail
                            .lookup_node(path_segment, store)
                            .await?,
                    },
                    MissingLink(_, _) => bail!(FsError::NotFound),
                    NotADirectory(_, _) => bail!(FsError::NotFound),
                }
            }
            None => PublicOpResult {
                root_dir,
                result: Some(PublicNode::Dir(self)),
            },
        })
    }

    /// Looks up a node by its path name in the current directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, Id, MemoryBlockStore};
    /// use std::rc::Rc;
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let mut store = MemoryBlockStore::default();
    ///
    ///     let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .mkdir(&["pictures".into(), "cats".into()], Utc::now(), &store)
    ///         .await
    ///         .unwrap();
    ///
    ///     let node = root_dir.lookup_node("pictures", &store).await.unwrap();
    ///
    ///     assert!(node.is_some());
    /// }
    /// ```
    pub async fn lookup_node<B: BlockStore>(
        &self,
        path_segment: &str,
        store: &B,
    ) -> Result<Option<PublicNode>> {
        Ok(match self.userland.get(path_segment) {
            Some(link) => Some(link.resolve_value(store).await?.clone()),
            None => None,
        })
    }

    #[async_recursion(?Send)]
    /// Stores directory in provided block store.
    ///
    /// This function can be recursive if the directory contains other directories.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::PublicDirectory, Id, MemoryBlockStore};
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let mut store = MemoryBlockStore::default();
    ///     let dir = PublicDirectory::new(Utc::now());
    ///
    ///     dir.store(&mut store).await.unwrap();
    /// }
    /// ```
    #[inline(always)]
    pub async fn store<B: BlockStore>(&self, store: &mut B) -> Result<Cid> {
        store.put_async_serializable(self).await
    }

    /// Reads specified file content from the directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, MemoryBlockStore};
    /// use libipld::cid::Cid;
    /// use std::rc::Rc;
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let time = Utc::now();
    ///     let dir = Rc::new(PublicDirectory::new(time));
    ///     let mut store = MemoryBlockStore::default();
    ///     let cid = Cid::default();
    ///
    ///     let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .write(
    ///             &["pictures".into(), "cats".into(), "tabby.png".into()],
    ///             cid,
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir, result } = root_dir
    ///         .read(&["pictures".into(), "cats".into(), "tabby.png".into()], &mut store)
    ///         .await
    ///         .unwrap();
    ///
    ///     assert_eq!(result, cid);
    /// }
    /// ```
    pub async fn read<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        store: &mut B,
    ) -> Result<PublicOpResult<Cid>> {
        let root_dir = Rc::clone(&self);
        let (path, filename) = utils::split_last(path_segments)?;

        match self.get_path_nodes(path, store).await? {
            PathNodesResult::Complete(node_path) => {
                match node_path.tail.lookup_node(filename, store).await? {
                    Some(PublicNode::File(file)) => Ok(PublicOpResult {
                        root_dir,
                        result: file.userland,
                    }),
                    Some(PublicNode::Dir(_)) => error(FsError::NotAFile),
                    None => error(FsError::NotFound),
                }
            }
            _ => error(FsError::NotFound),
        }
    }

    /// Writes a file to the directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, MemoryBlockStore};
    /// use libipld::cid::Cid;
    /// use std::rc::Rc;
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let time = Utc::now();
    ///     let dir = Rc::new(PublicDirectory::new(time));
    ///     let store = MemoryBlockStore::default();
    ///
    ///     let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .write(
    ///             &["pictures".into(), "cats".into(), "tabby.png".into()],
    ///             Cid::default(),
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    /// }
    /// ```
    pub async fn write<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        content_cid: Cid,
        time: DateTime<Utc>,
        store: &B,
    ) -> Result<PublicOpResult<()>> {
        let (directory_path, filename) = utils::split_last(path_segments)?;

        // This will create directories if they don't exist yet
        let mut directory_path_nodes = self
            .get_or_create_path_nodes(directory_path, time, store)
            .await?;

        let mut directory = (*directory_path_nodes.tail).clone();

        // Modify the file if it already exists, otherwise create a new file with expected content
        let file = match directory.lookup_node(filename, store).await? {
            Some(PublicNode::File(file_before)) => {
                let mut file = (*file_before).clone();
                file.userland = content_cid;
                file.metadata = Metadata::new(time, UnixFsNodeKind::File);
                file
            }
            Some(PublicNode::Dir(_)) => bail!(FsError::DirectoryAlreadyExists),
            None => PublicFile::new(time, content_cid),
        };

        // insert the file into its parent directory
        directory
            .userland
            .insert(filename.to_string(), PublicLink::with_file(Rc::new(file)));
        directory_path_nodes.tail = Rc::new(directory);

        // Fix up the file path
        Ok(PublicOpResult {
            root_dir: Self::fix_up_path_nodes(directory_path_nodes),
            result: (),
        })
    }

    /// Creates a new directory at the specified path.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, Id, MemoryBlockStore};
    /// use std::rc::Rc;
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let mut store = MemoryBlockStore::default();
    ///
    ///     let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .mkdir(&["pictures".into(), "cats".into()], Utc::now(), &store)
    ///         .await
    ///         .unwrap();
    /// }
    /// ```
    ///
    /// This method acts like `mkdir -p` in Unix because it creates intermediate directories if they do not exist.
    pub async fn mkdir<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        time: DateTime<Utc>,
        store: &B,
    ) -> Result<PublicOpResult<()>> {
        let path_nodes = self
            .get_or_create_path_nodes(path_segments, time, store)
            .await?;

        Ok(PublicOpResult {
            root_dir: Self::fix_up_path_nodes(path_nodes),
            result: (),
        })
    }

    /// Returns names and metadata of directory's immediate children.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, MemoryBlockStore};
    /// use libipld::cid::Cid;
    /// use std::rc::Rc;
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let time = Utc::now();
    ///     let dir = Rc::new(PublicDirectory::new(time));
    ///     let store = MemoryBlockStore::default();
    ///
    ///     let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .write(
    ///             &["pictures".into(), "cats".into(), "tabby.png".into()],
    ///             Cid::default(),
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir, result } = root_dir
    ///         .ls(&["pictures".into(), "cats".into()], &store)
    ///         .await
    ///         .unwrap();
    ///
    ///     assert_eq!(result.len(), 1);
    ///     assert_eq!(result[0].0, "tabby.png");
    /// }
    /// ```
    pub async fn ls<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        store: &B,
    ) -> Result<PublicOpResult<Vec<(String, Metadata)>>> {
        let root_dir = Rc::clone(&self);
        match self.get_path_nodes(path_segments, store).await? {
            PathNodesResult::Complete(path_nodes) => {
                let mut result = vec![];
                for (name, link) in path_nodes.tail.userland.iter() {
                    match link.resolve_value(store).await? {
                        PublicNode::File(file) => {
                            result.push((name.clone(), file.metadata.clone()));
                        }
                        PublicNode::Dir(dir) => {
                            result.push((name.clone(), dir.metadata.clone()));
                        }
                    }
                }
                Ok(PublicOpResult { root_dir, result })
            }
            _ => bail!(FsError::NotFound),
        }
    }

    /// Removes a file or directory from the directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, MemoryBlockStore};
    /// use libipld::cid::Cid;
    /// use std::rc::Rc;
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let time = Utc::now();
    ///     let dir = Rc::new(PublicDirectory::new(time));
    ///     let store = MemoryBlockStore::default();
    ///
    ///     let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .write(
    ///             &["pictures".into(), "cats".into(), "tabby.png".into()],
    ///             Cid::default(),
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir, .. } = root_dir
    ///         .rm(&["pictures".into(), "cats".into()], &store)
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir, result } = root_dir
    ///         .ls(&["pictures".into()], &store)
    ///         .await
    ///         .unwrap();
    ///
    ///     assert_eq!(result.len(), 0);
    /// }
    /// ```
    pub async fn rm<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        store: &B,
    ) -> Result<PublicOpResult<PublicNode>> {
        let (directory_path, node_name) = utils::split_last(path_segments)?;

        let mut directory_node_path = match self.get_path_nodes(directory_path, store).await? {
            PublicPathNodesResult::Complete(node_path) => node_path,
            _ => bail!(FsError::NotFound),
        };

        let mut directory = (*directory_node_path.tail).clone();

        // Remove the entry from its parent directory
        let removed_node = match directory.userland.remove(node_name) {
            Some(link) => link.get_owned_value(store).await?,
            None => bail!(FsError::NotFound),
        };

        directory_node_path.tail = Rc::new(directory);

        Ok(PublicOpResult {
            root_dir: Self::fix_up_path_nodes(directory_node_path),
            result: removed_node,
        })
    }

    /// Moves a file or directory from one path to another.
    ///
    /// This function requires stating the destination name explicitly.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, MemoryBlockStore};
    /// use libipld::cid::Cid;
    /// use std::rc::Rc;
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let time = Utc::now();
    ///     let dir = Rc::new(PublicDirectory::new(time));
    ///     let store = MemoryBlockStore::default();
    ///
    ///     let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .write(
    ///             &["pictures".into(), "cats".into(), "tabby.png".into()],
    ///             Cid::default(),
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir, .. } = root_dir
    ///         .basic_mv(
    ///             &["pictures".into(), "cats".into()],
    ///             &["cats".into()],
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir, result } = root_dir
    ///         .ls(&[], &store)
    ///         .await
    ///         .unwrap();
    ///
    ///     assert_eq!(result.len(), 2);
    /// }
    /// ```
    pub async fn basic_mv<B: BlockStore>(
        self: Rc<Self>,
        path_segments_from: &[String],
        path_segments_to: &[String],
        time: DateTime<Utc>,
        store: &B,
    ) -> Result<PublicOpResult<()>> {
        let root_dir = Rc::clone(&self);
        let (directory_path_nodes, filename) = utils::split_last(path_segments_to)?;

        let PublicOpResult {
            root_dir,
            result: removed_node,
        } = root_dir.rm(path_segments_from, store).await?;

        let mut path_nodes = match root_dir.get_path_nodes(directory_path_nodes, store).await? {
            PublicPathNodesResult::Complete(node_path) => node_path,
            _ => bail!(FsError::NotFound),
        };

        let mut directory = (*path_nodes.tail).clone();

        ensure!(
            !directory.userland.contains_key(filename),
            FsError::FileAlreadyExists
        );

        let removed_node = removed_node.update_mtime(time);

        directory
            .userland
            .insert(filename.clone(), PublicLink::new(removed_node));

        path_nodes.tail = Rc::new(directory);

        Ok(PublicOpResult {
            root_dir: Self::fix_up_path_nodes(path_nodes),
            result: (),
        })
    }

    /// Constructs a tree from directory with `base` as the historical ancestor.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, MemoryBlockStore};
    /// use libipld::cid::Cid;
    /// use std::rc::Rc;
    /// use chrono::Utc;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let time = Utc::now();
    ///     let dir = Rc::new(PublicDirectory::new(time));
    ///     let mut store = MemoryBlockStore::default();
    ///
    ///     let PublicOpResult { root_dir: base_root, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .write(
    ///             &["pictures".into(), "cats".into(), "tabby.png".into()],
    ///             Cid::default(),
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir: recent_root, .. } = Rc::clone(&base_root)
    ///         .write(
    ///             &["pictures".into(), "cats".into(), "katherine.png".into()],
    ///             Cid::default(),
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir: derived_root, .. } = recent_root
    ///         .base_history_on(base_root, &mut store)
    ///         .await
    ///         .unwrap();
    /// }
    /// ```
    pub async fn base_history_on<B: BlockStore>(
        self: Rc<Self>,
        base: Rc<Self>,
        store: &mut B,
    ) -> Result<PublicOpResult<()>> {
        if Rc::ptr_eq(&self, &base) {
            return Ok(PublicOpResult {
                root_dir: Rc::clone(&self),
                result: (),
            });
        }

        let mut dir = (*self).clone();
        dir.previous = Some(base.store(store).await?);

        for (name, entry) in self.userland.iter() {
            if let Some(base_entry) = base.userland.get(name) {
                if let Some(new_entry) =
                    Self::base_history_on_helper(entry, base_entry, store).await?
                {
                    dir.userland.insert(name.to_string(), new_entry);
                }
            }
        }

        Ok(PublicOpResult {
            root_dir: Rc::new(dir),
            result: (),
        })
    }

    /// Constructs a tree from directory with `base` as the historical ancestor.
    #[async_recursion(?Send)]
    pub(crate) async fn base_history_on_helper<B: BlockStore>(
        link: &PublicLink,
        base_link: &PublicLink,
        store: &mut B,
    ) -> Result<Option<PublicLink>> {
        if link.deep_eq(base_link, store).await? {
            return Ok(None);
        }

        let node = link.resolve_value(store).await?;
        let base_node = base_link.resolve_value(store).await?;

        let (mut dir, dir_rc, base_dir) = match (node, base_node) {
            (PublicNode::Dir(dir_rc), PublicNode::Dir(base_dir_rc)) => {
                let mut dir = (**dir_rc).clone();
                dir.previous = Some(*base_link.resolve_cid(store).await?);
                (dir, dir_rc, base_dir_rc)
            }
            (PublicNode::File(file_rc), PublicNode::File(_)) => {
                let mut file = (**file_rc).clone();
                file.previous = Some(*base_link.resolve_cid(store).await?);
                return Ok(Some(PublicLink::with_file(Rc::new(file))));
            }
            _ => {
                // One is a file and the other is a directory
                // No need to fix up previous links
                return Ok(None);
            }
        };

        for (name, entry) in dir_rc.userland.iter() {
            if let Some(base_entry) = base_dir.userland.get(name) {
                if let Some(new_entry) =
                    Self::base_history_on_helper(entry, base_entry, store).await?
                {
                    dir.userland.insert(name.to_string(), new_entry);
                }
            }
        }

        Ok(Some(PublicLink::with_dir(Rc::new(dir))))
    }

    /// Gets a stream for walking the history of a directory node.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::{rc::Rc, pin::Pin};
    ///
    /// use wnfs::{public::{PublicDirectory, PublicOpResult}, MemoryBlockStore};
    /// use libipld::cid::Cid;
    /// use chrono::Utc;
    /// use futures_util::pin_mut;
    /// use async_std::stream::StreamExt;
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let time = Utc::now();
    ///     let dir = Rc::new(PublicDirectory::new(time));
    ///     let mut store = MemoryBlockStore::default();
    ///
    ///     let PublicOpResult { root_dir: base_root, .. } = Rc::new(PublicDirectory::new(Utc::now()))
    ///         .write(
    ///             &["pictures".into(), "cats".into(), "tabby.png".into()],
    ///             Cid::default(),
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir: recent_root, .. } = Rc::clone(&base_root)
    ///         .write(
    ///             &["pictures".into(), "cats".into(), "katherine.png".into()],
    ///             Cid::default(),
    ///             Utc::now(),
    ///             &store
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     let PublicOpResult { root_dir: derived_root, .. } = recent_root
    ///         .base_history_on(base_root, &mut store)
    ///         .await
    ///         .unwrap();
    ///
    ///     let history = derived_root.get_history(&store);
    ///
    ///     pin_mut!(history);
    ///
    ///     while let Some(cid) = history.next().await {
    ///         println!("previous = {:?}", cid);
    ///     }
    /// }
    /// ```
    pub fn get_history<B: BlockStore>(
        self: Rc<Self>,
        store: &B,
    ) -> impl Stream<Item = Result<Cid>> + '_ {
        let mut working_node = self;
        try_stream! {
            while let Some(cid) = working_node.get_previous() {
                working_node = Rc::new(store.get_deserializable(&cid).await?);
                yield cid;
            }
        }
    }

    pub async fn basic_merge<B: BlockStore>(
        self: Rc<Self>,
        other: Rc<Self>,
        store: &mut B,
    ) -> Result<Rc<Self>> {
        let mut result: BTreeMap<String, PublicLink> = BTreeMap::new();

        for diff_item in MapDiff::new(self.userland.iter(), other.userland.iter()) {
            match diff_item {
                DiffItem::InLeft(key, value) => result.insert(key.clone(), value.clone()),
                DiffItem::InRight(key, value) => result.insert(key.clone(), value.clone()),
                DiffItem::InBoth(key, l_value, r_value) => {
                    let merged =
                        Self::basic_merge_links(l_value.clone(), r_value.clone(), store).await?;
                    result.insert(key.clone(), merged)
                }
            };
        }

        Ok(Rc::new(Self {
            // TODO commutative metadata merge
            metadata: self.metadata.clone(),
            // TODO does the previous link have to be equal? Or do we figure out which one is the longer hash-chain?
            previous: self.previous,
            userland: result,
        }))
    }

    #[async_recursion(?Send)]
    async fn basic_merge_links<B: BlockStore>(
        left: PublicLink,
        right: PublicLink,
        store: &mut B,
    ) -> Result<PublicLink> {
        if left.deep_eq(&right, store).await? {
            return Ok(left);
        }

        async fn tie_break<B: BlockStore>(
            left: PublicLink,
            right: PublicLink,
            store: &mut B,
        ) -> Result<PublicLink> {
            let left_cid = left.resolve_cid(store).await?;
            let right_cid = right.resolve_cid(store).await?;
            if left_cid < right_cid {
                Ok(left)
            } else {
                Ok(right)
            }
        }

        match (
            left.resolve_value(store).await?,
            right.resolve_value(store).await?,
        ) {
            (PublicNode::File(_), PublicNode::File(_)) => tie_break(left, right, store).await,
            (PublicNode::File(_), PublicNode::Dir(_)) => Ok(right),
            (PublicNode::Dir(_), PublicNode::File(_)) => Ok(left),
            (PublicNode::Dir(left_dir), PublicNode::Dir(right_dir)) => {
                let mut result: BTreeMap<String, PublicLink> = BTreeMap::new();
                for diff_item in MapDiff::new(left_dir.userland.iter(), right_dir.userland.iter()) {
                    match diff_item {
                        DiffItem::InLeft(key, value) => result.insert(key.clone(), value.clone()),
                        DiffItem::InRight(key, value) => result.insert(key.clone(), value.clone()),
                        DiffItem::InBoth(key, l_value, r_value) => {
                            let merged =
                                Self::basic_merge_links(l_value.clone(), r_value.clone(), store)
                                    .await?;
                            result.insert(key.clone(), merged)
                        }
                    };
                }
                Ok(PublicLink::from(PublicNode::Dir(Rc::new(
                    PublicDirectory {
                        metadata: left_dir.metadata.clone(),
                        previous: left_dir.previous,
                        userland: result,
                    },
                ))))
            }
        }
    }
}

impl Id for PublicDirectory {
    fn get_id(&self) -> String {
        format!("{:p}", &self.metadata)
    }
}

/// Implements async deserialization for serde serializable types.
#[async_trait(?Send)]
impl AsyncSerialize for PublicDirectory {
    type StoreRef = Cid;

    async fn async_serialize<S, RS>(&self, serializer: S, store: &mut RS) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        RS: ReferenceableStore<Ref = Self::StoreRef> + ?Sized,
    {
        let encoded_userland = {
            let mut map = BTreeMap::new();
            for (name, link) in self.userland.iter() {
                map.insert(
                    name.clone(),
                    *link
                        .resolve_cid(store)
                        .await
                        .map_err(|e| SerError::custom(format!("{}", e)))?,
                );
            }
            map
        };

        (PublicDirectorySerde {
            metadata: self.metadata.clone(),
            userland: encoded_userland,
            previous: self.previous,
        })
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PublicDirectory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let PublicDirectorySerde {
            metadata,
            userland,
            previous,
        } = PublicDirectorySerde::deserialize(deserializer)?;

        let decoded_userland = userland
            .into_iter()
            .map(|(name, cid)| (name, PublicLink::from_cid(cid)))
            .collect();

        Ok(Self {
            metadata,
            userland: decoded_userland,
            previous,
        })
    }
}

struct MapDiff<K, V, I> {
    left_item: Option<(K, V)>,
    left_iter: I,
    right_item: Option<(K, V)>,
    right_iter: I,
}

impl<K, V, I> MapDiff<K, V, I>
where
    I: Iterator<Item = (K, V)>,
{
    fn new(mut left: I, mut right: I) -> Self {
        let left_item = left.next();
        let right_item = right.next();
        Self {
            left_item,
            left_iter: left,
            right_item,
            right_iter: right,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DiffItem<K, V> {
    InLeft(K, V),
    InRight(K, V),
    InBoth(K, V, V),
}

impl<K, V, I> Iterator for MapDiff<K, V, I>
where
    I: Iterator<Item = (K, V)>,
    K: Clone + Ord,
    V: Clone,
{
    type Item = DiffItem<K, V>;

    fn next(&mut self) -> Option<Self::Item> {
        match (self.left_item.clone(), self.right_item.clone()) {
            (None, None) => None,
            (None, Some((key, value))) => {
                self.right_item = self.right_iter.next();
                Some(DiffItem::InRight(key, value))
            }
            (Some((key, value)), None) => {
                self.left_item = self.left_iter.next();
                Some(DiffItem::InLeft(key, value))
            }
            (Some((l_key, l_value)), Some((r_key, r_value))) => match l_key.cmp(&r_key) {
                cmp::Ordering::Less => {
                    self.left_item = self.left_iter.next();
                    Some(DiffItem::InLeft(l_key, l_value))
                }
                cmp::Ordering::Equal => {
                    self.left_item = self.left_iter.next();
                    self.right_item = self.right_iter.next();
                    Some(DiffItem::InBoth(l_key, l_value, r_value))
                }
                cmp::Ordering::Greater => {
                    self.right_item = self.right_iter.next();
                    Some(DiffItem::InRight(r_key, r_value))
                }
            },
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod public_directory_tests {
    use super::*;
    use crate::{dagcbor, public::PublicFile, MemoryBlockStore};
    use chrono::Utc;

    #[async_std::test]
    async fn look_up_can_fetch_file_added_to_directory() {
        let root_dir = Rc::new(PublicDirectory::new(Utc::now()));
        let store = MemoryBlockStore::default();
        let content_cid = Cid::default();
        let time = Utc::now();

        let PublicOpResult { root_dir, .. } = root_dir
            .write(&["text.txt".into()], content_cid, time, &store)
            .await
            .unwrap();

        let node = root_dir.lookup_node("text.txt", &store).await.unwrap();

        assert!(node.is_some());

        assert_eq!(
            node,
            Some(PublicNode::File(Rc::new(PublicFile::new(
                time,
                content_cid
            ))))
        );
    }

    #[async_std::test]
    async fn look_up_cannot_fetch_file_not_added_to_directory() {
        let root = PublicDirectory::new(Utc::now());
        let store = MemoryBlockStore::default();

        let node = root.lookup_node("Unknown", &store).await;

        assert!(node.is_ok());

        assert_eq!(node.unwrap(), None);
    }

    #[async_std::test]
    async fn directory_added_to_store_can_be_retrieved() {
        let root = PublicDirectory::new(Utc::now());
        let mut store = MemoryBlockStore::default();

        let cid = root.store(&mut store).await.unwrap();

        let encoded_dir = store.get_block(&cid).await.unwrap();
        let deserialized_dir = dagcbor::decode::<PublicDirectory>(encoded_dir.as_ref()).unwrap();

        assert_eq!(root, deserialized_dir);
    }

    #[async_std::test]
    async fn directory_can_encode_decode_as_cbor() {
        let root = PublicDirectory::new(Utc::now());
        let store = &mut MemoryBlockStore::default();

        let encoded_dir = dagcbor::async_encode(&root, store).await.unwrap();
        let decoded_dir = dagcbor::decode::<PublicDirectory>(encoded_dir.as_ref()).unwrap();

        assert_eq!(root, decoded_dir);
    }

    #[async_std::test]
    async fn mkdir_can_create_new_directory() {
        let time = Utc::now();
        let store = MemoryBlockStore::default();

        let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(time))
            .mkdir(&["tamedun".into(), "pictures".into()], time, &store)
            .await
            .unwrap();

        let PublicOpResult { result, .. } = root_dir
            .get_node(&["tamedun".into(), "pictures".into()], &store)
            .await
            .unwrap();

        assert!(result.is_some());
    }

    #[async_std::test]
    async fn ls_can_list_children_under_directory() {
        let time = Utc::now();
        let store = MemoryBlockStore::default();
        let root_dir = Rc::new(PublicDirectory::new(time));

        let PublicOpResult { root_dir, .. } = root_dir
            .mkdir(&["tamedun".into(), "pictures".into()], time, &store)
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .write(
                &["tamedun".into(), "pictures".into(), "puppy.jpg".into()],
                Cid::default(),
                time,
                &store,
            )
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .mkdir(
                &["tamedun".into(), "pictures".into(), "cats".into()],
                time,
                &store,
            )
            .await
            .unwrap();

        let PublicOpResult { result, .. } = root_dir
            .ls(&["tamedun".into(), "pictures".into()], &store)
            .await
            .unwrap();

        assert_eq!(result.len(), 2);

        assert_eq!(result[0].0, String::from("cats"));

        assert_eq!(result[1].0, String::from("puppy.jpg"));

        assert_eq!(result[0].1.unix_fs.kind, UnixFsNodeKind::Dir);

        assert_eq!(result[1].1.unix_fs.kind, UnixFsNodeKind::File);
    }

    #[async_std::test]
    async fn rm_can_remove_children_from_directory() {
        let time = Utc::now();
        let store = MemoryBlockStore::default();
        let root_dir = Rc::new(PublicDirectory::new(time));

        let PublicOpResult { root_dir, .. } = root_dir
            .mkdir(&["tamedun".into(), "pictures".into()], time, &store)
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .write(
                &["tamedun".into(), "pictures".into(), "puppy.jpg".into()],
                Cid::default(),
                time,
                &store,
            )
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .mkdir(
                &["tamedun".into(), "pictures".into(), "cats".into()],
                time,
                &store,
            )
            .await
            .unwrap();

        let result = root_dir
            .rm(&["tamedun".into(), "pictures".into()], &store)
            .await;

        assert!(result.is_ok());

        let result = result
            .unwrap()
            .root_dir
            .rm(&["tamedun".into(), "pictures".into()], &store)
            .await;

        assert!(result.is_err());
    }

    #[async_std::test]
    async fn read_can_fetch_userland_of_file_added_to_directory() {
        let mut store = MemoryBlockStore::default();
        let content_cid = Cid::default();
        let time = Utc::now();

        let PublicOpResult { root_dir, .. } = Rc::new(PublicDirectory::new(time))
            .write(&["text.txt".into()], content_cid, time, &store)
            .await
            .unwrap();

        let PublicOpResult { result, .. } = root_dir
            .read(&["text.txt".into()], &mut store)
            .await
            .unwrap();

        assert_eq!(result, content_cid);
    }

    #[async_std::test]
    async fn path_nodes_can_generates_new_path_nodes() {
        let store = MemoryBlockStore::default();
        let now = Utc::now();

        let path_nodes =
            PublicDirectory::create_path_nodes(&["Documents".into(), "Apps".into()], now);

        let fixed = PublicDirectory::fix_up_path_nodes(path_nodes.clone());
        let result = fixed
            .get_path_nodes(&["Documents".into(), "Apps".into()], &store)
            .await
            .unwrap();

        match result {
            PathNodesResult::MissingLink(_, segment) => panic!("MissingLink {segment}"),
            PathNodesResult::NotADirectory(_, segment) => panic!("NotADirectory {segment}"),
            PathNodesResult::Complete(path_nodes_2) => {
                assert_eq!(path_nodes.path.len(), path_nodes_2.path.len());
                assert_eq!(path_nodes.path[0].1, path_nodes_2.path[0].1);
                assert_eq!(path_nodes.path[1].1, path_nodes_2.path[1].1);
            }
        }
    }

    #[async_std::test]
    async fn base_history_on_can_create_a_new_derived_tree_pointing_to_base() {
        let time = Utc::now();
        let mut store = MemoryBlockStore::default();
        let root_dir = Rc::new(PublicDirectory::new(time));

        let PublicOpResult {
            root_dir: base_root,
            ..
        } = root_dir
            .write(
                &["pictures".into(), "cats".into(), "tabby.jpg".into()],
                Cid::default(),
                time,
                &store,
            )
            .await
            .unwrap();

        let PublicOpResult {
            root_dir: updated_root,
            ..
        } = Rc::clone(&base_root)
            .write(
                &["pictures".into(), "cats".into(), "luna.png".into()],
                Cid::default(),
                time,
                &store,
            )
            .await
            .unwrap();

        let PublicOpResult {
            root_dir: derived_root,
            ..
        } = updated_root
            .base_history_on(Rc::clone(&base_root), &mut store)
            .await
            .unwrap();

        // Assert that the root node points to its old version.
        let derived_previous_cid = derived_root.get_previous();
        let base_cid = base_root.store(&mut store).await.unwrap();

        assert!(derived_previous_cid.is_some());
        assert_eq!(derived_previous_cid.unwrap(), base_cid);

        // Assert that some node that exists between versions points to its old version.
        let PublicOpResult {
            result: derived_node,
            ..
        } = Rc::clone(&derived_root)
            .get_node(&["pictures".into(), "cats".into()], &store)
            .await
            .unwrap();

        let PublicOpResult {
            result: base_node, ..
        } = base_root
            .get_node(&["pictures".into(), "cats".into()], &store)
            .await
            .unwrap();

        assert!(derived_node.is_some());
        assert!(base_node.is_some());

        let derived_previous_cid = derived_node.unwrap().get_previous();
        let base_cid = base_node.unwrap().store(&mut store).await.unwrap();

        assert!(derived_previous_cid.is_some());
        assert_eq!(derived_previous_cid.unwrap(), base_cid);

        // Assert that some node that doesn't exists between versions does not point to anything.
        let PublicOpResult {
            result: derived_node,
            ..
        } = Rc::clone(&derived_root)
            .get_node(
                &["pictures".into(), "cats".into(), "luna.png".into()],
                &store,
            )
            .await
            .unwrap();

        assert!(derived_node.is_some());
        assert!(matches!(derived_node.unwrap().get_previous(), None));
    }

    #[async_std::test]
    async fn mv_can_move_sub_directory_to_another_valid_location() {
        let time = Utc::now();
        let store = MemoryBlockStore::default();
        let root_dir = Rc::new(PublicDirectory::new(time));

        let PublicOpResult { root_dir, .. } = root_dir
            .write(
                &["pictures".into(), "cats".into(), "tabby.jpg".into()],
                Cid::default(),
                time,
                &store,
            )
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .write(
                &["pictures".into(), "cats".into(), "luna.png".into()],
                Cid::default(),
                time,
                &store,
            )
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .mkdir(&["images".into()], time, &store)
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .basic_mv(
                &["pictures".into(), "cats".into()],
                &["images".into(), "cats".into()],
                Utc::now(),
                &store,
            )
            .await
            .unwrap();

        let PublicOpResult { root_dir, result } =
            root_dir.ls(&["images".into()], &store).await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, String::from("cats"));

        let PublicOpResult { result, .. } =
            root_dir.ls(&["pictures".into()], &store).await.unwrap();

        assert_eq!(result.len(), 0);
    }

    #[async_std::test]
    async fn mv_cannot_move_sub_directory_to_invalid_location() {
        let time = Utc::now();
        let store = MemoryBlockStore::default();
        let root_dir = Rc::new(PublicDirectory::new(time));

        let PublicOpResult { root_dir, .. } = root_dir
            .mkdir(
                &[
                    "videos".into(),
                    "movies".into(),
                    "anime".into(),
                    "ghibli".into(),
                ],
                time,
                &store,
            )
            .await
            .unwrap();

        let result = root_dir
            .basic_mv(
                &["videos".into(), "movies".into()],
                &["videos".into(), "movies".into(), "anime".into()],
                Utc::now(),
                &store,
            )
            .await;

        assert!(result.is_err());
    }

    #[async_std::test]
    async fn mv_can_rename_directories() {
        let time = Utc::now();
        let mut store = MemoryBlockStore::default();
        let root_dir = Rc::new(PublicDirectory::new(time));

        let PublicOpResult { root_dir, .. } = root_dir
            .write(&["file.txt".into()], Cid::default(), time, &store)
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .basic_mv(
                &["file.txt".into()],
                &["renamed.txt".into()],
                Utc::now(),
                &store,
            )
            .await
            .unwrap();

        let PublicOpResult { result, .. } = root_dir
            .read(&["renamed.txt".into()], &mut store)
            .await
            .unwrap();

        assert!(result == Cid::default());
    }

    #[async_std::test]
    async fn mv_fails_moving_directories_to_files() {
        let time = Utc::now();
        let store = MemoryBlockStore::default();
        let root_dir = Rc::new(PublicDirectory::new(time));

        let PublicOpResult { root_dir, .. } = root_dir
            .mkdir(&["movies".into(), "ghibli".into()], time, &store)
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .write(&["file.txt".into()], Cid::default(), time, &store)
            .await
            .unwrap();

        let result = root_dir
            .basic_mv(
                &["movies".into(), "ghibli".into()],
                &["file.txt".into()],
                Utc::now(),
                &store,
            )
            .await;

        assert!(result.is_err());
    }

    #[async_std::test]
    async fn basic_merge_example() {
        let time = Utc::now();
        let store = &mut MemoryBlockStore::default();
        let root_dir = Rc::new(PublicDirectory::new(time));

        let PublicOpResult { root_dir, .. } = root_dir
            .mkdir(&["movies".into(), "ghibli".into()], time, store)
            .await
            .unwrap();

        let PublicOpResult { root_dir, .. } = root_dir
            .write(&["file.txt".into()], Cid::default(), time, store)
            .await
            .unwrap();

        let PublicOpResult {
            root_dir: root_left,
            ..
        } = Rc::clone(&root_dir)
            .write(&["file2.txt".into()], Cid::default(), time, store)
            .await
            .unwrap();

        let PublicOpResult {
            root_dir: root_right,
            ..
        } = root_dir
            .write(
                &["movies".into(), "action.txt".into()],
                Cid::default(),
                time,
                store,
            )
            .await
            .unwrap();

        let merged = root_left.basic_merge(root_right, store).await.unwrap();

        let PublicOpResult {
            result: read_action,
            ..
        } = Rc::clone(&merged)
            .read(&["movies".into(), "action.txt".into()], store)
            .await
            .unwrap();

        let PublicOpResult {
            result: read_file, ..
        } = Rc::clone(&merged)
            .read(&["file.txt".into()], store)
            .await
            .unwrap();

        let PublicOpResult {
            result: read_file2, ..
        } = merged.read(&["file2.txt".into()], store).await.unwrap();

        assert_eq!(read_action, Cid::default());
        assert_eq!(read_file, Cid::default());
        assert_eq!(read_file2, Cid::default());
    }

    #[test]
    fn diff_iter_example() {
        let mut left = BTreeMap::new();
        let mut right = BTreeMap::new();

        left.insert("Hello", 1);
        right.insert("Hello", 2);

        left.insert("World", 3);
        right.insert("James", 4);

        let results: Vec<DiffItem<&&str, &i32>> = MapDiff::new(left.iter(), right.iter()).collect();

        assert_eq!(
            results,
            vec![
                DiffItem::InBoth(&"Hello", &1, &2),
                DiffItem::InRight(&"James", &4),
                DiffItem::InLeft(&"World", &3),
            ]
        )
    }
}
