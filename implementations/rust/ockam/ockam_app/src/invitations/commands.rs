use miette::IntoDiagnostic;
use std::net::SocketAddr;
use std::str::FromStr;
use tauri::{AppHandle, Manager, Runtime, State};
use tracing::{debug, info, trace, warn};

use ockam_api::address::get_free_address;
use ockam_api::cli_state::{CliState, StateDirTrait};
use ockam_api::cloud::project::Project;
use ockam_api::cloud::share::{AcceptInvitation, CreateServiceInvitation, InvitationWithAccess};
use ockam_api::cloud::share::{InvitationListKind, ListInvitations};
use ockam_api::nodes::models::portal::InletStatus;

use crate::app::{AppState, PROJECT_NAME};
use crate::cli::cli_bin;
use crate::projects::commands::{create_enrollment_ticket, SyncAdminProjectsState};
use crate::shared_service::relay::RELAY_NAME;

use super::{events::REFRESHED_INVITATIONS, state::SyncInvitationsState};

pub async fn accept_invitation<R: Runtime>(id: String, app: AppHandle<R>) -> Result<(), String> {
    accept_invitation_impl(id, &app)
        .await
        .map_err(|e| e.to_string())?;
    app.trigger_global(super::events::REFRESH_INVITATIONS, None);
    Ok(())
}

async fn accept_invitation_impl<R: Runtime>(id: String, app: &AppHandle<R>) -> crate::Result<()> {
    info!(?id, "accepting invitation");
    let state: State<'_, AppState> = app.state();
    let node_manager_worker = state.node_manager_worker().await;
    let res = node_manager_worker
        .accept_invitation(
            &state.context(),
            AcceptInvitation { id },
            &state.controller_address(),
            None,
        )
        .await?;
    debug!(?res);
    Ok(())
}

#[tauri::command]
pub async fn create_service_invitation<R: Runtime>(
    recipient_email: String,
    outlet_socket_addr: String,
    app: AppHandle<R>,
) -> Result<(), String> {
    info!(
        ?recipient_email,
        ?outlet_socket_addr,
        "creating service invitation"
    );
    let state: State<'_, SyncAdminProjectsState> = app.state();
    let projects = state.read().await;
    let project_id = projects
        .iter()
        .find(|p| p.name == *PROJECT_NAME)
        .map(|p| p.id.to_owned())
        .ok_or_else(|| "could not find default project".to_string())?;
    let enrollment_ticket = create_enrollment_ticket(project_id, app.clone())
        .await
        .map_err(|e| e.to_string())?;

    let socket_addr = SocketAddr::from_str(outlet_socket_addr.as_str())
        .into_diagnostic()
        .map_err(|e| format!("Cannot parse the outlet address as a socket address: {e}"))?;
    let invite_args = super::build_args_for_create_service_invitation(
        &app,
        &socket_addr,
        &recipient_email,
        enrollment_ticket,
    )
    .await
    .map_err(|e| e.to_string())?;

    // send the invitation asynchronously to avoid blocking the application waiting for a result
    let app_clone = app.clone();

    tauri::async_runtime::spawn(async move {
        let _ = send_invitation(invite_args, &app_clone).await;
        app_clone.trigger_global(super::events::REFRESH_INVITATIONS, None);
    });
    Ok(())
}

async fn send_invitation<R: Runtime>(
    invite_args: CreateServiceInvitation,
    app: &AppHandle<R>,
) -> crate::Result<()> {
    let state: State<'_, AppState> = app.state();
    let node_manager_worker = state.node_manager_worker().await;
    let res = node_manager_worker
        .create_service_invitation(
            &state.context(),
            invite_args,
            &state.controller_address(),
            None,
        )
        .await
        .map_err(|e| e.to_string());
    debug!(?res, "invitation sent");
    Ok(())
}

