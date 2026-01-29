use axum::{
    async_trait,
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use serde::{Deserialize, Serialize};
use jsonwebtoken::{encode, decode, Header, Validation, EncodingKey, DecodingKey};
use chrono::{Utc, Duration};
use argon2::{
    password_hash::{
        rand_core::OsRng,
        PasswordHash, PasswordHasher, PasswordVerifier, SaltString
    },
    Argon2
};

use crate::api::AppState;
use crate::models::{
    LoginRequest, CreateUserRequest, LoginResponse, UserResponse,
    VerifyEmailRequest, ResendVerificationRequest, MessageResponse
};

/// JWT claims
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: i64,  // user id
    pub username: String,
    pub exp: usize,
}

/// Authenticated user extracted from JWT
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
}

/// Error response
#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Extractor for authenticated user from JWT token
#[async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        // Extract Authorization header
        let TypedHeader(Authorization(bearer)) = TypedHeader::<Authorization<Bearer>>::from_request_parts(parts, state)
            .await
            .map_err(|_| {
                (StatusCode::UNAUTHORIZED, Json(ErrorResponse {
                    error: "Missing or invalid authorization header".to_string()
                })).into_response()
            })?;

        // Verify token
        let claims = verify_token(bearer.token(), &state.jwt_secret)
            .map_err(|e| {
                tracing::debug!("Token verification failed: {}", e);
                (StatusCode::UNAUTHORIZED, Json(ErrorResponse {
                    error: "Invalid or expired token".to_string()
                })).into_response()
            })?;

        Ok(AuthUser {
            id: claims.sub,
            username: claims.username,
        })
    }
}

/// Login handler - accepts email OR username
pub async fn login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, Response> {
    // Get user from database by email or username
    let user = state.db.get_user_by_identifier(&payload.identifier)
        .await
        .map_err(|e| {
            tracing::error!("Database error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Internal server error".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::UNAUTHORIZED, Json(ErrorResponse {
                error: "Invalid credentials".to_string()
            })).into_response()
        })?;

    // Check if email is verified
    if !user.email_verified {
        return Err((StatusCode::FORBIDDEN, Json(ErrorResponse {
            error: "Please verify your email before logging in. Check your inbox for the verification link.".to_string()
        })).into_response());
    }

    // Verify password
    let parsed_hash = PasswordHash::new(&user.password_hash)
        .map_err(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Internal server error".to_string()
            })).into_response()
        })?;

    Argon2::default()
        .verify_password(payload.password.as_bytes(), &parsed_hash)
        .map_err(|_| {
            (StatusCode::UNAUTHORIZED, Json(ErrorResponse {
                error: "Invalid credentials".to_string()
            })).into_response()
        })?;

    // Generate JWT token
    let expiration = Utc::now()
        .checked_add_signed(Duration::days(7))
        .expect("valid timestamp")
        .timestamp();

    let claims = Claims {
        sub: user.id,
        username: user.username.clone(),
        exp: expiration as usize,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret.as_bytes())
    ).map_err(|e| {
        tracing::error!("Failed to create token: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
            error: "Failed to create token".to_string()
        })).into_response()
    })?;

    Ok(Json(LoginResponse {
        token,
        user: UserResponse {
            id: user.id,
            username: user.username,
            email: user.email.unwrap_or_default(),
            email_verified: user.email_verified,
            created_at: user.created_at,
        }
    }))
}

/// Register handler - requires email, sends verification email
pub async fn register(
    State(state): State<AppState>,
    Json(payload): Json<CreateUserRequest>,
) -> Result<Json<MessageResponse>, Response> {
    // Validate email format
    if !payload.email.contains('@') || payload.email.len() < 5 {
        return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
            error: "Invalid email format".to_string()
        })).into_response());
    }

    // Validate username
    if payload.username.len() < 3 || payload.username.len() > 30 {
        return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
            error: "Username must be between 3 and 30 characters".to_string()
        })).into_response());
    }

    // Validate password
    if payload.password.len() < 8 {
        return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
            error: "Password must be at least 8 characters".to_string()
        })).into_response());
    }

    // Generate verification token
    let verification_token = uuid::Uuid::new_v4().to_string();
    let token_expires_at = Utc::now() + Duration::hours(24);

    // Hash password
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let password_hash = argon2.hash_password(payload.password.as_bytes(), &salt)
        .map_err(|e| {
            tracing::error!("Failed to hash password: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to create user".to_string()
            })).into_response()
        })?
        .to_string();

    // Create user
    let user = state.db.create_user(
        &payload.username,
        &payload.email,
        &password_hash,
        &verification_token,
        token_expires_at,
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to create user: {}", e);
        let error_str = e.to_string();
        if error_str.contains("UNIQUE") {
            if error_str.contains("email") {
                (StatusCode::CONFLICT, Json(ErrorResponse {
                    error: "An account with this email already exists".to_string()
                })).into_response()
            } else {
                (StatusCode::CONFLICT, Json(ErrorResponse {
                    error: "Username is already taken".to_string()
                })).into_response()
            }
        } else {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to create user".to_string()
            })).into_response()
        }
    })?;

    // Send verification email - registration fails if email can't be sent
    if let Some(ref email_service) = state.email_service {
        if let Err(e) = email_service.send_verification_email(&payload.email, &user.username, &verification_token) {
            tracing::error!("Failed to send verification email: {}", e);

            // Delete the user since we couldn't send verification email
            if let Err(delete_err) = state.db.delete_user(user.id).await {
                tracing::error!("Failed to rollback user creation: {}", delete_err);
            }

            return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to send verification email. Please try again later.".to_string()
            })).into_response());
        }
    } else {
        // No email service configured - delete user and return error
        if let Err(delete_err) = state.db.delete_user(user.id).await {
            tracing::error!("Failed to rollback user creation: {}", delete_err);
        }

        return Err((StatusCode::SERVICE_UNAVAILABLE, Json(ErrorResponse {
            error: "Email service is not configured. Registration is currently unavailable.".to_string()
        })).into_response());
    }

    Ok(Json(MessageResponse {
        message: "Registration successful! Please check your email to verify your account.".to_string()
    }))
}

