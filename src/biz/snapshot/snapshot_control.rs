use crate::biz::collab::metrics::CollabMetrics;
use crate::biz::snapshot::cache::SnapshotCache;
use crate::biz::snapshot::queue::PendingQueue;
use crate::state::RedisClient;
use anyhow::anyhow;
use app_error::AppError;
use async_stream::stream;
use collab_rt::data_validation::validate_encode_collab;
use database::collab::{
  create_snapshot_and_maintain_limit, get_all_collab_snapshot_meta, select_snapshot,
  should_create_snapshot, AppResult, COLLAB_SNAPSHOT_LIMIT,
};
use database_entity::dto::{AFSnapshotMeta, AFSnapshotMetas, InsertSnapshotParams, SnapshotData};
use futures_util::StreamExt;
use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::time::interval;
use tracing::{debug, error, trace, warn};
use validator::Validate;

pub type SnapshotCommandReceiver = tokio::sync::mpsc::Receiver<SnapshotCommand>;
pub type SnapshotCommandSender = tokio::sync::mpsc::Sender<SnapshotCommand>;

pub const SNAPSHOT_TICK_INTERVAL: Duration = Duration::from_secs(2);

pub enum SnapshotCommand {
  InsertSnapshot(InsertSnapshotParams),
  Tick(tokio::sync::oneshot::Sender<SnapshotMetric>),
}

#[derive(Clone)]
pub struct SnapshotControl {
  cache: SnapshotCache,
  command_sender: SnapshotCommandSender,
  pg_pool: PgPool,
}

impl SnapshotControl {
  pub async fn new(
    redis_client: RedisClient,
    pg_pool: PgPool,
    collab_metrics: Arc<CollabMetrics>,
  ) -> Self {
    let redis_client = Arc::new(Mutex::new(redis_client));
    let (command_sender, rx) = tokio::sync::mpsc::channel(2000);
    let cache = SnapshotCache::new(redis_client);

    let runner = SnapshotCommandRunner::new(pg_pool.clone(), cache.clone(), rx);
    tokio::spawn(runner.run());

    let cloned_sender = command_sender.clone();
    tokio::spawn(async move {
      let mut interval = interval(SNAPSHOT_TICK_INTERVAL);
      loop {
        interval.tick().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Err(err) = cloned_sender.send(SnapshotCommand::Tick(tx)).await {
          error!("Failed to send tick command: {}", err);
        }

        if let Ok(metric) = rx.await {
          collab_metrics.record_write_snapshot(
            metric.success_write_snapshot_count,
            metric.total_write_snapshot_count,
          );
        }
      }
    });

    Self {
      cache,
      command_sender,
      pg_pool,
    }
  }

  pub async fn should_create_snapshot(&self, oid: &str) -> bool {
    if oid.is_empty() {
      warn!("unexpected empty object id when checking should_create_snapshot");
      return false;
    }

    should_create_snapshot(oid, &self.pg_pool)
      .await
      .unwrap_or(false)
  }

  pub async fn create_snapshot(&self, params: InsertSnapshotParams) -> AppResult<AFSnapshotMeta> {
    params.validate()?;

    debug!("create snapshot for object:{}", params.object_id);
    match self.pg_pool.try_begin().await {
      Ok(Some(transaction)) => {
        let meta = create_snapshot_and_maintain_limit(
          transaction,
          &params.workspace_id,
          &params.object_id,
          &params.encoded_collab_v1,
          COLLAB_SNAPSHOT_LIMIT,
        )
        .await?;
        Ok(meta)
      },
      _ => Err(AppError::Internal(anyhow!(
        "fail to acquire transaction to create snapshot for object:{}",
        params.object_id,
      ))),
    }
  }

  pub async fn get_collab_snapshot(&self, snapshot_id: &i64) -> AppResult<SnapshotData> {
    match select_snapshot(&self.pg_pool, snapshot_id).await? {
      None => Err(AppError::RecordNotFound(format!(
        "Can't find the snapshot with id:{}",
        snapshot_id
      ))),
      Some(row) => Ok(SnapshotData {
        object_id: row.oid,
        encoded_collab_v1: row.blob,
        workspace_id: row.workspace_id.to_string(),
      }),
    }
  }

  /// Returns list of snapshots for given object_id in descending order of creation time.
  pub async fn get_collab_snapshot_list(&self, oid: &str) -> AppResult<AFSnapshotMetas> {
    let metas = get_all_collab_snapshot_meta(&self.pg_pool, oid).await?;
    Ok(metas)
  }

  pub async fn queue_snapshot(&self, params: InsertSnapshotParams) -> Result<(), AppError> {
    params.validate()?;
    trace!("Queuing snapshot for {}", params.object_id);
    self
      .command_sender
      .send(SnapshotCommand::InsertSnapshot(params))
      .await
      .map_err(|err| AppError::Internal(err.into()))?;
    Ok(())
  }

