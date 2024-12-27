use std::{
    collections::HashSet,
    convert::TryInto,
    hash::Hash,
    path::{Path, PathBuf},
    process::Output,
    rc::Rc,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
    u128,
};

use anyhow::{anyhow, Context};
use futures::Future;
use itertools::Itertools;
use redb::{Database, ReadableTable, TableDefinition, TypeName};
use serde::{Deserialize, Serialize};
use tracing::{error, info, instrument, warn};
use walkdir::WalkDir;

/// File-synchronized database for arbitrary collections derived from file contents
///
/// The generic type is the value type in these collections. It must be serializable and deserializable.
///
/// Data is stored in KV stores in the form (FileKey, Vec<T>)
pub struct FileDB<T>
where
    T: Serialize + for<'a> Deserialize<'a> + 'static,
{
    dir: &'static Path,
    /// File -> Entity; there can be multiple entities per file.
    cache: Option<Vec<(Arc<FileKey>, Arc<T>)>>,
    _t: std::marker::PhantomData<T>,
}

/// String Relative Path: path of a file relative to the root_dir of the collection of files.
pub type FileKey = String;
/// Semantic File State: semantic state of the file, indicated by the time that it was last modified
///
/// Maybe in the future this will be a hash or some other state indication.
#[derive(Debug, Eq, Hash, PartialEq, Clone, Copy)]
struct FileState(u128);

/// What is this? This is an interface for constructing and chaining sync effects for the files in the FileDB.
///
/// It allows for constructing update logic for both the files and the items of the database, as well as chain
/// operations together for efficient and easily composable updates of the database (composing the functinos on the file
/// instead of mutating the database).
///
/// What does it do? This allows you to construct the update logic for the sub-sets of your collection that are out of sync,
/// and do so without worrying too much about the storage mechanisms for these collections or about collection contents that needs to be removed
/// from the database
///
/// When the item type can be updated to the DB, the run function will be available to perform this update.
/// The item type can be updated to the DB when the item type is the same as the DB type, where the DB type is the
/// is the value type of values stored in collections associate with files.
///
/// Initially, the Item type of the sync will be (). The provided functions are meant to allow for transforming the ()
/// into the database item type. Once this happens, the run function will become available and you can sync to the database.
pub struct Sync<SyncItem, DBItem>
where
    DBItem: Serialize + for<'a> Deserialize<'a> + 'static,
{
    db: FileDB<DBItem>,
    updates: Vec<FileItemUpdate<SyncItem>>,
    deletes: HashSet<FileKey>,
}

#[derive(Debug)]
struct FileItemUpdate<Item> {
    key: Arc<FileKey>,
    state: FileState,
    sync_item: Item,
}

use util::ResultIteratorExt;

type FileContent = Arc<str>;

// methods related to constructing the initial sync.
impl<DatabaseItem> Sync<(), DatabaseItem>
where
    DatabaseItem: Serialize + for<'a> Deserialize<'a> + 'static,
{
    #[instrument(skip(self, f))]
    /// Populate the sync using the recent content of the file, the key of the file, and the possibly
    /// the collection slice related to the file.
    pub async fn async_populate<I, F: Future<Output = I>>(
        self,
        f: impl for<'a> Fn(Arc<FileKey>, FileContent) -> F + Copy,
    ) -> Sync<I, DatabaseItem>
    where
        I: std::fmt::Debug,
    {
        let dir = self.db.dir;
        let futures = self.updates.into_iter().map(
            |FileItemUpdate {
                 key,
                 state,
                 sync_item: _,
             }| async move {
                let root_dir = dir;
                let path = root_dir.join(key.as_str());
                let content = tokio::fs::read_to_string(&path).await?;
                let sync_item = f(key.clone(), Arc::from(content)).await;

                anyhow::Ok(FileItemUpdate {
                    key,
                    state,
                    sync_item: sync_item.into(),
                })
            },
        );

        let updates = futures::future::join_all(futures)
            .await
            .into_iter()
            .flatten_results_and_log()
            .collect::<Vec<_>>();

        Sync {
            db: self.db,
            deletes: self.deletes,
            updates,
        }
    }
}