pub async fn refresh_invitations<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    debug!("Refreshing invitations");
    let state: State<'_, AppState> = app.state();
    if !state.is_enrolled().await.unwrap_or(false) {
        debug!("not enrolled, skipping invitations refresh");
        return Ok(());
    }
    let node_manager_worker = state.node_manager_worker().await;
    let invitations = node_manager_worker
        .list_shares(
            &state.context(),
            ListInvitations {
                kind: InvitationListKind::All,
            },
            &state.controller_address(),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    debug!("Invitations fetched");
    trace!(?invitations);
    {
        let invitation_state: State<'_, SyncInvitationsState> = app.state();
        let mut writer = invitation_state.write().await;
        writer.replace_by(invitations.clone());
    }
    refresh_inlets(&app, invitations.accepted.as_ref())
        .await
        .map_err(|e| e.to_string())?;
    app.trigger_global(REFRESHED_INVITATIONS, None);
    Ok(())
}

async fn refresh_inlets<R: Runtime>(
    app: &AppHandle<R>,
    accepted_invitations: Option<&Vec<InvitationWithAccess>>,
) -> crate::Result<()> {
    debug!("Refreshing inlets");
    let accepted_invitations = match accepted_invitations {
        Some(accepted_invitations) => accepted_invitations,
        None => {
            debug!("No accepted invitations, skipping inlets refresh");
            return Ok(());
        }
    };
    let app_state: State<'_, AppState> = app.state();
    let cli_state = app_state.state().await;
    let cli_bin = cli_bin()?;
    let mut inlets_socket_addrs = vec![];
    for invitation in accepted_invitations {
        match InletDataFromInvitation::new(&cli_state, invitation) {
            Ok(i) => match i {
                Some(i) => {
                    let mut inlet_is_running = false;
                    debug!(node = %i.local_node_name, "Checking node status");
                    if let Ok(node) = cli_state.nodes.get(&i.local_node_name) {
                        if node.is_running() {
                            debug!(node = %i.local_node_name, "Node already running");
                            debug!(node = %i.local_node_name, "Checking TCP inlet status");
                            if let Ok(cmd) = duct::cmd!(
                                &cli_bin,
                                "--no-input",
                                "tcp-inlet",
                                "show",
                                &i.service_name,
                                "--at",
                                &i.local_node_name,
                                "--output",
                                "json"
                            )
                            .env("OCKAM_LOG", "off")
                            .stderr_null()
                            .stdout_capture()
                            .run()
                            {
                                trace!(output = ?String::from_utf8_lossy(&cmd.stdout), "TCP inlet status");
                                let inlet: InletStatus = serde_json::from_slice(&cmd.stdout)?;
                                let inlet_socket_addr = SocketAddr::from_str(&inlet.bind_addr)?;
                                inlet_is_running = true;
                                debug!(
                                    at = ?inlet.bind_addr,
                                    alias = inlet.alias,
                                    "TCP inlet running"
                                );
                                inlets_socket_addrs
                                    .push((invitation.invitation.id.clone(), inlet_socket_addr));
                            }
                        }
                    }
                    if inlet_is_running {
                        continue;
                    }
                    debug!(node = %i.local_node_name, "Deleting node");
                    let _ = duct::cmd!(
                        &cli_bin,
                        "--no-input",
                        "node",
                        "delete",
                        "--yes",
                        &i.local_node_name
                    )
                    .stderr_null()
                    .stdout_capture()
                    .run();
                    match create_inlet(&i).await {
                        Ok(inlet_socket_addr) => {
                            inlets_socket_addrs
                                .push((invitation.invitation.id.clone(), inlet_socket_addr));
                        }
                        Err(err) => {
                            warn!(%err, node = %i.local_node_name, "Failed to create TCP inlet for accepted invitation");
                        }
                    }
                }
                None => {
                    warn!("Invalid invitation data");
                }
            },
            Err(err) => {
                warn!(%err, "Failed to parse invitation data");
            }
        }
    }
    {
        let invitations_state: State<'_, SyncInvitationsState> = app.state();
        let mut writer = invitations_state.write().await;
        for (invitation_id, inlet_socket_addr) in inlets_socket_addrs {
            writer
                .accepted
                .inlets
                .insert(invitation_id, inlet_socket_addr);
        }
    }
    info!("Inlets refreshed");
    Ok(())
}

