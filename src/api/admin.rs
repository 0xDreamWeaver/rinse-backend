use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::api::{AppState, AuthUser, ErrorResponse, require_admin};
use crate::models::SystemStats;

/// Admin user response (includes fields not in regular UserResponse)
#[derive(Debug, Serialize)]
pub struct AdminUserResponse {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub has_avatar: bool,
}

/// Request body for updating a user's role
#[derive(Debug, Deserialize)]
pub struct UpdateRoleRequest {
    pub role: String,
}

/// GET /api/admin/stats — System statistics
pub async fn get_stats(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<SystemStats>, Response> {
    require_admin(&auth)?;

    let stats = state.db.get_system_stats()
        .await
        .map_err(|e| {
            tracing::error!("Failed to get system stats: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to get system stats".to_string(),
            })).into_response()
        })?;

    Ok(Json(stats))
}

/// GET /api/admin/users — List all users
pub async fn get_users(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<AdminUserResponse>>, Response> {
    require_admin(&auth)?;

    let users = state.db.get_all_users()
        .await
        .map_err(|e| {
            tracing::error!("Failed to get users: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to get users".to_string(),
            })).into_response()
        })?;

    let response: Vec<AdminUserResponse> = users.into_iter().map(|u| AdminUserResponse {
        id: u.id,
        username: u.username,
        email: u.email,
        role: u.role,
        created_at: u.created_at,
        has_avatar: u.avatar_path.is_some(),
    }).collect();

    Ok(Json(response))
}

/// PUT /api/admin/users/:id/role — Update a user's role
pub async fn update_user_role(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<i64>,
    Json(payload): Json<UpdateRoleRequest>,
) -> Result<Json<serde_json::Value>, Response> {
    require_admin(&auth)?;

    // Validate role value
    if !["admin", "user", "viewer"].contains(&payload.role.as_str()) {
        return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
            error: format!("Invalid role '{}'. Must be 'admin', 'user', or 'viewer'", payload.role),
        })).into_response());
    }

    // Prevent removing the last admin
    if payload.role != "admin" {
        // Check if the target user is currently an admin
        let target_user = state.db.get_user_by_id(user_id)
            .await
            .map_err(|e| {
                tracing::error!("Failed to get user: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                    error: "Failed to get user".to_string(),
                })).into_response()
            })?
            .ok_or_else(|| {
                (StatusCode::NOT_FOUND, Json(ErrorResponse {
                    error: "User not found".to_string(),
                })).into_response()
            })?;

        if target_user.role == "admin" {
            let admin_count = state.db.count_admins()
                .await
                .map_err(|e| {
                    tracing::error!("Failed to count admins: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                        error: "Failed to verify admin count".to_string(),
                    })).into_response()
                })?;

            if admin_count <= 1 {
                return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
                    error: "Cannot remove the last admin. Promote another user to admin first.".to_string(),
                })).into_response());
            }
        }
    }

    state.db.update_user_role(user_id, &payload.role)
        .await
        .map_err(|e| {
            tracing::error!("Failed to update user role: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to update user role".to_string(),
            })).into_response()
        })?;

    tracing::info!(
        "[Admin] User '{}' changed user_id={} role to '{}'",
        auth.username, user_id, payload.role
    );

    Ok(Json(serde_json::json!({
        "message": format!("User role updated to '{}'", payload.role),
        "user_id": user_id,
        "role": payload.role,
    })))
}