impl<I, V> Sync<I, V>
where
    V: Serialize + for<'a> Deserialize<'a> + 'static,
{
    pub fn map<IP>(self, f: impl Fn(I) -> IP) -> Sync<IP, V> {
        Sync {
            db: self.db,
            deletes: self.deletes,
            updates: self
                .updates
                .into_iter()
                .map(|file_item_update| file_item_update.map(&f))
                .collect(),
        }
    }

    /// maps sync items into collection, then flattens the collections while keeping the sync items associated files
    pub fn flat_map<IP, C: IntoIterator<Item = IP>>(self, f: impl Fn(&I) -> C) -> Sync<IP, V> {
        Sync {
            db: self.db,
            deletes: self.deletes,
            updates: self
                .updates
                .into_iter()
                .flat_map(
                    |FileItemUpdate {
                         key,
                         state,
                         sync_item,
                     }| {
                        f(&sync_item).into_iter().map(move |it| FileItemUpdate {
                            key: key.clone(),
                            state,
                            sync_item: it,
                        })
                    },
                )
                .collect(),
        }
    }

    /// Batch map the full collection. The order and count of sync values must be maintained.
    pub async fn external_async_map<IP, F: Future<Output = anyhow::Result<Vec<IP>>>>(
        self,
        f: impl Fn(Vec<I>) -> F,
    ) -> anyhow::Result<Sync<IP, V>> {
        let keys = self
            .updates
            .iter()
            .map(|it| (it.key.clone(), it.state))
            .collect::<Vec<_>>();

        let old_values = self
            .updates
            .into_iter()
            .map(|it| it.sync_item)
            .collect::<Vec<_>>();
        let result = f(old_values).await?;

        let updates = keys
            .into_iter()
            .zip(result)
            .map(|((key, state), new_value)| FileItemUpdate {
                key,
                state,
                sync_item: new_value,
            })
            .collect::<Vec<_>>();

        Ok(Sync {
            db: self.db,
            deletes: self.deletes,
            updates,
        })
    }
}

