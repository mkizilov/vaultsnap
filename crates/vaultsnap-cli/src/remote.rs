//! Opening the repository the user named: a directory, or an S3-compatible bucket given as
//! `s3://BUCKET[/FOLDER]`.
//!
//! Credentials for a bucket are not handled here, and there is no option to give any: the AWS
//! credential chain supplies them (environment variables, the shared `~/.aws` files and profiles,
//! SSO, web identity, container and instance roles). Certificates are always verified; a private
//! CA is trusted with `--s3-ca-bundle` (or `AWS_CA_BUNDLE`), never by turning verification off.

use std::sync::Arc;

use vaultsnap_core::{Backend, Error, Repository, S3Backend, S3Config};

use crate::cli::{Cli, S3Args};
use crate::commands::CliError;

/// The prefix that makes `--repo` name a bucket instead of a directory.
const S3_SCHEME: &str = "s3://";

/// The repository `--repo` names. A directory is only referred to; a bucket is connected to,
/// which checks that it can be reached and that the service honors conditional writes.
pub async fn open(cli: &Cli) -> Result<Repository, CliError> {
    let root = cli.repo.clone().ok_or(CliError::MissingRepository)?;
    let Some(location) = root.to_str().and_then(|text| text.strip_prefix(S3_SCHEME)) else {
        return Ok(Repository::at(root));
    };
    let config = s3_config(location, &cli.s3)?;
    tracing::debug!(?config, "connecting to an S3 bucket");
    let backend = connect(config).await?;
    Ok(Repository::with_backend(backend))
}

/// The configuration for `bucket/folder` and the command-line options. Everything that can be
/// refused without a network is refused here.
pub fn s3_config(location: &str, args: &S3Args) -> Result<S3Config, Error> {
    let mut config = S3Config::from_location(location)?;
    config.endpoint = args.endpoint.clone();
    config.region = args.region.clone();
    config.profile = args.profile.clone();
    config.path_style = args.path_style;
    config.ca_bundle = args.ca_bundle.clone();
    config.allow_insecure_http = args.allow_insecure_http;
    if let Some(requests) = args.max_requests {
        config.max_concurrent_requests = requests;
    }
    config.validate()?;
    Ok(config)
}

/// Connects on a blocking thread: the S3 client has its own runtime and must not be started from
/// inside this one's asynchronous code.
async fn connect(config: S3Config) -> Result<Arc<dyn Backend>, CliError> {
    let connected = tokio::task::spawn_blocking(move || S3Backend::connect(config))
        .await
        .map_err(|err| Error::Task(err.to_string()))?;
    Ok(connected?)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn args() -> S3Args {
        S3Args {
            endpoint: None,
            region: None,
            profile: None,
            path_style: None,
            ca_bundle: None,
            allow_insecure_http: false,
            max_requests: None,
        }
    }

    #[test]
    fn the_options_end_up_in_the_configuration() {
        let given = S3Args {
            endpoint: Some("http://127.0.0.1:21100".to_owned()),
            region: Some("eu-west-1".to_owned()),
            profile: Some("backup".to_owned()),
            path_style: Some(true),
            ca_bundle: Some(PathBuf::from("/etc/ca.pem")),
            allow_insecure_http: false,
            max_requests: Some(3),
        };
        let config = s3_config("my-bucket/laptop/backups", &given).expect("valid");
        assert_eq!(config.bucket, "my-bucket");
        assert_eq!(config.prefix, "laptop/backups");
        assert_eq!(config.endpoint.as_deref(), Some("http://127.0.0.1:21100"));
        assert_eq!(config.region.as_deref(), Some("eu-west-1"));
        assert_eq!(config.profile.as_deref(), Some("backup"));
        assert_eq!(config.path_style, Some(true));
        assert_eq!(config.ca_bundle, Some(PathBuf::from("/etc/ca.pem")));
        assert_eq!(config.max_concurrent_requests, 3);
    }

    #[test]
    fn the_defaults_are_those_of_aws() {
        let config = s3_config("my-bucket", &args()).expect("valid");
        assert!(config.endpoint.is_none() && config.region.is_none() && config.profile.is_none());
        assert_eq!(config.prefix, "");
        assert_eq!(config.max_concurrent_requests, 8);
        assert!(!config.allow_insecure_http);
    }

    #[test]
    fn bad_locations_and_unsafe_endpoints_are_refused_before_any_network_use() {
        assert!(s3_config("UPPER/case", &args()).is_err());
        assert!(s3_config("my-bucket/../x", &args()).is_err());

        let plain = S3Args {
            endpoint: Some("http://s3.example.com".to_owned()),
            ..args()
        };
        let err = s3_config("my-bucket", &plain).expect_err("plain HTTP to another machine");
        assert!(err.to_string().contains("plain HTTP"), "{err}");
        let allowed = S3Args {
            allow_insecure_http: true,
            ..plain
        };
        assert!(s3_config("my-bucket", &allowed).is_ok());

        let with_password = S3Args {
            endpoint: Some("https://user:topsecret@s3.example.com".to_owned()),
            allow_insecure_http: true,
            ..args()
        };
        let err = s3_config("my-bucket", &with_password).expect_err("credentials in a URL");
        assert!(!err.to_string().contains("topsecret"), "{err}");

        let too_many = S3Args {
            max_requests: Some(0),
            ..args()
        };
        assert!(s3_config("my-bucket", &too_many).is_err());
    }
}
