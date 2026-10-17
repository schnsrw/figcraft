//! The relay's check that a batch may refer to an image (TS-009, SAAS-ADR-017).
//!
//! A document's collaboration [`Server`](figcraft_collab::Server) accepts an image reference only
//! when its [`AssetDirectory`] says the tenant holds that verified asset with exactly that size.
//! [`CloudAssets`] is the directory of one open document: it asks the database in the document's
//! tenant (`claim_document_asset`), so another tenant's identical bytes are invisible, and in the
//! same transaction attaches the asset to the document, so the reference keeps it from being
//! collected even when it was uploaded for another document. A storage failure or timeout answers
//! "not found": the batch is refused with a reason and the client tries again.
//!
//! The answer is asked on the hub thread, like the backend's other storage calls, bounded by the
//! backend's storage timeout. Assets that were found once are remembered for the life of the room
//! (an attached asset is not collected while its document exists).

use std::collections::HashSet;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, TenantId};
use figcraft_cloud_storage::assets::{claim_document_asset, parse_sha256_hex};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_collab::AssetDirectory;
use sqlx::PgPool;
use tokio::runtime::Handle;

/// How many found assets a room remembers.
const MAX_REMEMBERED: usize = 4096;

/// The asset directory of one open cloud document.
pub struct CloudAssets {
    pool: PgPool,
    runtime: Handle,
    tenant: TenantId,
    document: CloudDocumentId,
    timeout: Duration,
    found: Mutex<HashSet<(String, u64)>>,
}

impl CloudAssets {
    #[must_use]
    pub fn new(pool: PgPool, runtime: Handle, tenant: TenantId, document: CloudDocumentId, timeout: Duration) -> Self {
        Self { pool, runtime, tenant, document, timeout, found: Mutex::new(HashSet::new()) }
    }

    async fn claim(&self, sha256: [u8; 32], len: u64) -> bool {
        let work = async {
            let mut transaction = begin_tenant_transaction(&self.pool, self.tenant).await.ok()?;
            let claimed = claim_document_asset(&mut transaction, self.document, &sha256, len).await.ok()?;
            if claimed {
                transaction.commit().await.ok()?;
            }
            Some(claimed)
        };
        matches!(tokio::time::timeout(self.timeout, work).await, Ok(Some(true)))
    }
}

impl fmt::Debug for CloudAssets {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("CloudAssets").field("document", &self.document).finish_non_exhaustive()
    }
}

impl AssetDirectory for CloudAssets {
    fn verified(&self, hash: &str, len: u64) -> bool {
        let key = (hash.to_string(), len);
        if self.found.lock().unwrap_or_else(PoisonError::into_inner).contains(&key) {
            return true;
        }
        let Some(sha256) = parse_sha256_hex(hash) else { return false };
        if !self.runtime.block_on(self.claim(sha256, len)) {
            return false;
        }
        let mut found = self.found.lock().unwrap_or_else(PoisonError::into_inner);
        if found.len() >= MAX_REMEMBERED {
            found.clear();
        }
        found.insert(key);
        true
    }
}
