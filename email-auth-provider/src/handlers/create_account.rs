/// Create account handler, this is available on /create-account/ route
///
/// If a user does not exist for the given username and email, we create it.
///
/// It can happen that a user exists for the given email, but has no identity
/// (identity is empty string). This can happen if you have imported email
/// subscribers to the user table.
///
/// When importing, make sure that only unverified email is added in the
/// email provider data, nothing else. E.g. `data -> 'email'` should only
/// contain `{ "emails": ["email@being-imported.com"] }`, all other
/// subscriber data, e.g. if there is double opt-in, or the `name` of user,
/// `tags` for the user should be stored in any other `data` key (`data -> 'subscriptions')
#[ft_sdk::form]
pub fn create_account(
    mut conn: ft_sdk::Connection,
    ft_sdk::Form(payload): ft_sdk::Form<CreateAccountPayload>,
    ft_sdk::Cookie(sid): ft_sdk::Cookie<{ ft_sdk::auth::SESSION_KEY }>,
    // code can be invalid. eg: xyz
    ft_sdk::Query(code): ft_sdk::Query<"code", Option<String>>,
    host: ft_sdk::Host,
    config: email_auth::Config,
    app_url: ft_sdk::AppUrl,
) -> ft_sdk::form::Result {
    ft_sdk::println!("Config: {config:?}");
    let account_meta = validate(payload, &mut conn, &code)?;
    ft_sdk::println!("Account meta done for {}", account_meta.name);

    let uid = match account_meta.user_id.clone() {
        Some(uid) => {
            ft_sdk::auth::provider::update_user(
                &mut conn,
                email_auth::PROVIDER_ID,
                &uid,
                account_meta.to_provider_data(),
                true,
            )?;
            uid
        }
        None => ft_sdk::auth::provider::create_user(
            &mut conn,
            email_auth::PROVIDER_ID,
            account_meta.to_provider_data(),
        )?,
    };

    let ft_sdk::SessionID(sid) =
        ft_sdk::auth::provider::login(&mut conn, &uid, sid.map(ft_sdk::SessionID))?;

    ft_sdk::println!("Create User done for sid {sid}");

    if account_meta.pre_verified {
        return Ok(
            ft_sdk::form::redirect("/")?.with_cookie(common::session_cookie(sid.as_str(), host)?)
        );
    }

    let conf_link = confirmation_link(
        &account_meta.email_confirmation_code,
        &account_meta.email,
        &host,
        &app_url,
    );
    ft_sdk::println!("Confirmation link added {conf_link}");

    let from = email_from_address_from_env();
    ft_sdk::println!("Found email sender: {from:?}");

    if let Err(e) = ft_sdk::email::send(&ft_sdk::Email {
        from,
        to: vec![(account_meta.name.clone(), account_meta.email).into()],
        subject: "Confirm you account".to_string(),
        body_html: confirm_account_html_template(&account_meta.name, &conf_link),
        body_text: confirm_account_text_template(&account_meta.name, &conf_link),
        reply_to: None,
        cc: None,
        bcc: None,
        mkind: "auth_confirm_account_request".to_string(),
    }) {
        ft_sdk::println!("auth.wasm: failed to queue email: {:?}", e);
        return Err(e.into());
    }
    ft_sdk::println!("Email added to the queue");

    Ok(ft_sdk::form::redirect("/")?.with_cookie(common::session_cookie(sid.as_str(), host)?))
}

struct CreateAccount {
    email: String,
    #[cfg(feature = "username")]
    username: String,
    name: String,
    hashed_password: String,
    email_confirmation_code: String,
    user_id: Option<ft_sdk::UserId>,
    email_confirmation_sent_at: chrono::DateTime<chrono::Utc>,
    /// do not send a confirmation email or set a confirmation key in db if the user is
    /// `pre_verified`. This can be set by apps like subscription app.
    pre_verified: bool,
}

