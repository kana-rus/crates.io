//! Functionality related to publishing a new crate or version of a crate.

use crate::auth::AuthCheck;
use crate::background_jobs::{Job, PRIORITY_RENDER_README};
use axum::body::Bytes;
use crates_io_tarball::{process_tarball, TarballError};
use diesel::dsl::{exists, select};
use hex::ToHex;
use hyper::body::Buf;
use sha2::{Digest, Sha256};
use tokio::runtime::Handle;
use url::Url;

use crate::controllers::cargo_prelude::*;
use crate::models::{
    insert_version_owner_action, Category, Crate, Keyword, NewCrate, NewVersion, Rights,
    VersionAction,
};

use crate::licenses::parse_license_expr;
use crate::middleware::log_request::RequestLogExt;
use crate::models::token::EndpointScope;
use crate::rate_limiter::LimitedAction;
use crate::schema::*;
use crate::sql::canon_crate_name;
use crate::util::errors::{cargo_err, internal, AppResult};
use crate::util::Maximums;
use crate::views::{
    EncodableCrate, EncodableCrateDependency, GoodCrate, PublishMetadata, PublishWarnings,
};

const MISSING_RIGHTS_ERROR_MESSAGE: &str = "this crate exists but you don't seem to be an owner. \
     If you believe this is a mistake, perhaps you need \
     to accept an invitation to be an owner before \
     publishing.";

const LICENSE_ERROR: &str = "unknown or invalid license expression; \
    see http://opensource.org/licenses for options, \
    and http://spdx.org/licenses/ for their identifiers";

