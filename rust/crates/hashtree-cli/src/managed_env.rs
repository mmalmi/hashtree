use hashtree_core::lmdb_runtime::{
    acquire_lmdb_write_permit, with_lmdb_lock_acquisition, LmdbWritePermit,
};
use heed::{Env, EnvOpenOptions, RoTxn, RwTxn};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static ENV_REFERENCES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

fn env_references() -> &'static Mutex<HashMap<PathBuf, usize>> {
    ENV_REFERENCES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Closes Heed's globally cached environment after the last managed owner drops.
pub(crate) struct ManagedEnv {
    env: Option<Env>,
    path: PathBuf,
}

impl ManagedEnv {
    /// # Safety
    ///
    /// This has the same safety requirements as [`EnvOpenOptions::open`].
    pub(crate) unsafe fn open<P: AsRef<Path>>(
        options: &EnvOpenOptions,
        path: P,
    ) -> Result<Self, heed::Error> {
        let mut references = env_references()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let env = with_lmdb_lock_acquisition(|| unsafe { options.open(path) })?;
        let path = env.path().to_path_buf();
        let count = references.entry(path.clone()).or_default();
        *count = count
            .checked_add(1)
            .expect("managed LMDB environment reference count overflow");
        Ok(Self {
            env: Some(env),
            path,
        })
    }

    pub(crate) fn read_txn(&self) -> heed::Result<RoTxn<'_>> {
        with_lmdb_lock_acquisition(|| self.deref().read_txn())
    }

    pub(crate) fn write_txn(&self) -> heed::Result<ManagedRwTxn<'_>> {
        let permit = acquire_lmdb_write_permit();
        let txn = self.deref().write_txn()?;
        Ok(ManagedRwTxn {
            txn: Some(txn),
            _permit: permit,
        })
    }
}

pub(crate) struct ManagedRwTxn<'env> {
    txn: Option<RwTxn<'env>>,
    _permit: LmdbWritePermit,
}

impl ManagedRwTxn<'_> {
    pub(crate) fn commit(mut self) -> heed::Result<()> {
        self.txn
            .take()
            .expect("managed LMDB transaction accessed after completion")
            .commit()
    }
}

impl<'env> Deref for ManagedRwTxn<'env> {
    type Target = RwTxn<'env>;

    fn deref(&self) -> &Self::Target {
        self.txn
            .as_ref()
            .expect("managed LMDB transaction accessed after completion")
    }
}

impl DerefMut for ManagedRwTxn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.txn
            .as_mut()
            .expect("managed LMDB transaction accessed after completion")
    }
}

impl Deref for ManagedEnv {
    type Target = Env;

    fn deref(&self) -> &Self::Target {
        self.env
            .as_ref()
            .expect("managed LMDB environment accessed after drop")
    }
}

impl Drop for ManagedEnv {
    fn drop(&mut self) {
        let Some(env) = self.env.take() else {
            return;
        };
        let mut references = env_references()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = references
            .get_mut(&self.path)
            .expect("managed LMDB environment reference was not registered");
        *count = count
            .checked_sub(1)
            .expect("managed LMDB environment reference count underflow");
        if *count == 0 {
            references.remove(&self.path);
            let _closing = env.prepare_for_closing();
        }
    }
}