  pub async fn get_snapshot(
    &self,
    workspace_id: &str,
    object_id: &str,
    snapshot_id: &i64,
  ) -> Result<SnapshotData, AppError> {
    let key = SnapshotKey::from_object_id(object_id);
    let encoded_collab_v1 = self.cache.try_get(&key.0).await.unwrap_or(None);

    match encoded_collab_v1 {
      None => self.get_collab_snapshot(snapshot_id).await,
      Some(encoded_collab_v1) => Ok(SnapshotData {
        encoded_collab_v1,
        workspace_id: workspace_id.to_string(),
        object_id: object_id.to_string(),
      }),
    }
  }
}

struct SnapshotCommandRunner {
  pg_pool: PgPool,
  queue: RwLock<PendingQueue>,
  cache: SnapshotCache,
  recv: Option<SnapshotCommandReceiver>,
  success_attempts: AtomicU64,
  total_attempts: AtomicU64,
}
impl SnapshotCommandRunner {
  fn new(pg_pool: PgPool, cache: SnapshotCache, recv: SnapshotCommandReceiver) -> Self {
    let queue = PendingQueue::new();
    Self {
      pg_pool,
      queue: RwLock::new(queue),
      cache,
      recv: Some(recv),
      success_attempts: Default::default(),
      total_attempts: Default::default(),
    }
  }

  async fn run(mut self) {
    let mut receiver = self.recv.take().expect("Only take once");
    let stream = stream! {
      while let Some(cmd) = receiver.recv().await {
         yield cmd;
      }
    };

    stream
      .for_each(|command| async {
        self.handle_command(command).await;
      })
      .await;
  }

  async fn handle_command(&self, command: SnapshotCommand) {
    match command {
      SnapshotCommand::InsertSnapshot(params) => {
        let mut queue = self.queue.write().await;
        let item = queue.generate_item(params.workspace_id, params.object_id, params.collab_type);
        let key = SnapshotKey::from_object_id(&item.object_id);
        queue.push_item(item);
        drop(queue);

        if let Err(err) = self.cache.insert(&key.0, params.encoded_collab_v1).await {
          error!("Failed to insert snapshot to cache: {}", err);
        }
      },
      SnapshotCommand::Tick(tx) => {
        if let Err(e) = self.process_next_batch().await {
          error!("Failed to process next batch: {}", e);
        }

        let _ = tx.send(SnapshotMetric {
          success_write_snapshot_count: self.success_attempts.load(Ordering::Relaxed) as i64,
          total_write_snapshot_count: self.total_attempts.load(Ordering::Relaxed) as i64,
        });
      },
    }
  }

  async fn process_next_batch(&self) -> Result<(), AppError> {
    let next_item = match self.queue.write().await.pop() {
      Some(item) => item,
      None => return Ok(()), // No items to process
    };

    let key = SnapshotKey::from_object_id(&next_item.object_id);
    self.total_attempts.fetch_add(1, Ordering::Relaxed);
    let encoded_collab_v1 = match self.cache.try_get(&key.0).await {
      Ok(Some(data)) => {
        // This step is not necessary, but use it to check if the data is valid. Will be removed
        // in the future.
        match validate_encode_collab(&next_item.object_id, &data, &next_item.collab_type) {
          Ok(_) => data,
          Err(err) => {
            error!(
              "Collab doc state is not correct when creating snapshot: {},{}",
              next_item.object_id, err
            );
            return Ok(());
          },
        }
      },
      Ok(None) => {
        warn!("Failed to get snapshot from cache: {}", key.0);
        return Ok(());
      },
      Err(_) => {
        if cfg!(debug_assertions) {
          error!("Failed to get snapshot from cache: {}", key.0);
        }
        self.queue.write().await.push_item(next_item);
        return Ok(());
      },
    };

    let transaction = match self.pg_pool.try_begin().await {
      Ok(Some(tx)) => tx,
      _ => {
        debug!("Failed to start transaction to write snapshot, retrying later");
        self.queue.write().await.push_item(next_item);
        return Ok(());
      },
    };

    match create_snapshot_and_maintain_limit(
      transaction,
      &next_item.workspace_id,
      &next_item.object_id,
      &encoded_collab_v1,
      COLLAB_SNAPSHOT_LIMIT,
    )
    .await
    {
      Ok(_) => {
        trace!(
          "successfully created snapshot for {}, remaining task: {}",
          next_item.object_id,
          self.queue.read().await.len()
        );
        let _ = self.cache.remove(&key.0).await;
        self.success_attempts.fetch_add(1, Ordering::Relaxed);
        Ok(())
      },
      Err(e) => {
        // self.queue.write().await.push_item(next_item);
        Err(e)
      },
    }
  }
}

const SNAPSHOT_PREFIX: &str = "full_snapshot";
struct SnapshotKey(String);

impl SnapshotKey {
  fn from_object_id(object_id: &str) -> Self {
    Self(format!("{}:{}", SNAPSHOT_PREFIX, object_id))
  }
}

pub struct SnapshotMetric {
  success_write_snapshot_count: i64,
  total_write_snapshot_count: i64,
}