/// Handles the `PUT /crates/new` route.
/// Used by `cargo publish` to publish a new crate or to publish a new version of an
/// existing crate.
///
/// Currently blocks the HTTP thread, perhaps some function calls can spawn new
/// threads and return completion or error through other methods  a `cargo publish
/// --status` command, via crates.io's front end, or email.
pub async fn publish(app: AppState, req: BytesRequest) -> AppResult<Json<GoodCrate>> {
    let (req, bytes) = req.0.into_parts();
    let (json_bytes, tarball_bytes) = split_body(bytes)?;

    let metadata: PublishMetadata = serde_json::from_slice(&json_bytes)
        .map_err(|e| cargo_err(&format_args!("invalid upload request: {e}")))?;

    let request_log = req.request_log();
    request_log.add("crate_name", &*metadata.name);
    request_log.add("crate_version", &*metadata.vers);

    conduit_compat(move || {
        let conn = &mut *app.db_write()?;

        // this query should only be used for the endpoint scope calculation
        // since a race condition there would only cause `publish-new` instead of
        // `publish-update` to be used.
        let existing_crate = Crate::by_name(&metadata.name)
            .first::<Crate>(conn)
            .optional()?;

        let endpoint_scope = match existing_crate {
            Some(_) => EndpointScope::PublishUpdate,
            None => EndpointScope::PublishNew,
        };

        let auth = AuthCheck::default()
            .with_endpoint_scope(endpoint_scope)
            .for_crate(&metadata.name)
            .check(&req, conn)?;

        let api_token_id = auth.api_token_id();
        let user = auth.user();

        let verified_email_address = user.verified_email(conn)?;
        let verified_email_address = verified_email_address.ok_or_else(|| {
            cargo_err(&format!(
                "A verified email address is required to publish crates to crates.io. \
             Visit https://{}/settings/profile to set and verify your email address.",
                app.config.domain_name,
            ))
        })?;

        // Use a different rate limit whether this is a new or an existing crate.
        let rate_limit_action = match existing_crate {
            Some(_) => LimitedAction::PublishUpdate,
            None => LimitedAction::PublishNew,
        };
        app.rate_limiter
            .check_rate_limit(user.id, rate_limit_action, conn)?;

        let content_length = tarball_bytes.len() as u64;

        let maximums = Maximums::new(
            existing_crate.as_ref().and_then(|c| c.max_upload_size),
            app.config.max_upload_size,
            app.config.max_unpack_size,
        );

        if content_length > maximums.max_upload_size {
            return Err(cargo_err(&format_args!(
                "max upload size is: {}",
                maximums.max_upload_size
            )));
        }

        let pkg_name = format!("{}-{}", &*metadata.name, &*metadata.vers);
        let tarball_info = process_tarball(&pkg_name, &*tarball_bytes, maximums.max_unpack_size)?;

        // `unwrap()` is safe here since `process_tarball()` validates that
        // we only accept manifests with a `package` section and without
        // inheritance.
        let package = tarball_info.manifest.package.unwrap();

        let description = package.description.map(|it| it.as_local().unwrap());
        let mut license = package.license.map(|it| it.as_local().unwrap());
        let license_file = package.license_file.map(|it| it.as_local().unwrap());
        let homepage = package.homepage.map(|it| it.as_local().unwrap());
        let documentation = package.documentation.map(|it| it.as_local().unwrap());
        let repository = package.repository.map(|it| it.as_local().unwrap());

        // Make sure required fields are provided
        fn empty(s: Option<&String>) -> bool {
            s.map_or(true, String::is_empty)
        }

        // It can have up to three elements per below conditions.
        let mut missing = Vec::with_capacity(3);
        if empty(description.as_ref()) {
            missing.push("description");
        }
        if empty(license.as_ref()) && empty(license_file.as_ref()) {
            missing.push("license");
        }
        if !missing.is_empty() {
            let message = missing_metadata_error_message(&missing);
            return Err(cargo_err(&message));
        }

        if let Some(ref license) = license {
            parse_license_expr(license).map_err(|_| cargo_err(LICENSE_ERROR))?;
        } else if license_file.is_some() {
            // If no license is given, but a license file is given, flag this
            // crate as having a nonstandard license. Note that we don't
            // actually do anything else with license_file currently.
            license = Some(String::from("non-standard"));
        }

        validate_url(homepage.as_deref(), "homepage")?;
        validate_url(documentation.as_deref(), "documentation")?;
        validate_url(repository.as_deref(), "repository")?;

        let keywords = package
            .keywords
            .map(|it| it.as_local().unwrap())
            .unwrap_or_default();

        if keywords.len() > 5 {
            return Err(cargo_err("expected at most 5 keywords per crate"));
        }

        for keyword in keywords.iter() {
            if keyword.len() > 20 {
                return Err(cargo_err(&format!(
                    "\"{keyword}\" is an invalid keyword (keywords must have less than 20 characters)"
                )));
            } else if !Keyword::valid_name(keyword) {
                return Err(cargo_err(&format!("\"{keyword}\" is an invalid keyword")));
            }
        }

        let categories = package
            .categories
            .map(|it| it.as_local().unwrap())
            .unwrap_or_default();

        if categories.len() > 5 {
            return Err(cargo_err("expected at most 5 categories per crate"));
        }

        // Create a transaction on the database, if there are no errors,
        // commit the transactions to record a new or updated crate.
        conn.transaction(|conn| {
            let name = metadata.name;
            let vers = &*metadata.vers;
            let features = metadata
                .features
                .into_iter()
                .map(|(k, v)| (k.0, v.into_iter().map(|v| v.0).collect()))
                .collect();
            let keywords = keywords.iter().map(|s| s.as_str()).collect::<Vec<_>>();
            let categories = categories.iter().map(|s| s.as_str()).collect::<Vec<_>>();

            // Persist the new crate, if it doesn't already exist
            let persist = NewCrate {
                name: &name,
                description: description.as_deref(),
                homepage: homepage.as_deref(),
                documentation: documentation.as_deref(),
                readme: metadata.readme.as_deref(),
                repository: repository.as_deref(),
                max_upload_size: None,
            };

            if is_reserved_name(persist.name, conn)? {
                return Err(cargo_err("cannot upload a crate with a reserved name"));
            }

            // To avoid race conditions, we try to insert
            // first so we know whether to add an owner
            let krate = match persist.create(conn, user.id).optional()? {
                Some(krate) => krate,
                None => persist.update(conn)?,
            };

            let owners = krate.owners(conn)?;
            if user.rights(&app, &owners)? < Rights::Publish {
                return Err(cargo_err(MISSING_RIGHTS_ERROR_MESSAGE));
            }

            if krate.name != *name {
                return Err(cargo_err(&format_args!(
                    "crate was previously named `{}`",
                    krate.name
                )));
            }

            if let Some(daily_version_limit) = app.config.new_version_rate_limit {
                let published_today = count_versions_published_today(krate.id, conn)?;
                if published_today >= daily_version_limit as i64 {
                    return Err(cargo_err(
                        "You have published too many versions of this crate in the last 24 hours",
                    ));
                }
            }

            // Read tarball from request
            let hex_cksum: String = Sha256::digest(&tarball_bytes).encode_hex();

            let rust_version = package.rust_version.map(|rv| rv.as_local().unwrap());

            // Persist the new version of this crate
            let version = NewVersion::new(
                krate.id,
                vers,
                &features,
                license,
                // Downcast is okay because the file length must be less than the max upload size
                // to get here, and max upload sizes are way less than i32 max
                content_length as i32,
                user.id,
                hex_cksum,
                package.links,
                rust_version,
            )?
            .save(conn, &verified_email_address)?;

            insert_version_owner_action(
                conn,
                version.id,
                user.id,
                api_token_id,
                VersionAction::Publish,
            )?;

            // Link this new version to all dependencies
            add_dependencies(conn, &metadata.deps, version.id)?;

            // Update all keywords for this crate
            Keyword::update_crate(conn, &krate, &keywords)?;

            // Update all categories for this crate, collecting any invalid categories
            // in order to be able to warn about them
            let ignored_invalid_categories = Category::update_crate(conn, &krate, &categories)?;

            let top_versions = krate.top_versions(conn)?;

            let pkg_path_in_vcs = tarball_info.vcs_info.map(|info| info.path_in_vcs);

            if let Some(readme) = metadata.readme {
                if !readme.is_empty() {
                    Job::render_and_upload_readme(
                        version.id,
                        readme,
                        metadata
                            .readme_file
                            .unwrap_or_else(|| String::from("README.md")),
                        repository,
                        pkg_path_in_vcs,
                    )
                    .enqueue_with_priority(conn, PRIORITY_RENDER_README)?;
                }
            }

            // Upload crate tarball
            Handle::current()
                .block_on(app.storage.upload_crate_file(
                    &krate.name,
                    &vers.to_string(),
                    tarball_bytes,
                ))
                .map_err(|e| internal(format!("failed to upload crate: {e}")))?;

            Job::enqueue_sync_to_index(&krate.name, conn)?;

            // The `other` field on `PublishWarnings` was introduced to handle a temporary warning
            // that is no longer needed. As such, crates.io currently does not return any `other`
            // warnings at this time, but if we need to, the field is available.
            let warnings = PublishWarnings {
                invalid_categories: ignored_invalid_categories,
                invalid_badges: vec![],
                other: vec![],
            };

            Ok(Json(GoodCrate {
                krate: EncodableCrate::from_minimal(krate, Some(&top_versions), None, false, None),
                warnings,
            }))
        })
    })
    .await
}

