use std::{collections::HashMap, path::Path, thread};

use async_trait::async_trait;
use zbus::{
    blocking::{Connection, Proxy},
    proxy::MethodFlags,
    zvariant::{OwnedObjectPath, OwnedValue},
};

use super::PackageBackend;
use crate::domain::{
    BackendCapabilities, BackendError, BackendKind, InstalledPackage, PackageId, RemovalPlan,
    RemovalRequest, RestartRequirement, TransactionEvent, TransactionStream, receiver_stream,
};

const SERVICE: &str = "org.freedesktop.PackageKit";
const ROOT_PATH: &str = "/org/freedesktop/PackageKit";
const ROOT_INTERFACE: &str = "org.freedesktop.PackageKit";
const TRANSACTION_INTERFACE: &str = "org.freedesktop.PackageKit.Transaction";

// PackageKit bitfields use 1 << enum value. These values are stable D-Bus ABI.
const ROLE_GET_DETAILS: u64 = 1 << 3;
const ROLE_GET_PACKAGES: u64 = 1 << 5;
const ROLE_REMOVE_PACKAGES: u64 = 1 << 14;
const ROLE_SEARCH_FILE: u64 = 1 << 19;
const FILTER_INSTALLED: u64 = 1 << 2;
const TRANSACTION_FLAG_NONE: u64 = 0;
const TRANSACTION_FLAG_SIMULATE: u64 = 1 << 2;

#[derive(Clone, Debug)]
pub struct PackageKitBackend {
    capabilities: BackendCapabilities,
}

#[derive(Default)]
struct TransactionResult {
    packages: Vec<InstalledPackage>,
    details: HashMap<String, PackageDetails>,
    restart: RestartRequirement,
    error: Option<BackendError>,
}

#[derive(Default)]
struct PackageDetails {
    summary: Option<String>,
    description: Option<String>,
    installed_size: Option<u64>,
}

impl PackageKitBackend {
    pub async fn detect() -> Result<Self, BackendError> {
        let capabilities = blocking::unblock(Self::detect_sync).await?;
        Ok(Self { capabilities })
    }

    fn detect_sync() -> Result<BackendCapabilities, BackendError> {
        let connection = Connection::system().map_err(dbus_error)?;
        let root =
            Proxy::new(&connection, SERVICE, ROOT_PATH, ROOT_INTERFACE).map_err(dbus_error)?;

        let major: u32 = root.get_property("VersionMajor").map_err(dbus_error)?;
        let minor: u32 = root.get_property("VersionMinor").map_err(dbus_error)?;
        let micro: u32 = root.get_property("VersionMicro").map_err(dbus_error)?;
        let version = format!("{major}.{minor}.{micro}");
        if (major, minor, micro) < (1, 3, 5) {
            return Err(BackendError::InsecureVersion { found: version });
        }

        let backend_name: String = root.get_property("BackendName").map_err(dbus_error)?;
        let roles: u64 = root.get_property("Roles").map_err(dbus_error)?;
        let can_list = supports_all_roles(roles, &[ROLE_GET_PACKAGES, ROLE_GET_DETAILS]);
        let can_remove = supports_all_roles(roles, &[ROLE_REMOVE_PACKAGES]);
        let can_search_files = supports_all_roles(roles, &[ROLE_SEARCH_FILE]);
        if !(can_list && can_remove && can_search_files) {
            return Err(BackendError::Unsupported);
        }

        Ok(BackendCapabilities {
            kind: BackendKind::PackageKit,
            backend_name: Some(backend_name),
            can_list,
            can_simulate_removal: can_remove,
            can_remove,
            diagnostic: None,
        })
    }