/// Verify email handler
pub async fn verify_email(
    State(state): State<AppState>,
    Json(payload): Json<VerifyEmailRequest>,
) -> Result<Json<MessageResponse>, Response> {
    // Find user by verification token
    let user = state.db.get_user_by_verification_token(&payload.token)
        .await
        .map_err(|e| {
            tracing::error!("Database error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Internal server error".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::BAD_REQUEST, Json(ErrorResponse {
                error: "Invalid or expired verification token".to_string()
            })).into_response()
        })?;

    // Check if already verified
    if user.email_verified {
        return Ok(Json(MessageResponse {
            message: "Email is already verified. You can log in.".to_string()
        }));
    }

    // Check if token is expired
    if let Some(expires_at) = user.verification_token_expires_at {
        if Utc::now() > expires_at {
            return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
                error: "Verification token has expired. Please request a new one.".to_string()
            })).into_response());
        }
    }

    // Mark email as verified
    state.db.verify_user_email(user.id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to verify email: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to verify email".to_string()
            })).into_response()
        })?;

    tracing::info!("User {} verified their email", user.username);

    Ok(Json(MessageResponse {
        message: "Email verified successfully! You can now log in.".to_string()
    }))
}

/// Resend verification email handler
pub async fn resend_verification(
    State(state): State<AppState>,
    Json(payload): Json<ResendVerificationRequest>,
) -> Result<Json<MessageResponse>, Response> {
    // Generic success message to prevent email enumeration
    let success_message = "If an account exists with this email, a new verification link has been sent.";

    // Find user by email
    let user = match state.db.get_user_by_email(&payload.email).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            // Don't reveal if email exists
            return Ok(Json(MessageResponse {
                message: success_message.to_string()
            }));
        }
        Err(e) => {
            tracing::error!("Database error: {}", e);
            return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Internal server error".to_string()
            })).into_response());
        }
    };

    // Check if already verified
    if user.email_verified {
        return Ok(Json(MessageResponse {
            message: "Email is already verified. You can log in.".to_string()
        }));
    }

    // Generate new verification token
    let verification_token = uuid::Uuid::new_v4().to_string();
    let token_expires_at = Utc::now() + Duration::hours(24);

    // Update token in database
    state.db.update_verification_token(user.id, &verification_token, token_expires_at)
        .await
        .map_err(|e| {
            tracing::error!("Failed to update verification token: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to resend verification".to_string()
            })).into_response()
        })?;

    // Send verification email
    if let Some(ref email_service) = state.email_service {
        if let Err(e) = email_service.send_verification_email(&payload.email, &user.username, &verification_token) {
            tracing::error!("Failed to send verification email: {}", e);
            return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to send verification email".to_string()
            })).into_response());
        }
    } else {
        tracing::warn!("Email service not configured");
    }

    Ok(Json(MessageResponse {
        message: success_message.to_string()
    }))
}

/// Get current user info (requires authentication)
pub async fn me(
    State(state): State<AppState>,
    auth_user: AuthUser,
) -> Result<Json<UserResponse>, Response> {
    // Get full user info from database
    let user = state.db.get_user_by_id(auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Database error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Internal server error".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::NOT_FOUND, Json(ErrorResponse {
                error: "User not found".to_string()
            })).into_response()
        })?;

    Ok(Json(UserResponse {
        id: user.id,
        username: user.username,
        email: user.email.unwrap_or_default(),
        email_verified: user.email_verified,
        created_at: user.created_at,
    }))
}

/// Verify JWT token and extract claims
pub fn verify_token(token: &str, secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )?;

    Ok(token_data.claims)
}