/// Counts the number of versions for `krate_id` that were published within
/// the last 24 hours.
fn count_versions_published_today(krate_id: i32, conn: &mut PgConnection) -> QueryResult<i64> {
    use crate::schema::versions::dsl::*;
    use diesel::dsl::{now, IntervalDsl};

    versions
        .filter(crate_id.eq(krate_id))
        .filter(created_at.gt(now - 24.hours()))
        .count()
        .get_result(conn)
}

#[instrument(skip_all)]
fn split_body(mut bytes: Bytes) -> AppResult<(Bytes, Bytes)> {
    // The format of the req.body() of a publish request is as follows:
    //
    // metadata length
    // metadata in JSON about the crate being published
    // .crate tarball length
    // .crate tarball file

    if bytes.len() < 4 {
        // Avoid panic in `get_u32_le()` if there is not enough remaining data
        return Err(cargo_err("invalid metadata length"));
    }

    let json_len = bytes.get_u32_le() as usize;
    if json_len > bytes.len() {
        return Err(cargo_err(&format!(
            "invalid metadata length for remaining payload: {json_len}"
        )));
    }

    let json_bytes = bytes.split_to(json_len);

    if bytes.len() < 4 {
        // Avoid panic in `get_u32_le()` if there is not enough remaining data
        return Err(cargo_err("invalid tarball length"));
    }

    let tarball_len = bytes.get_u32_le() as usize;
    if tarball_len > bytes.len() {
        return Err(cargo_err(&format!(
            "invalid tarball length for remaining payload: {tarball_len}"
        )));
    }

    let tarball_bytes = bytes.split_to(tarball_len);

    Ok((json_bytes, tarball_bytes))
}

fn is_reserved_name(name: &str, conn: &mut PgConnection) -> QueryResult<bool> {
    select(exists(reserved_crate_names::table.filter(
        canon_crate_name(reserved_crate_names::name).eq(canon_crate_name(name)),
    )))
    .get_result(conn)
}

fn validate_url(url: Option<&str>, field: &str) -> AppResult<()> {
    let Some(url) = url else {
        return Ok(());
    };

    // Manually check the string, as `Url::parse` may normalize relative URLs
    // making it difficult to ensure that both slashes are present.
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(cargo_err(&format_args!(
            "URL for field `{field}` must begin with http:// or https:// (url: {url})"
        )));
    }

    // Ensure the entire URL parses as well
    Url::parse(url)
        .map_err(|_| cargo_err(&format_args!("`{field}` is not a valid url: `{url}`")))?;
    Ok(())
}