/// Create the tcp-inlet for the accepted invitation
/// Returns the inlet SocketAddr
async fn create_inlet(inlet_data: &InletDataFromInvitation) -> crate::Result<SocketAddr> {
    debug!(service_name = ?inlet_data.service_name, "Creating TCP inlet for accepted invitation");
    let InletDataFromInvitation {
        local_node_name,
        service_name,
        service_route,
        enrollment_ticket_hex,
    } = inlet_data;
    let from = get_free_address()?;
    let from_str = from.to_string();
    let cli_bin = cli_bin()?;
    if let Some(enrollment_ticket_hex) = enrollment_ticket_hex {
        let _ = duct::cmd!(
            &cli_bin,
            "--no-input",
            "project",
            "enroll",
            "--new-trust-context-name",
            &local_node_name,
            &enrollment_ticket_hex,
        )
        .stderr_null()
        .stdout_capture()
        .run();
        debug!(node = %local_node_name, "Node enrolled using enrollment ticket");
    }
    duct::cmd!(
        &cli_bin,
        "--no-input",
        "node",
        "create",
        &local_node_name,
        "--trust-context",
        &local_node_name
    )
    .stderr_null()
    .stdout_capture()
    .run()?;
    debug!(node = %local_node_name, "Node created");
    duct::cmd!(
        &cli_bin,
        "--no-input",
        "tcp-inlet",
        "create",
        "--at",
        &local_node_name,
        "--from",
        &from_str,
        "--to",
        &service_route,
        "--alias",
        &service_name,
        "--retry-wait",
        "0",
    )
    .stderr_null()
    .stdout_capture()
    .run()?;
    info!(
        from = from_str,
        to = service_route,
        "Created tcp-inlet for accepted invitation"
    );
    Ok(from)
}

#[derive(Debug)]
struct InletDataFromInvitation {
    pub local_node_name: String,
    pub service_name: String,
    pub service_route: String,
    pub enrollment_ticket_hex: Option<String>,
}

impl InletDataFromInvitation {
    pub fn new(
        cli_state: &CliState,
        invitation: &InvitationWithAccess,
    ) -> crate::Result<Option<Self>> {
        match &invitation.service_access_details {
            Some(d) => {
                let service_name = d.service_name()?;
                let mut enrollment_ticket = d.enrollment_ticket()?;
                // The enrollment ticket contains the project data.
                // We need to replace the project name on the enrollment ticket with the project id,
                // so that, when using the enrollment ticket, there are no conflicts with the default project.
                // The node created when setting up the TCP inlet is meant to only serve that TCP inlet and
                // only has to resolve the `/project/{id}` project to create the needed secure-channel.
                if let Some(project) = enrollment_ticket.project.as_mut() {
                    project.name = project.id.clone();
                }
                let enrollment_ticket_hex = if invitation.invitation.is_expired()? {
                    None
                } else {
                    Some(enrollment_ticket.hex_encoded()?)
                };

                if let Some(project) = enrollment_ticket.project {
                    // At this point, the project name will be the project id.
                    let project = cli_state
                        .projects
                        .overwrite(project.name.clone(), Project::from(project.clone()))?;
                    assert_eq!(
                        project.name(),
                        project.id(),
                        "Project name should be the project id"
                    );

                    let project_id = project.id();
                    let local_node_name = format!("ockam_app_{project_id}_{service_name}");
                    let service_route = format!(
                        "/project/{project_id}/service/{}/secure/api/service/{service_name}",
                        *RELAY_NAME
                    );

                    Ok(Some(Self {
                        local_node_name,
                        service_name,
                        service_route,
                        enrollment_ticket_hex,
                    }))
                } else {
                    warn!(?invitation, "No project data found in enrollment ticket");
                    Ok(None)
                }
            }
            None => {
                warn!(
                    ?invitation,
                    "No service details found in accepted invitation"
                );
                Ok(None)
            }
        }
    }
}
