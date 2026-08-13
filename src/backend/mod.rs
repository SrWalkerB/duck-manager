mod diagnostic;
mod packagekit;

use std::{path::Path, sync::Arc};

use async_trait::async_trait;

use crate::domain::{
    BackendCapabilities, BackendError, InstalledPackage, PackageId, RemovalPlan, RemovalRequest,
    TransactionStream,
};

pub use diagnostic::DiagnosticBackend;
pub use packagekit::PackageKitBackend;

#[async_trait]
pub trait PackageBackend: Send + Sync {
    async fn capabilities(&self) -> BackendCapabilities;
    async fn list_installed(&self) -> Result<Vec<InstalledPackage>, BackendError>;
    async fn get_details(&self, ids: &[PackageId]) -> Result<Vec<InstalledPackage>, BackendError>;
    async fn find_owner(&self, path: &Path) -> Result<Option<PackageId>, BackendError>;
    async fn simulate_removal(&self, request: RemovalRequest) -> Result<RemovalPlan, BackendError>;
    async fn remove(&self, confirmed_plan: RemovalPlan) -> Result<TransactionStream, BackendError>;
}

pub struct BackendFactory;

impl BackendFactory {
    pub async fn detect() -> Arc<dyn PackageBackend> {
        match PackageKitBackend::detect().await {
            Ok(backend) => Arc::new(backend),
            Err(error) => Arc::new(DiagnosticBackend::new(error.to_string())),
        }
    }
}