    fn root_proxy(connection: &Connection) -> Result<Proxy<'_>, BackendError> {
        Proxy::new(connection, SERVICE, ROOT_PATH, ROOT_INTERFACE).map_err(dbus_error)
    }

    fn transaction_proxy<'a>(
        connection: &'a Connection,
        path: &'a OwnedObjectPath,
    ) -> Result<Proxy<'a>, BackendError> {
        Proxy::new(connection, SERVICE, path.as_str(), TRANSACTION_INTERFACE).map_err(dbus_error)
    }

    fn new_transaction(connection: &Connection) -> Result<OwnedObjectPath, BackendError> {
        Self::root_proxy(connection)?
            .call("CreateTransaction", &())
            .map_err(dbus_error)
    }

    fn configure_transaction(proxy: &Proxy<'_>, interactive: bool) -> Result<(), BackendError> {
        let locale = std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into());
        let hints = vec![
            format!("locale={locale}"),
            format!("interactive={interactive}"),
            "background=false".to_owned(),
            "supports-plural-signals=true".to_owned(),
        ];
        proxy.call("SetHints", &(hints)).map_err(dbus_error)
    }

    fn run_transaction<F>(
        proxy: &Proxy<'_>,
        invoke: F,
        mut on_event: impl FnMut(TransactionEvent),
    ) -> Result<TransactionResult, BackendError>
    where
        F: FnOnce(&Proxy<'_>) -> Result<(), BackendError>,
    {
        let mut signals = proxy.receive_all_signals().map_err(dbus_error)?;
        invoke(proxy)?;
        let mut result = TransactionResult::default();

        for message in &mut signals {
            let message = message;
            let header = message.header();
            let member = header
                .member()
                .map(|member| member.as_str())
                .unwrap_or_default();
            match member {
                "Package" => {
                    let (_info, raw_id, summary): (u32, String, String) =
                        message.body().deserialize().map_err(dbus_error)?;
                    if let Ok(id) = PackageId::new(raw_id) {
                        result
                            .packages
                            .push(InstalledPackage::from_packagekit(id.clone(), Some(summary)));
                        on_event(TransactionEvent::PackageProgress { id, percentage: 0 });
                    }
                }
                "Packages" => {
                    let packages: Vec<(u32, String, String)> =
                        message.body().deserialize().map_err(dbus_error)?;
                    for (_info, raw_id, summary) in packages {
                        if let Ok(id) = PackageId::new(raw_id) {
                            result
                                .packages
                                .push(InstalledPackage::from_packagekit(id, Some(summary)));
                        }
                    }
                }
                "Details" => {
                    let values: HashMap<String, OwnedValue> =
                        message.body().deserialize().map_err(dbus_error)?;
                    if let Some(package_id) = variant_string(&values, "package_id") {
                        result.details.insert(
                            package_id,
                            PackageDetails {
                                summary: variant_string(&values, "summary"),
                                description: variant_string(&values, "detail"),
                                installed_size: variant_u64(&values, "size"),
                            },
                        );
                    }
                }
                "ItemProgress" => {
                    let (raw_id, _status, percentage): (String, u32, u32) =
                        message.body().deserialize().map_err(dbus_error)?;
                    if let Ok(id) = PackageId::new(raw_id) {
                        on_event(TransactionEvent::PackageProgress {
                            id,
                            percentage: percentage.min(100) as u8,
                        });
                    }
                }
                "RequireRestart" => {
                    let (kind, _package_id): (u32, String) =
                        message.body().deserialize().map_err(dbus_error)?;
                    let restart = restart_requirement(kind);
                    result.restart = result.restart.merge(restart);
                    on_event(TransactionEvent::RestartRequired(restart));
                }
                "ErrorCode" => {
                    let (code, details): (u32, String) =
                        message.body().deserialize().map_err(dbus_error)?;
                    result.error = Some(BackendError::PackageKit { code, details });
                }
                "Finished" => {
                    let (exit, _runtime): (u32, u32) =
                        message.body().deserialize().map_err(dbus_error)?;
                    if exit == 3 {
                        result.error = Some(BackendError::Cancelled);
                    }
                    break;
                }
                _ => {}
            }
        }

        if let Some(error) = result.error.clone() {
            return Err(error);
        }
        result.packages.sort_by(|a, b| a.id.cmp(&b.id));
        result.packages.dedup_by(|a, b| a.id == b.id);
        Ok(result)
    }

    fn list_installed_sync() -> Result<Vec<InstalledPackage>, BackendError> {
        let connection = Connection::system().map_err(dbus_error)?;
        let path = Self::new_transaction(&connection)?;
        let proxy = Self::transaction_proxy(&connection, &path)?;
        Self::configure_transaction(&proxy, false)?;
        let result = Self::run_transaction(
            &proxy,
            |proxy| {
                proxy
                    .call("GetPackages", &(FILTER_INSTALLED,))
                    .map_err(dbus_error)
            },
            |_| {},
        )?;
        let mut packages = result.packages;
        for chunk in packages.chunks_mut(128) {
            let ids: Vec<_> = chunk.iter().map(|package| package.id.clone()).collect();
            let details = Self::get_details_sync(&ids)?;
            let details: HashMap<_, _> = details
                .into_iter()
                .map(|package| (package.id.clone(), package))
                .collect();
            for package in chunk {
                if let Some(detail) = details.get(&package.id) {
                    *package = detail.clone();
                }
            }
        }
        Ok(packages)
    }

    fn get_details_sync(ids: &[PackageId]) -> Result<Vec<InstalledPackage>, BackendError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let connection = Connection::system().map_err(dbus_error)?;
        let path = Self::new_transaction(&connection)?;
        let proxy = Self::transaction_proxy(&connection, &path)?;
        Self::configure_transaction(&proxy, false)?;
        let raw_ids: Vec<_> = ids.iter().map(|id| id.as_str().to_owned()).collect();
        let result = Self::run_transaction(
            &proxy,
            |proxy| proxy.call("GetDetails", &(raw_ids,)).map_err(dbus_error),
            |_| {},
        )?;
        Ok(ids
            .iter()
            .map(|id| {
                let mut package = InstalledPackage::from_packagekit(id.clone(), None);
                if let Some(detail) = result.details.get(id.as_str()) {
                    package.summary = detail.summary.clone();
                    package.description = detail.description.clone();
                    package.installed_size = detail.installed_size;
                }
                package
            })
            .collect())
    }

    fn find_owner_sync(path: String) -> Result<Option<PackageId>, BackendError> {
        let connection = Connection::system().map_err(dbus_error)?;
        let transaction_path = Self::new_transaction(&connection)?;
        let proxy = Self::transaction_proxy(&connection, &transaction_path)?;
        Self::configure_transaction(&proxy, false)?;
        let result = Self::run_transaction(
            &proxy,
            |proxy| {
                proxy
                    .call("SearchFiles", &(FILTER_INSTALLED, vec![path]))
                    .map_err(dbus_error)
            },
            |_| {},
        )?;
        Ok(result.packages.into_iter().next().map(|package| package.id))
    }

    fn simulate_removal_sync(request: RemovalRequest) -> Result<RemovalPlan, BackendError> {
        request.validate()?;
        let connection = Connection::system().map_err(dbus_error)?;
        let path = Self::new_transaction(&connection)?;
        let proxy = Self::transaction_proxy(&connection, &path)?;
        Self::configure_transaction(&proxy, false)?;
        let ids: Vec<_> = request
            .package_ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect();
        let result = Self::run_transaction(
            &proxy,
            |proxy| {
                proxy
                    .call(
                        "RemovePackages",
                        &(TRANSACTION_FLAG_SIMULATE, ids, false, false),
                    )
                    .map_err(dbus_error)
            },
            |_| {},
        )?;
        let packages_to_remove = if result.packages.is_empty() {
            request
                .package_ids
                .iter()
                .map(|id| InstalledPackage::from_packagekit(id.clone(), None))
                .collect::<Vec<_>>()
        } else {
            let ids: Vec<_> = result
                .packages
                .iter()
                .map(|package| package.id.clone())
                .collect();
            Self::get_details_sync(&ids)?
        };
        let estimated_freed_bytes = packages_to_remove
            .iter()
            .map(|package| package.installed_size)
            .try_fold(0_u64, |total, size| size.map(|size| total + size));
        Ok(RemovalPlan {
            request,
            packages_to_remove,
            affected_applications: Vec::new(),
            estimated_freed_bytes,
            restart: result.restart,
            warnings: Vec::new(),
        })
    }

    fn execute_removal_sync(
        plan: RemovalPlan,
        sender: async_channel::Sender<TransactionEvent>,
    ) -> Result<(), BackendError> {
        let connection = Connection::system().map_err(dbus_error)?;
        let path = Self::new_transaction(&connection)?;
        let proxy = Self::transaction_proxy(&connection, &path)?;
        Self::configure_transaction(&proxy, true)?;
        let ids: Vec<_> = plan
            .request
            .package_ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect();
        let _ = sender.send_blocking(TransactionEvent::WaitingForAuthorization);
        let result = Self::run_transaction(
            &proxy,
            |proxy| {
                let _ = sender.send_blocking(TransactionEvent::Started);
                let reply: Option<()> = proxy
                    .call_with_flags(
                        "RemovePackages",
                        MethodFlags::AllowInteractiveAuth.into(),
                        &(TRANSACTION_FLAG_NONE, ids, false, false),
                    )
                    .map_err(dbus_error)?;
                let _ = reply;
                Ok(())
            },
            |event| {
                let _ = sender.send_blocking(event);
            },
        );
        match result {
            Ok(_) => {
                let _ = sender.send_blocking(TransactionEvent::OverallProgress(100));
                let _ = sender.send_blocking(TransactionEvent::Completed);
                Ok(())
            }
            Err(error) => {
                let _ = sender.send_blocking(TransactionEvent::Failed(error.clone()));
                Err(error)
            }
        }
    }
}