fn missing_metadata_error_message(missing: &[&str]) -> String {
    format!(
        "missing or empty metadata fields: {}. Please \
         see https://doc.rust-lang.org/cargo/reference/manifest.html for \
         how to upload metadata",
        missing.join(", ")
    )
}

#[instrument(skip_all)]
pub fn add_dependencies(
    conn: &mut PgConnection,
    deps: &[EncodableCrateDependency],
    target_version_id: i32,
) -> AppResult<()> {
    use self::dependencies::dsl::*;
    use diesel::insert_into;

    let new_dependencies = deps
        .iter()
        .map(|dep| {
            if let Some(registry) = &dep.registry {
                if !registry.is_empty() {
                    return Err(cargo_err(&format_args!("Dependency `{}` is hosted on another registry. Cross-registry dependencies are not permitted on crates.io.", &*dep.name)));
                }
            }

            // Match only identical names to ensure the index always references the original crate name
            let krate:Crate = Crate::by_exact_name(&dep.name)
                .first(conn)
                .map_err(|_| cargo_err(&format_args!("no known crate named `{}`", &*dep.name)))?;

            if let Ok(version_req) = semver::VersionReq::parse(&dep.version_req.0) {
                 if version_req == semver::VersionReq::STAR {
                    return Err(cargo_err(&format_args!("wildcard (`*`) dependency constraints are not allowed \
                        on crates.io. Crate with this problem: `{}` See https://doc.rust-lang.org/cargo/faq.html#can-\
                        libraries-use--as-a-version-for-their-dependencies for more \
                        information", &*dep.name)));
                }
            }

            Ok((
                version_id.eq(target_version_id),
                crate_id.eq(krate.id),
                req.eq(dep.version_req.to_string()),
                dep.kind.map(|k| kind.eq(k as i32)),
                optional.eq(dep.optional),
                default_features.eq(dep.default_features),
                features.eq(&dep.features),
                target.eq(dep.target.as_deref()),
                explicit_name.eq(dep.explicit_name_in_toml.as_deref())
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;

    insert_into(dependencies)
        .values(&new_dependencies)
        .execute(conn)?;

    Ok(())
}

impl From<TarballError> for BoxedAppError {
    fn from(error: TarballError) -> Self {
        match error {
            TarballError::Malformed(err) => err.chain(cargo_err(
                "uploaded tarball is malformed or too large when decompressed",
            )),
            TarballError::InvalidPath(path) => cargo_err(&format!("invalid path found: {path}")),
            TarballError::UnexpectedSymlink(path) => {
                cargo_err(&format!("unexpected symlink or hard link found: {path}"))
            }
            TarballError::IO(err) => err.into(),
            TarballError::MissingManifest => {
                cargo_err("uploaded tarball is missing a `Cargo.toml` manifest file")
            }
            TarballError::IncorrectlyCasedManifest(name) => {
                cargo_err(&format!(
                    "uploaded tarball is missing a `Cargo.toml` manifest file; `{name}` was found, but must be named `Cargo.toml` with that exact casing",
                    name = name.to_string_lossy(),
                ))
            }
            TarballError::TooManyManifests(paths) => {
                let paths = paths
                    .into_iter()
                    .map(|path| {
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect::<Vec<_>>()
                    .join("`, `");
                cargo_err(&format!(
                    "uploaded tarball contains more than one `Cargo.toml` manifest file; found `{paths}`"
                ))
            }
            TarballError::InvalidManifest(err) => cargo_err(&format!(
                "failed to parse `Cargo.toml` manifest file\n\n{err}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{missing_metadata_error_message, validate_url};

    #[test]
    fn deny_relative_urls() {
        assert_err!(validate_url(Some("https:/example.com/home"), "homepage"));
    }

    #[test]
    fn missing_metadata_error_message_test() {
        assert_eq!(missing_metadata_error_message(&["a"]), "missing or empty metadata fields: a. Please see https://doc.rust-lang.org/cargo/reference/manifest.html for how to upload metadata");
        assert_eq!(missing_metadata_error_message(&["a", "b"]), "missing or empty metadata fields: a, b. Please see https://doc.rust-lang.org/cargo/reference/manifest.html for how to upload metadata");
        assert_eq!(missing_metadata_error_message(&["a", "b", "c"]), "missing or empty metadata fields: a, b, c. Please see https://doc.rust-lang.org/cargo/reference/manifest.html for how to upload metadata");
    }
}