impl CreateAccount {
    fn to_provider_data(&self) -> ft_sdk::auth::ProviderData {
        let email_sent_at_in_nanos = self
            .email_confirmation_sent_at
            .timestamp_nanos_opt()
            .expect("unexpected out of range datetime");

        let mut res = ft_sdk::auth::ProviderData {
            #[cfg(feature = "username")]
            identity: self.username.to_string(),
            #[cfg(not(feature = "username"))]
            identity: self.email.to_string(),
            #[cfg(feature = "username")]
            username: Some(self.username.to_string()),
            #[cfg(not(feature = "username"))]
            username: None,
            name: Some(self.name.to_string()),
            emails: vec![self.email.clone()],
            verified_emails: vec![self.email.clone()],
            profile_picture: None,
            custom: serde_json::json!({
                "hashed_password": self.hashed_password,
            }),
        };

        if !self.pre_verified {
            res.custom = serde_json::json!({
                "hashed_password": self.hashed_password,
                email_auth::EMAIL_CONF_SENT_AT: email_sent_at_in_nanos,
                email_auth::EMAIL_CONF_CODE_KEY: self.email_confirmation_code,
            });
            res.verified_emails = vec![];
        }

        res
    }
}

fn validate(
    payload: CreateAccountPayload,
    conn: &mut ft_sdk::Connection,
    code: &Option<String>,
) -> Result<CreateAccount, ft_sdk::Error> {
    let mut errors = std::collections::HashMap::new();

    payload.validate(conn, &mut errors)?;

    if !errors.is_empty() {
        return Err(ft_sdk::SpecialError::Multi(errors).into());
    }

    use diesel::prelude::*;

    #[derive(diesel::QueryableByName)]
    #[diesel(table_name = ft_sdk::auth::fastn_user)]
    struct Identity {
        identity: Option<String>,
        id: i64,
    }

    let (query_result, pre_verified) = match code {
        Some(code) => (
            diesel::sql_query(
                r#"
            SELECT
                id, identity
            FROM fastn_user
            WHERE
                EXISTS (
                    SELECT 1
                    FROM json_each ( data -> 'subscription' -> 'confirmation-code')
                    WHERE value = $1
                )
                AND
                EXISTS (
                    SELECT 1
                    FROM json_each ( data -> 'email' -> 'emails')
                    WHERE value = $2
                )
            "#,
            )
            .bind::<diesel::sql_types::Text, _>(code)
            .bind::<diesel::sql_types::Text, _>(&payload.email)
            .get_result::<Identity>(conn),
            true,
        ),
        None => (
            diesel::sql_query(
                r#"
            SELECT
                id, identity
            FROM fastn_user
            WHERE
                EXISTS (
                    SELECT 1
                    FROM json_each ( data -> 'email' -> 'emails')
                    WHERE value = $1
                )
            "#,
            )
            .bind::<diesel::sql_types::Text, _>(&payload.email)
            .get_result::<Identity>(conn),
            false,
        ),
    };

    // check if the code is associated with a subscriber that is creating an account
    // if we find a user_id, it means the user is pre_verified
    let (user_id, pre_verified) = match query_result {
        Ok(identity) => {
            if identity.identity.is_some() {
                return Err(ft_sdk::single_error("email", "Email already exists.").into());
            }
            (Some(ft_sdk::auth::UserId(identity.id)), pre_verified)
        }
        Err(diesel::result::Error::NotFound) => (None, false),
        Err(e) => {
            return Err(e.into());
        }
    };

    Ok(CreateAccount {
        pre_verified,
        user_id,
        hashed_password: hashed_password(&payload.password),
        email: payload.email,
        name: payload.name,
        #[cfg(feature = "username")]
        username: payload.username,
        email_confirmation_code: generate_key(64),
        email_confirmation_sent_at: ft_sdk::env::now(),
    })
}

#[derive(serde::Deserialize)]
pub struct CreateAccountPayload {
    pub(crate) email: String,
    #[cfg(feature = "username")]
    pub(crate) username: String,
    pub(crate) name: String,
    pub(crate) password: String,
    pub(crate) password2: String,
    pub(crate) accept_terms: bool,
}

