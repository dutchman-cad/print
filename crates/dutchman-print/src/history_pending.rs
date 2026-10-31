//! Pending audit metadata is isolated from machine safety state and its locks.
use crate::history::Record;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use tokio::sync::{mpsc, oneshot, Mutex};

pub(crate) type Records = HashMap<String, Record>;
enum Write {
    Save(Vec<u8>),
    Flush(oneshot::Sender<()>),
}
#[derive(Default)]
pub(crate) struct Pending {
    pub records: Mutex<Records>,
    writer: OnceLock<mpsc::Sender<Write>>,
    legacy: OnceLock<Records>,
    migrated: Arc<AtomicBool>,
    #[cfg(test)]
    pub hook: Arc<WriteHook>,
}
#[derive(Default, Deserialize)]
struct Legacy {
    #[serde(default)]
    history: Records,
}
impl Pending {
    /// Called while the records mutex is held: snapshots are enqueued in mutation
    /// order. The writer owns the bytes and never takes the records mutex.
    pub fn changed(&self, path: &Path, records: &Records) {
        let bytes = match serde_json::to_vec(records) {
            Ok(bytes) => bytes,
            Err(_) => {
                crate::history::log_error();
                return;
            }
        };
        let writer = self.writer.get_or_init(|| {
            let (sender, mut receiver) = mpsc::channel(64);
            let path = path.to_path_buf();
            let migrated = self.migrated.clone();
            #[cfg(test)]
            let hook = self.hook.clone();
            tokio::spawn(async move {
                while let Some(write) = receiver.recv().await {
                    match write {
                        Write::Save(bytes) => {
                            #[cfg(test)]
                            hook.before_write().await;
                            if crate::safety::write_state(&path, &bytes).await.is_ok() {
                                migrated.store(true, Ordering::Release);
                            } else {
                                crate::history::log_error();
                            }
                        }
                        Write::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            });
            sender
        });
        if writer.try_send(Write::Save(bytes)).is_err() {
            crate::history::log_error();
        }
    }
    pub async fn flush(&self) {
        if let Some(writer) = self.writer.get() {
            let (sender, done) = oneshot::channel();
            if writer.send(Write::Flush(sender)).await.is_ok() {
                let _ = done.await;
            }
        }
    }
    pub async fn restore(&self, path: &Path, safety_bytes: Option<&[u8]>) {
        let records = match crate::safety::read_state(path).await {
            Ok(bytes) => match serde_json::from_slice::<Records>(&bytes) {
                Ok(records) if records.len() <= 5000 => {
                    self.migrated.store(true, Ordering::Release);
                    Some(records)
                }
                _ => {
                    crate::history::log_error();
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => {
                crate::history::log_error();
                None
            }
        };
        // Once a pending file exists it is authoritative, including an empty map.
        // Old safety snapshots must not resurrect already archived executions.
        if let Some(records) = records {
            *self.records.lock().await = records;
        } else if let Some(bytes) = safety_bytes {
            match serde_json::from_slice::<Legacy>(bytes) {
                Ok(legacy) if legacy.history.len() <= 5000 => {
                    if !legacy.history.is_empty() {
                        let _ = self.legacy.set(legacy.history.clone());
                        let mut records = self.records.lock().await;
                        *records = legacy.history;
                        self.changed(path, &records);
                    }
                }
                _ => crate::history::log_error(),
            }
        }
    }
    pub fn safety_bytes(&self, state: &impl Serialize) -> Result<Vec<u8>, serde_json::Error> {
        // Until the migration snapshot is durable, retain the immutable legacy
        // payload in safety checkpoints. This takes no history lock and prevents
        // a crash between safety checkpointing and the pending writer losing it.
        if !self.migrated.load(Ordering::Acquire) {
            if let Some(history) = self.legacy.get() {
                #[derive(Serialize)]
                struct Migrating<'a, T> {
                    #[serde(flatten)]
                    state: &'a T,
                    history: &'a Records,
                }
                return serde_json::to_vec(&Migrating { state, history });
            }
        }
        serde_json::to_vec(state)
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct WriteHook {
    pub block: AtomicBool,
    pub entered: tokio::sync::Notify,
    pub waiting: AtomicBool,
    pub release: tokio::sync::Notify,
}
#[cfg(test)]
impl WriteHook {
    async fn before_write(&self) {
        if self.block.swap(false, Ordering::AcqRel) {
            self.waiting.store(true, Ordering::Release);
            self.entered.notify_one();
            self.release.notified().await;
            self.waiting.store(false, Ordering::Release);
        }
    }
}