// flatten
impl<
        ItemInner,
        IterableItem: IntoIterator<Item = ItemInner>,
        DatabaseItem: Serialize + for<'a> Deserialize<'a>,
    > Sync<IterableItem, DatabaseItem>
{
    /// Flatten the inner collection while maintaining file association of the items
    pub fn inner_flatten(self) -> Sync<ItemInner, DatabaseItem> {
        Sync {
            db: self.db,
            deletes: self.deletes,
            updates: self
                .updates
                .into_iter()
                .flat_map(
                    |FileItemUpdate {
                         key,
                         state,
                         sync_item,
                     }| {
                        sync_item
                            .into_iter()
                            .map(|item| FileItemUpdate {
                                key: key.clone(),
                                state,
                                sync_item: item,
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .collect(),
        }
    }
}

impl<I> Sync<I, I>
where
    I: Serialize + for<'a> Deserialize<'a> + 'static,
{
    pub async fn run(self) -> anyhow::Result<FileDB<I>> {
        let Sync {
            db,
            updates,
            deletes,
        } = self;

        let updates: Vec<(Arc<FileKey>, FileState, Vec<I>)> = updates
            .into_iter()
            .into_group_map_by(|update| (update.key.clone(), update.state))
            .into_iter()
            .map(|(key, updates)| {
                (
                    key,
                    updates.into_iter().map(|update| update.sync_item).collect(),
                )
            })
            .map(|it| (it.0 .0, it.0 .1, it.1))
            .collect();

        db.apply_sync(updates, deletes)
    }
}

const DB_NAME: &str = "oxide.db";

// yes this is a wacky trait bound but seems to be necessary for redb given our configuration.
impl<T> FileDB<T>
where
    T: Serialize + for<'a> Deserialize<'a> + 'static,
{
    /// Table definition: FileKey: String, Value: Vec of binary serialized items which make up the file-derived collection
    // if performance is bad, TODO try changing this to use zero copy.
    const TABLE: TableDefinition<'static, FileKey, Vec<Vec<u8>>> =
        TableDefinition::new("main-table");
    const STATE_TABLE: TableDefinition<'static, FileKey, FileState> =
        TableDefinition::new("state-table");

    fn db_path(&self) -> PathBuf {
        self.dir.join(DB_NAME)
    }

    pub fn new(dir: &'static Path) -> Self {
        let cache = Self {
            dir,
            cache: None,
            _t: std::marker::PhantomData,
        }
        .mem_map(None);

        let cache = match cache {
            Ok(cache) => Some(cache),
            Err(e) => {
                warn!("Failed to create cache: {e:?}");
                None
            }
        };

        Self {
            dir,
            cache,
            _t: std::marker::PhantomData,
        }
    }

    #[instrument(skip(self))]
    pub fn new_msync(self) -> anyhow::Result<Sync<(), T>> {
        // recursively walk the file directory
        let new_files_state: HashSet<(Arc<FileKey>, FileState)> = {
            let walker = WalkDir::new(self.dir)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    // If it's a directory, only enter if it's not hidden
                    if e.file_type().is_dir() {
                        return !e
                            .file_name()
                            .to_str()
                            .map(|s| s.starts_with('.'))
                            .unwrap_or(false);
                    }

                    // For files, check both hidden status and markdown extension
                    !e.file_name()
                        .to_str()
                        .map(|s| s.starts_with('.'))
                        .unwrap_or(false)
                        && e.path()
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| {
                                ext.eq_ignore_ascii_case("md")
                                    || ext.eq_ignore_ascii_case("markdown")
                            })
                            .unwrap_or(false)
                })
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_file());

            walker
                .logging_flat_map(|it| {
                    // ignore any metadata errors; I don't forsee these being an issue
                    let path = it.path();
                    let metadata = path.metadata()?;
                    let file_state: FileState = metadata.modified()?.into();
                    let relative_path = path.strip_prefix(self.dir)?;
                    anyhow::Ok((Arc::new(relative_path.to_string_lossy().into_owned()), file_state))
                })
                .collect()
        };

        let old_files_state: HashSet<(Arc<FileKey>, FileState)> = self.state()?.unwrap_or_default();

        let diff_new_and_different = new_files_state
            .difference(&old_files_state)
            .collect::<Vec<_>>();
        info!("{} Updated files", diff_new_and_different.len());

        let new_paths: HashSet<&FileKey> = new_files_state.iter().map(|(key, _)| key.as_ref()).collect();
        let old_paths: HashSet<&FileKey> = old_files_state.iter().map(|(key, _)| key.as_ref()).collect();

        let removed_paths: HashSet<FileKey> = old_paths
            .difference(&new_paths)
            .into_iter()
            .map(|it| it.to_string())
            .collect();

        info!("{} Deleted files", removed_paths.len());

        Ok(Sync {
            db: self,
            deletes: removed_paths.into_iter().collect(),
            updates: diff_new_and_different
                .into_iter()
                .map(|(key, state)| FileItemUpdate {
                    key: key.clone(),
                    state: *state,
                    sync_item: ().into(),
                })
                .collect(),
        })
    }

    #[instrument(skip_all)]
    fn apply_sync(
        self,
        updates: Vec<(Arc<FileKey>, FileState, Vec<T>)>,
        deletes: HashSet<FileKey>,
    ) -> anyhow::Result<Self> {
        let db = Database::create(self.dir.join(DB_NAME))?;
        let write_txn = db.begin_write()?;

        {
            let mut main_table = write_txn.open_table(Self::TABLE)?;
            let mut state_table = write_txn.open_table(Self::STATE_TABLE)?;

            for (key, state, collection) in updates.as_slice() {
                let serialized = collection
                    .into_iter()
                    .map(|it| bincode::serialize(&it))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| {
                        anyhow!("Failed to serialize item for key {} with error {e:?}", key)
                    })?;

                main_table.insert(key.as_ref(), serialized)?;
                state_table.insert(key.as_ref(), state)?;
            }

            for key in deletes.iter() {
                main_table.remove(key)?;
                state_table.remove(key)?;
            }
        }

        write_txn.commit()?;

        // Update cache after syncing
        let cache = self.mem_map(Some(db))?;

        Ok(Self {
            dir: self.dir,
            cache: Some(cache),
            _t: std::marker::PhantomData,
        })
    }

    #[instrument(skip(self))]
    fn state(&self) -> anyhow::Result<Option<HashSet<(Arc<FileKey>, FileState)>>> {
        match Database::open(self.db_path()) {
            Ok(db) => {
                let read_txn = db
                    .begin_read()
                    .context("Failed to begin read transaction")?;
                let table = read_txn
                    .open_table(Self::STATE_TABLE)
                    .context("Failed to open table")?;

                let result = table
                    .iter()?
                    .map(|entry| entry.map(|(key, state)| (Arc::from(key.value()), state.value())))
                    .collect::<Result<HashSet<_>, _>>()?;

                Ok(Some(result))
            }
            Err(redb::DatabaseError::Storage(redb::StorageError::Io(io_error)))
                if io_error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    #[instrument(skip(self))]
    fn mem_map(
        &self,
        database: Option<redb::Database>,
    ) -> anyhow::Result<Vec<(Arc<FileKey>, Arc<T>)>> {
        let cache = self
            .db_iter(database)?
            .map(|(key, value)| (Arc::new(key), Arc::new(value)))
            .collect_vec();
        info!("Created memory cache with {} items", cache.len());
        Ok(cache)
    }

    fn deserialize_db_value(value: &[u8]) -> anyhow::Result<T> {
        Ok(bincode::deserialize(&value)?)
    }

    /// Iterator over all items in the database with their file keys
    #[instrument(skip(self))]
    pub fn db_iter(
        &self,
        database: Option<redb::Database>,
    ) -> anyhow::Result<impl Iterator<Item = (FileKey, T)>> {
        let read_txn = match database {
            Some(db) => db.begin_read()?,
            None => Database::open(self.db_path())?.begin_read()?,
        };
        let table = read_txn.open_table(Self::TABLE)?;

        let items: Vec<_> = table
            .iter()?
            .map(|result| {
                result.map(|(key_guard, value_guard)| {
                    let key = key_guard.value().to_string();
                    let values = value_guard.value();
                    (key, values)
                })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flat_map(|(key, values)| {
                values
                    .iter()
                    .filter_map(move |value| {
                        Self::deserialize_db_value(value)
                            .ok()
                            .map(|item| (key.clone(), item))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        Ok(items.into_iter())
    }

    /// Iterator over just the items, without file keys
    /// Iterator over the in-memory cache of items with their file keys
    #[instrument(skip(self))]
    pub fn iter(&self) -> impl Iterator<Item = (&FileKey, &T)> {
        self.cache.as_ref().expect("this should exist because if you are iterating over the FileDB, the cache should exist").iter().map(|(key, value)| (key.as_ref(), value.as_ref()))
    }

    pub fn values(&self) -> anyhow::Result<impl Iterator<Item = Arc<T>>> {
        Ok(self.db_iter(None)?.map(|(_, value)| value.into()))
    }

    /// Iterator over file keys
    pub fn keys(&self) -> anyhow::Result<impl Iterator<Item = FileKey>> {
        let db = Database::open(self.db_path())?;
        let read_txn = db.begin_read()?;
        let table = read_txn.open_table(Self::TABLE)?;

        let keys: Vec<_> = table
            .iter()?
            .map(|result| result.map(|(key_guard, _)| key_guard.value().to_string()))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(keys.into_iter())
    }

    pub fn fold<B, F>(&self, init: B, mut f: F) -> anyhow::Result<B>
    where
        F: FnMut(B, &str, &T) -> B,
    {
        let db = Database::open(self.db_path())?;
        let read_txn = db.begin_read()?;
        let table = read_txn.open_table(Self::TABLE)?;

        table
            .iter()?
            .flatten_results_and_log()
            .try_fold(init, |acc, (key_guard, value_guard)| {
                let key = key_guard.value();
                let values = value_guard.value();

                values.iter().try_fold(acc, |inner_acc, value| {
                    let value: T = Self::deserialize_db_value(value)?;
                    Ok(f(inner_acc, &key, &value))
                })
            })
    }

    pub fn map<U, F: Copy>(&self, f: F) -> anyhow::Result<Vec<U>>
    where
        F: Fn(&FileKey, &T) -> U,
    {
        let db = Database::open(self.db_path())?;
        let read_txn = db.begin_read()?;
        let table = read_txn.open_table(Self::TABLE)?;

        table
            .iter()?
            .flatten_results_and_log()
            .flat_map(|(key_guard, value_guard)| {
                let key = key_guard.value();
                let values = value_guard.value();
                values
                    .iter()
                    .map(|value| Ok(f(&key, &Self::deserialize_db_value(value)?)))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

impl From<SystemTime> for FileState {
    fn from(value: SystemTime) -> Self {
        let t = value
            .duration_since(UNIX_EPOCH)
            .expect("SystemTime before UNIX EPOCH!")
            .as_millis() as u128; // This will truncate if the value is too lar

        FileState(t)
    }
}

impl redb::Value for FileState {
    type SelfType<'a> = Self where Self: 'a;

    type AsBytes<'a> = [u8; std::mem::size_of::<u128>()] where Self: 'a;

    fn fixed_width() -> Option<usize> {
        Some(std::mem::size_of::<FileState>())
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let t = u128::from_le_bytes(data.try_into().expect("Deserializing: Invalid data length")); // this should not happen -- in theory.
        Self(t)
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        value.0.to_le_bytes()
    }

    fn type_name() -> redb::TypeName {
        TypeName::new("vault::FileState")
    }
}

impl<Item> FileItemUpdate<Item> {
    fn map<ItemP>(self, f: &impl Fn(Item) -> ItemP) -> FileItemUpdate<ItemP> {
        FileItemUpdate {
            key: self.key,
            state: self.state,
            sync_item: f(self.sync_item),
        }
    }

    fn with_item_moved<I>(self, item: I) -> FileItemUpdate<I> {
        FileItemUpdate {
            key: self.key,
            state: self.state,
            sync_item: item.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_msync_workflow() -> anyhow::Result<()> {
        // Create a temporary directory for our test files
        let temp_dir = TempDir::new()?;
        let temp_path = temp_dir.path();

        // Create some test markdown files
        let files = vec![
            ("file1.md", "# Header 1\nContent 1"),
            ("file2.md", "# Header 2\nContent 2"),
            ("file3.md", "# Header 3\nContent 3"),
        ];

        for (filename, content) in &files {
            let mut file = File::create(temp_path.join(filename))?;
            file.write_all(content.as_bytes())?;
        }

        // Initialize FileDB
        let db = FileDB::<String>::new(Box::leak(temp_path.to_path_buf().into_boxed_path()));

        // Step 1: Populate the database using flatmap
        let msync: Sync<(), String> = db.new_msync()?;
        let populated_db = msync
            .async_populate(|_file_key, content| async move { content.to_string() })
            .await
            .flat_map(|content| {
                content
                    .lines()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
            })
            .run()
            .await?;

        // Step 2: Validate the population using the database map method
        let lines: Vec<String> = populated_db.map(|_, line| line.to_string())?;
        println!("{:?}", lines);
        assert_eq!(lines.len(), 6); // 3 files, 2 lines each
        assert!(lines.contains(&"# Header 1".to_string()));
        assert!(lines.contains(&"Content 1".to_string()));
        assert!(lines.contains(&"# Header 2".to_string()));
        assert!(lines.contains(&"Content 2".to_string()));
        assert!(lines.contains(&"# Header 3".to_string()));
        assert!(lines.contains(&"Content 3".to_string()));

        // Step 3: Clean up (this is handled automatically by TempDir when it goes out of scope)

        Ok(())
    }

    #[test]
    fn test_filedb_iteration() -> anyhow::Result<()> {
        let temp_dir = TempDir::new()?;
        let db: FileDB<String> =
            FileDB::new(Box::leak(temp_dir.path().to_path_buf().into_boxed_path()));

        // First insert some test data
        let updates = vec![
            (
                Arc::from("file1.md".to_string()),
                FileState(1),
                vec!["content1".to_string()],
            ),
            (
                Arc::from("file2.md".to_string()),
                FileState(2),
                vec!["content2".to_string()],
            ),
        ];
        let db = db.apply_sync(updates, vec![].into_iter().collect())?;

        // Test iter()
        let items: Vec<_> = db.db_iter(None)?.collect();
        assert_eq!(items.len(), 2);
        assert!(items
            .iter()
            .any(|(k, v)| k == "file1.md" && v.as_str() == "content1"));
        assert!(items
            .iter()
            .any(|(k, v)| k == "file2.md" && v.as_str() == "content2"));

        // Test values()
        let values: Vec<_> = db.values()?.collect();
        assert_eq!(values.len(), 2);
        assert!(values.iter().any(|v| v.as_ref() == "content1"));
        assert!(values.iter().any(|v| v.as_ref() == "content2"));

        // Test keys()
        let keys: Vec<_> = db.keys()?.collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"file1.md".to_string()));
        assert!(keys.contains(&"file2.md".to_string()));

        // Test iter()
        let cache_items: Vec<_> = db.iter().collect();
        assert_eq!(cache_items.len(), 2);
        assert!(cache_items
            .iter()
            .any(|(k, v)| *k == "file1.md" && v.as_str() == "content1"));
        assert!(cache_items
            .iter()
            .any(|(k, v)| *k == "file2.md" && v.as_str() == "content2"));

        Ok(())
    }
}