impl CreateAccountPayload {
    pub(crate) fn validate(
        &self,
        conn: &mut ft_sdk::Connection,
        errors: &mut std::collections::HashMap<String, String>,
    ) -> Result<(), ft_sdk::Error> {
        if !validator::ValidateEmail::validate_email(&self.email) {
            errors.insert("email".to_string(), "Invalid email format.".to_string());
        }

        if self.password != self.password2 {
            errors.insert(
                "password2".to_string(),
                "Password and Confirm password field do not match.".to_string(),
            );
        }

        if let Some(message) = is_strong_password(&self.password) {
            errors.insert("password".to_string(), message);
        }

        if !self.accept_terms {
            errors.insert(
                "accept_terms".to_string(),
                "You must accept the terms and conditions.".to_string(),
            );
        }

        #[cfg(feature = "username")]
        {
            common::validate_identity("username", &self.username, conn, errors)?;
        }

        #[cfg(not(feature = "username"))]
        {
            common::validate_identity("email", &self.email, conn, errors)?;
        }

        validate_verified_email(&self.email, conn, errors)?;

        Ok(())
    }
}

pub(crate) fn hashed_password(password: &str) -> String {
    let salt = argon2::password_hash::SaltString::generate(&mut ft_sdk::Rng {});
    let argon2 = argon2::Argon2::default();
    argon2::password_hash::PasswordHasher::hash_password(&argon2, password.as_bytes(), &salt)
        .unwrap()
        .to_string()
}

pub(crate) fn is_strong_password(password: &str) -> Option<String> {
    // TODO: better password validation
    if password.len() < 4 {
        return Some("password is too short".to_string());
    }

    None
}

fn validate_verified_email(
    email: &str,
    conn: &mut ft_sdk::Connection,
    errors: &mut std::collections::HashMap<String, String>,
) -> Result<(), ft_sdk::Error> {
    use diesel::prelude::*;

    if diesel::sql_query(
        r#"
        SELECT
            COUNT(*) AS count
        FROM fastn_user
        WHERE
            EXISTS (
                SELECT 1
                FROM json_each(data -> 'email' -> 'verified_emails')
                WHERE value = $1
            )
    "#,
    )
    .bind::<diesel::sql_types::Text, _>(email)
    .get_result::<ft_sdk::auth::Counter>(conn)?
    .count
        > 0
    {
        errors.insert("email".to_string(), "Email already exists.".to_string());
    }

    Ok(())
}

pub fn confirm_account_html_template(name: &str, link: &str) -> String {
    // TODO: until we figure out email templates, this has to do
    format!(
        r#"
            <html>
                <head>
                    <title>Confirm your account</title>
                </head>
                <body>
                    <h1>Hi {name},</h1>
                    <p>Click the link below to confirm your account</p>
                    <a href="{link}">Confirm your account</a>

                    In case you can't click the link, copy and paste the following link in your browser:
                    <br>
                    <a href="{link}">{link}</a>
                </body>
            </html>
            "#,
    )
}

pub fn confirm_account_text_template(name: &str, link: &str) -> String {
    format!(
        r#"
            Hi {name},

            Click the link below to confirm your account:

            {link}

            In case you can't click the link, copy and paste it in your browser.
            "#,
    )
}

pub fn generate_key(length: usize) -> String {
    ft_sdk::Rng::generate_key(length)
}

pub fn confirmation_link(
    key: &str,
    email: &str,
    ft_sdk::Host(host): &ft_sdk::Host,
    ft_sdk::AppUrl(app_url): &ft_sdk::AppUrl,
) -> String {
    format!(
        "https://{host}{app_url}{confirm_email_route}?code={key}&email={email}",
        confirm_email_route = email_auth::urls::Route::ConfirmEmail,
        app_url = app_url.as_ref().unwrap().trim_end_matches('/'),
    )
}

pub fn email_from_address_from_env() -> ft_sdk::EmailAddress {
    let email = ft_sdk::env::var("FASTN_SMTP_SENDER_EMAIL".to_string())
        .unwrap_or_else(|| "support@fifthtry.com".to_string());
    let name = Some(
        ft_sdk::env::var("FASTN_SMTP_SENDER_NAME".to_string())
            .unwrap_or_else(|| "FifthTry Team".to_string()),
    );

    ft_sdk::EmailAddress { name, email }
}