fn supports_all_roles(roles: u64, required: &[u64]) -> bool {
    required.iter().all(|role| roles & role != 0)
}

#[async_trait]
impl PackageBackend for PackageKitBackend {
    async fn capabilities(&self) -> BackendCapabilities {
        self.capabilities.clone()
    }

    async fn list_installed(&self) -> Result<Vec<InstalledPackage>, BackendError> {
        blocking::unblock(Self::list_installed_sync).await
    }

    async fn get_details(&self, ids: &[PackageId]) -> Result<Vec<InstalledPackage>, BackendError> {
        let ids = ids.to_vec();
        blocking::unblock(move || Self::get_details_sync(&ids)).await
    }

    async fn find_owner(&self, path: &Path) -> Result<Option<PackageId>, BackendError> {
        let path = path.to_string_lossy().into_owned();
        blocking::unblock(move || Self::find_owner_sync(path)).await
    }

    async fn simulate_removal(&self, request: RemovalRequest) -> Result<RemovalPlan, BackendError> {
        blocking::unblock(move || Self::simulate_removal_sync(request)).await
    }

    async fn remove(&self, confirmed_plan: RemovalPlan) -> Result<TransactionStream, BackendError> {
        let comparison = confirmed_plan.clone();
        let fresh =
            blocking::unblock(move || Self::simulate_removal_sync(comparison.request.clone()))
                .await?;
        if !confirmed_plan.equivalent_transaction(&fresh) {
            return Err(BackendError::PlanChanged);
        }

        let (sender, receiver) = async_channel::bounded(64);
        thread::Builder::new()
            .name("duck-packages-transaction".into())
            .spawn(move || {
                let _ = Self::execute_removal_sync(fresh, sender);
            })
            .map_err(|error| BackendError::Unavailable(error.to_string()))?;
        Ok(receiver_stream(receiver))
    }
}

