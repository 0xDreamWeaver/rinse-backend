use anyhow::{Context, Result};
use lettre::{
    message::{header::ContentType, Mailbox},
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
    },
    Message, SmtpTransport, Transport,
};

/// Email service for sending verification emails
#[derive(Clone)]
pub struct EmailService {
    smtp_host: String,
    smtp_port: u16,
    smtp_username: String,
    smtp_password: String,
    from_email: String,
    from_name: String,
    app_url: String,
}

impl EmailService {
    pub fn new(
        smtp_host: String,
        smtp_port: u16,
        smtp_username: String,
        smtp_password: String,
        from_email: String,
        from_name: String,
        app_url: String,
    ) -> Self {
        Self {
            smtp_host,
            smtp_port,
            smtp_username,
            smtp_password,
            from_email,
            from_name,
            app_url,
        }
    }

    /// Send a verification email to a new user
    pub fn send_verification_email(
        &self,
        to_email: &str,
        username: &str,
        token: &str,
    ) -> Result<()> {
        let verification_link = format!("{}/verify-email?token={}", self.app_url, token);

        let html_body = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <style>
        body {{
            font-family: 'Courier New', Consolas, monospace;
            background-color: #0a0a0a;
            color: #e0e0e0;
            margin: 0;
            padding: 20px;
        }}
        .container {{
            max-width: 600px;
            margin: 0 auto;
            background-color: #111111;
            border: 1px solid #00ff00;
            padding: 40px;
        }}
        .logo {{
            font-size: 64px;
            text-align: center;
            color: #00ff00;
            font-weight: bold;
            margin-bottom: 20px;
            text-shadow: 0 0 10px rgba(0, 255, 0, 0.5);
        }}
        h1 {{
            color: #00ff00;
            font-size: 24px;
            margin-bottom: 20px;
            text-align: center;
        }}
        p {{
            line-height: 1.6;
            margin-bottom: 16px;
            color: #b0b0b0;
        }}
        .button-container {{
            text-align: center;
            margin: 32px 0;
        }}
        .button {{
            display: inline-block;
            padding: 14px 32px;
            background-color: #00ff00;
            color: #0a0a0a;
            text-decoration: none;
            font-weight: bold;
            font-family: 'Courier New', monospace;
            font-size: 14px;
            letter-spacing: 1px;
        }}
        .button:hover {{
            background-color: #00cc00;
        }}
        .link-fallback {{
            background-color: #1a1a1a;
            padding: 12px;
            margin: 20px 0;
            word-break: break-all;
            font-size: 12px;
            color: #00ff00;
            border: 1px solid #333;
        }}
        .footer {{
            margin-top: 40px;
            padding-top: 20px;
            border-top: 1px solid #333;
            font-size: 12px;
            color: #666;
            text-align: center;
        }}
        .expire-notice {{
            color: #ffaa00;
            font-size: 13px;
        }}
    </style>
</head>
<body>
    <div class="container">
        <div class="logo">R</div>
        <h1>Verify Your Email</h1>
        <p>Hello <strong style="color: #00ff00;">{username}</strong>,</p>
        <p>Welcome to Rinse! To complete your registration and start using the application, please verify your email address by clicking the button below:</p>
        <div class="button-container">
            <a href="{verification_link}" class="button">VERIFY EMAIL</a>
        </div>
        <p>Or copy and paste this link into your browser:</p>
        <div class="link-fallback">{verification_link}</div>
        <p class="expire-notice">This verification link will expire in 24 hours.</p>
        <div class="footer">
            <p>If you didn't create an account on Rinse, you can safely ignore this email.</p>
            <p style="margin-top: 10px; color: #444;">Rinse - Soulseek Download Manager</p>
        </div>
    </div>
</body>
</html>"#,
            username = username,
            verification_link = verification_link,
        );

        let plain_body = format!(
            r#"Hello {username},

Welcome to Rinse! To complete your registration, please verify your email address by visiting this link:

{verification_link}

This link will expire in 24 hours.

If you didn't create an account on Rinse, you can safely ignore this email.

--
Rinse - Soulseek Download Manager"#,
            username = username,
            verification_link = verification_link,
        );

        let from_mailbox: Mailbox = format!("{} <{}>", self.from_name, self.from_email)
            .parse()
            .context("Invalid from email address")?;

        let to_mailbox: Mailbox = to_email
            .parse()
            .context("Invalid recipient email address")?;

        let email = Message::builder()
            .from(from_mailbox)
            .to(to_mailbox)
            .subject("Verify your Rinse account")
            .multipart(
                lettre::message::MultiPart::alternative()
                    .singlepart(
                        lettre::message::SinglePart::builder()
                            .header(ContentType::TEXT_PLAIN)
                            .body(plain_body),
                    )
                    .singlepart(
                        lettre::message::SinglePart::builder()
                            .header(ContentType::TEXT_HTML)
                            .body(html_body),
                    ),
            )
            .context("Failed to build email message")?;

        let creds = Credentials::new(self.smtp_username.clone(), self.smtp_password.clone());

        tracing::debug!(
            "Connecting to SMTP: {}:{} as {}",
            self.smtp_host,
            self.smtp_port,
            self.smtp_username
        );

        // Use implicit TLS (port 465) or STARTTLS (port 587) based on port
        let mailer = if self.smtp_port == 465 {
            // Implicit TLS - connection is encrypted from the start
            let tls_parameters = TlsParameters::new(self.smtp_host.clone())
                .context("Failed to create TLS parameters")?;

            SmtpTransport::relay(&self.smtp_host)
                .context("Failed to create SMTP transport")?
                .port(self.smtp_port)
                .tls(Tls::Wrapper(tls_parameters))
                .credentials(creds)
                .build()
        } else {
            // STARTTLS - connection starts plain, then upgrades to TLS
            SmtpTransport::starttls_relay(&self.smtp_host)
                .context("Failed to create SMTP transport")?
                .port(self.smtp_port)
                .credentials(creds)
                .build()
        };

        match mailer.send(&email) {
            Ok(_) => {
                tracing::info!("Verification email sent to {}", to_email);
                Ok(())
            }
            Err(e) => {
                tracing::error!("SMTP error details: {:?}", e);
                Err(anyhow::anyhow!("Failed to send email: {}", e))
            }
        }
    }
}
