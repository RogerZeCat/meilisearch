use std::collections::BTreeMap;

use actix_web::web::Data;
use actix_web::{web, HttpRequest, HttpResponse};
use index_scheduler::IndexScheduler;
use log::debug;
use meilisearch_auth::AuthController;
use meilisearch_types::error::ResponseError;
use meilisearch_types::settings::{Settings, Unchecked};
use meilisearch_types::tasks::{Kind, Status, Task, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::analytics::Analytics;
use crate::extractors::authentication::policies::*;
use crate::extractors::authentication::GuardedData;

const PAGINATION_DEFAULT_LIMIT: usize = 20;

mod api_key;
mod dump;
pub mod indexes;
mod metrics;
mod multi_search;
mod swap_indexes;
pub mod tasks;

pub fn configure(cfg: &mut web::ServiceConfig, enable_metrics: bool) {
    cfg.service(web::scope("/tasks").configure(tasks::configure))
        .service(web::resource("/health").route(web::get().to(get_health)))
        .service(web::scope("/keys").configure(api_key::configure))
        .service(web::scope("/dumps").configure(dump::configure))
        .service(web::resource("/stats").route(web::get().to(get_stats)))
        .service(web::resource("/version").route(web::get().to(get_version)))
        .service(web::scope("/indexes").configure(indexes::configure))
        .service(web::scope("/multi-search").configure(multi_search::configure))
        .service(web::scope("/swap-indexes").configure(swap_indexes::configure));

    if enable_metrics {
        cfg.service(web::scope("/metrics").configure(metrics::configure));
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummarizedTaskView {
    task_uid: TaskId,
    index_uid: Option<String>,
    status: Status,
    #[serde(rename = "type")]
    kind: Kind,
    #[serde(serialize_with = "time::serde::rfc3339::serialize")]
    enqueued_at: OffsetDateTime,
}

impl From<Task> for SummarizedTaskView {
    fn from(task: Task) -> Self {
        SummarizedTaskView {
            task_uid: task.uid,
            index_uid: task.index_uid().map(|s| s.to_string()),
            status: task.status,
            kind: task.kind.as_kind(),
            enqueued_at: task.enqueued_at,
        }
    }
}

pub struct Pagination {
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaginationView<T> {
    pub results: Vec<T>,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
}

impl Pagination {
    /// Given the full data to paginate, returns the selected section.
    pub fn auto_paginate_sized<T>(
        self,
        content: impl IntoIterator<Item = T> + ExactSizeIterator,
    ) -> PaginationView<T>
    where
        T: Serialize,
    {
        let total = content.len();
        let content: Vec<_> = content.into_iter().skip(self.offset).take(self.limit).collect();
        self.format_with(total, content)
    }

    /// Given an iterator and the total number of elements, returns the selected section.
    pub fn auto_paginate_unsized<T>(
        self,
        total: usize,
        content: impl IntoIterator<Item = T>,
    ) -> PaginationView<T>
    where
        T: Serialize,
    {
        let content: Vec<_> = content.into_iter().skip(self.offset).take(self.limit).collect();
        self.format_with(total, content)
    }

    /// Given the data already paginated + the total number of elements, it stores
    /// everything in a [PaginationResult].
    pub fn format_with<T>(self, total: usize, results: Vec<T>) -> PaginationView<T>
    where
        T: Serialize,
    {
        PaginationView { results, offset: self.offset, limit: self.limit, total }
    }
}

impl<T> PaginationView<T> {
    pub fn new(offset: usize, limit: usize, total: usize, results: Vec<T>) -> Self {
        Self { offset, limit, results, total }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "name")]
pub enum UpdateType {
    ClearAll,
    Customs,
    DocumentsAddition {
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
    },
    DocumentsPartial {
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
    },
    DocumentsDeletion {
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<usize>,
    },
    Settings {
        settings: Settings<Unchecked>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessedUpdateResult {
    pub update_id: u64,
    #[serde(rename = "type")]
    pub update_type: UpdateType,
    pub duration: f64, // in seconds
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub processed_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedUpdateResult {
    pub update_id: u64,
    #[serde(rename = "type")]
    pub update_type: UpdateType,
    pub error: ResponseError,
    pub duration: f64, // in seconds
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub processed_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueuedUpdateResult {
    pub update_id: u64,
    #[serde(rename = "type")]
    pub update_type: UpdateType,
    #[serde(with = "time::serde::rfc3339")]
    pub enqueued_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none", with = "time::serde::rfc3339::option")]
    pub started_processing_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum UpdateStatusResponse {
    Enqueued {
        #[serde(flatten)]
        content: EnqueuedUpdateResult,
    },
    Processing {
        #[serde(flatten)]
        content: EnqueuedUpdateResult,
    },
    Failed {
        #[serde(flatten)]
        content: FailedUpdateResult,
    },
    Processed {
        #[serde(flatten)]
        content: ProcessedUpdateResult,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexUpdateResponse {
    pub update_id: u64,
}

impl IndexUpdateResponse {
    pub fn with_id(update_id: u64) -> Self {
        Self { update_id }
    }
}

/// Always return a 200 with:
/// ```json
/// {
///     "status": "Meilisearch is running"
/// }
/// ```
pub async fn running() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({ "status": "Meilisearch is running" }))
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub database_size: u64,
    #[serde(serialize_with = "time::serde::rfc3339::option::serialize")]
    pub last_update: Option<OffsetDateTime>,
    pub indexes: BTreeMap<String, indexes::IndexStats>,
}

async fn get_stats(
    index_scheduler: GuardedData<ActionPolicy<{ actions::STATS_GET }>, Data<IndexScheduler>>,
    auth_controller: GuardedData<ActionPolicy<{ actions::STATS_GET }>, Data<AuthController>>,
    req: HttpRequest,
    analytics: web::Data<dyn Analytics>,
) -> Result<HttpResponse, ResponseError> {
    analytics.publish("Stats Seen".to_string(), json!({ "per_index_uid": false }), Some(&req));
    let filters = index_scheduler.filters();

    let stats = create_all_stats((*index_scheduler).clone(), (*auth_controller).clone(), filters)?;

    debug!("returns: {:?}", stats);
    Ok(HttpResponse::Ok().json(stats))
}

pub fn create_all_stats(
    index_scheduler: Data<IndexScheduler>,
    auth_controller: Data<AuthController>,
    filters: &meilisearch_auth::AuthFilter,
) -> Result<Stats, ResponseError> {
    let mut last_task: Option<OffsetDateTime> = None;
    let mut indexes = BTreeMap::new();
    let mut database_size = 0;

    for index_uid in index_scheduler.index_names()? {
        // Accumulate the size of all indexes, even unauthorized ones, so
        // as to return a database_size representative of the correct database size on disk.
        // See <https://github.com/meilisearch/meilisearch/pull/3541#discussion_r1126747643> for context.
        let stats = index_scheduler.index_stats(&index_uid)?;
        database_size += stats.inner_stats.database_size;

        if !filters.is_index_authorized(&index_uid) {
            continue;
        }

        last_task = last_task.map_or(Some(stats.inner_stats.updated_at), |last| {
            Some(last.max(stats.inner_stats.updated_at))
        });
        indexes.insert(index_uid.to_string(), stats.into());
    }

    database_size += index_scheduler.size()?;
    database_size += auth_controller.size()?;
    database_size += index_scheduler.compute_update_file_size()?;

    let stats = Stats { database_size, last_update: last_task, indexes };
    Ok(stats)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionResponse {
    commit_sha: String,
    commit_date: String,
    pkg_version: String,
}

async fn get_version(
    _index_scheduler: GuardedData<ActionPolicy<{ actions::VERSION }>, Data<IndexScheduler>>,
    req: HttpRequest,
    analytics: web::Data<dyn Analytics>,
) -> HttpResponse {
    analytics.publish("Version Seen".to_string(), json!(null), Some(&req));

    let commit_sha = option_env!("VERGEN_GIT_SHA").unwrap_or("unknown");
    let commit_date = option_env!("VERGEN_GIT_COMMIT_TIMESTAMP").unwrap_or("unknown");

    HttpResponse::Ok().json(VersionResponse {
        commit_sha: commit_sha.to_string(),
        commit_date: commit_date.to_string(),
        pkg_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[derive(Serialize)]
struct KeysResponse {
    private: Option<String>,
    public: Option<String>,
}

pub async fn get_health(
    req: HttpRequest,
    index_scheduler: Data<IndexScheduler>,
    auth_controller: Data<AuthController>,
    analytics: web::Data<dyn Analytics>,
) -> Result<HttpResponse, ResponseError> {
    analytics.health_seen(&req);

    index_scheduler.health().unwrap();
    auth_controller.health().unwrap();

    Ok(HttpResponse::Ok().json(serde_json::json!({ "status": "available" })))
}