fn restart_requirement(value: u32) -> RestartRequirement {
    // Stable PkRestartEnum order: none, application, session, system, security-session,
    // security-system. Security variants map to their corresponding scope.
    match value {
        2 => RestartRequirement::Application,
        3 | 5 => RestartRequirement::Session,
        4 | 6 => RestartRequirement::System,
        _ => RestartRequirement::None,
    }
}

fn dbus_error(error: impl std::fmt::Display) -> BackendError {
    let message = error.to_string();
    if message.to_lowercase().contains("not authorized")
        || message.to_lowercase().contains("authentication")
    {
        BackendError::AuthorizationDenied
    } else {
        BackendError::DBus(message)
    }
}

fn variant_string(values: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    values
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn variant_u64(values: &HashMap<String, OwnedValue>, key: &str) -> Option<u64> {
    values.get(key).and_then(|value| u64::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_roles_must_include_every_required_capability() {
        let roles = ROLE_GET_PACKAGES | ROLE_GET_DETAILS | ROLE_REMOVE_PACKAGES;
        assert!(supports_all_roles(
            roles,
            &[ROLE_GET_PACKAGES, ROLE_GET_DETAILS]
        ));
        assert!(!supports_all_roles(
            roles,
            &[ROLE_GET_PACKAGES, ROLE_SEARCH_FILE]
        ));
    }

    #[test]
    fn stable_packagekit_flags_preserve_safe_removal() {
        assert_eq!(FILTER_INSTALLED, 4);
        assert_eq!(TRANSACTION_FLAG_SIMULATE, 4);
        assert_eq!(TRANSACTION_FLAG_NONE, 0);
    }

    #[test]
    fn restart_levels_never_hide_a_system_restart() {
        assert_eq!(restart_requirement(4), RestartRequirement::System);
        assert_eq!(restart_requirement(6), RestartRequirement::System);
    }
}
