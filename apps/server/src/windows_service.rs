//! Runs Orbit as a native Windows Service and manages its registration with the Service
//! Control Manager (SCM). Compiled only on Windows (`main.rs` gates this module on
//! `#[cfg(windows)]`), so every item here can assume a real SCM is available.

use std::env;
use std::ffi::OsString;
use std::process::ExitCode;
use std::time::Duration;

use orbit_platform::Config;
use orbit_server::app::App;
use tokio_util::sync::CancellationToken;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "Orbit";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
/// Orbit's own main() watches for this one argument to dispatch to the Service Control Manager
/// instead of running the normal CLI. `install` registers it as the service's launch argument.
pub const RUN_ARG: &str = "__service-run";

/// True for the exact argv the Service Control Manager uses to start Orbit (`orbit <RUN_ARG>`).
pub fn is_dispatch_invocation(args: &[OsString]) -> bool {
    args.len() == 1 && args[0] == RUN_ARG
}

/// `windows_service::Error`'s `Display` omits the underlying Win32 message (for example "Access
/// is denied") for its most common variant, which is the only part that tells an operator what
/// to do (run as Administrator, re-run `install` with the service stopped, ...).
fn describe(error: &windows_service::Error) -> String {
    match std::error::Error::source(error) {
        Some(source) => format!("{error}: {source}"),
        None => error.to_string(),
    }
}

/// Registers Orbit with the Service Control Manager: automatic start, running as LocalSystem,
/// launching this same executable with [`RUN_ARG`].
pub fn install() -> Result<String, String> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|error| {
        format!(
            "opening the Service Control Manager: {} (run this from an elevated prompt)",
            describe(&error)
        )
    })?;
    let executable_path =
        env::current_exe().map_err(|error| format!("finding orbit.exe's own path: {error}"))?;
    let service = manager
        .create_service(
            &ServiceInfo {
                name: OsString::from(SERVICE_NAME),
                display_name: OsString::from("Orbit"),
                service_type: SERVICE_TYPE,
                start_type: ServiceStartType::AutoStart,
                error_control: ServiceErrorControl::Normal,
                executable_path,
                launch_arguments: vec![OsString::from(RUN_ARG)],
                dependencies: vec![],
                account_name: None,
                account_password: None,
            },
            ServiceAccess::CHANGE_CONFIG,
        )
        .map_err(|error| format!("creating the {SERVICE_NAME} service: {}", describe(&error)))?;
    service
        .set_description("Orbit: a self-hosted workspace for teams.")
        .map_err(|error| format!("setting the service description: {}", describe(&error)))?;
    Ok(format!(
        "installed the {SERVICE_NAME} service (start type: automatic, account: LocalSystem). \
         Set the ORBIT_* configuration as machine environment variables, then start it with: \
         sc start {SERVICE_NAME}"
    ))
}

/// Removes Orbit's registration with the Service Control Manager. Stops the service first if
/// it is running.
pub fn uninstall() -> Result<String, String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|error| {
            format!(
                "opening the Service Control Manager: {} (run this from an elevated prompt)",
                describe(&error)
            )
        })?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .map_err(|error| format!("opening the {SERVICE_NAME} service: {}", describe(&error)))?;
    service
        .delete()
        .map_err(|error| format!("removing the {SERVICE_NAME} service: {}", describe(&error)))?;
    let status = service.query_status().map_err(|error| {
        format!(
            "checking the {SERVICE_NAME} service status: {}",
            describe(&error)
        )
    })?;
    if status.current_state != ServiceState::Stopped {
        service.stop().map_err(|error| {
            format!("stopping the {SERVICE_NAME} service: {}", describe(&error))
        })?;
    }
    Ok(format!(
        "removed the {SERVICE_NAME} service registration (it stays until Windows finishes \
         stopping it)"
    ))
}

/// Blocks this thread for the life of the service, dispatching Service Control Manager events
/// to [`service_main`]. Returns only once the service has stopped. An `Err` here means Orbit
/// was not actually started by the Service Control Manager (for example, `orbit __service-run`
/// typed directly at a prompt).
pub fn dispatch() -> ExitCode {
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "orbit {RUN_ARG} must be started by the Windows Service Control Manager: {}. \
                 Install the service with 'orbit service install' and start it from Services, \
                 or run 'orbit serve' directly for a console session.",
                describe(&error)
            );
            ExitCode::FAILURE
        }
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    // No console and no stdout/stderr from here on: ORBIT_LOG_FILE is the only way to see
    // what Orbit does. A failure before tracing initializes cannot be reported anywhere.
    let format = if env::var("ORBIT_ENV").as_deref() == Ok("development") {
        orbit_platform::TracingFormat::Compact
    } else {
        orbit_platform::TracingFormat::Json
    };
    let log_file = env::var_os("ORBIT_LOG_FILE").map(std::path::PathBuf::from);
    let _ = orbit_platform::init_tracing(format, log_file.as_deref());

    if let Err(error) = run_service() {
        tracing::error!(%error, "the Orbit service stopped with an error");
    }
}

fn run_service() -> Result<(), String> {
    let shutdown = CancellationToken::new();
    let handler_shutdown = shutdown.clone();
    let event_handler = move |control_event| match control_event {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop | ServiceControl::Shutdown => {
            handler_shutdown.cancel();
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .map_err(|error| format!("registering the service control handler: {error}"))?;
    report_status(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        ServiceExitCode::NO_ERROR,
    )?;

    let outcome = run_server(shutdown, &status_handle);

    report_status(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        match &outcome {
            Ok(()) => ServiceExitCode::NO_ERROR,
            Err(_) => ServiceExitCode::Win32(1),
        },
    )?;
    outcome
}

fn run_server(
    shutdown: CancellationToken,
    status_handle: &windows_service::service_control_handler::ServiceStatusHandle,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("starting the Tokio runtime: {error}"))?;
    let outcome = runtime.block_on(async move {
        let config = Config::from_environment(env::vars().collect())
            .map_err(|error| format!("invalid configuration: {error}"))?;
        config
            .validate()
            .map_err(|error| format!("invalid configuration: {error}"))?;
        let app = App::build(config)
            .await
            .map_err(|error| format!("starting Orbit: {error}"))?;
        if let Some(url) = app.setup_url() {
            tracing::info!(%url, "initial setup URL");
        }
        report_status(
            status_handle,
            ServiceState::Running,
            ServiceControlAccept::STOP,
            ServiceExitCode::NO_ERROR,
        )?;
        app.run(shutdown)
            .await
            .map_err(|error| format!("Orbit stopped with an error: {error}"))
    });
    runtime.shutdown_timeout(Duration::ZERO);
    outcome
}

fn report_status(
    status_handle: &windows_service::service_control_handler::ServiceStatusHandle,
    state: ServiceState,
    controls_accepted: ServiceControlAccept,
    exit_code: ServiceExitCode,
) -> Result<(), String> {
    status_handle
        .set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: state,
            controls_accepted,
            exit_code,
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })
        .map_err(|error| format!("reporting {state:?} to the Service Control Manager: {error}"))
}
