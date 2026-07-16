use heed::{Env, EnvOpenOptions};
use std::collections::HashMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static ENV_REFERENCES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

fn env_references() -> &'static Mutex<HashMap<PathBuf, usize>> {
    ENV_REFERENCES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Owns one process-local reference to a Heed environment.
///
/// Heed retains a strong global reference after ordinary `Env` handles are
/// dropped. The last managed owner must explicitly prepare the environment for
/// closing so LMDB releases file descriptors and, on macOS, named semaphores.
pub(crate) struct ManagedEnv {
    env: Option<Env>,
    path: PathBuf,
}

impl ManagedEnv {
    /// Open and register an environment as one atomic managed operation.
    ///
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
        let env = unsafe { options.open(path)? };
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
            // Keep the registry locked until Heed has removed its cached strong
            // reference, preventing a managed reopen from racing the final close.
            let _closing = env.prepare_for_closing();
        }
    }
}
