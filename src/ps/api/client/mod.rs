//! Functions to interact with the Pennsieve platform.

pub mod progress;

pub use self::progress::{ProgressCallback, ProgressUpdate};

use std::borrow::Borrow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{iter, time};

use futures::{Future as _Future, Stream as _Stream, *};
use hyper::client::{Client, HttpConnector};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{self, Method, StatusCode};
use hyper_tls::HttpsConnector;
use lazy_static::lazy_static;
use log::{debug, error};
use rusoto_cognito_idp::{CognitoIdentityProvider, InitiateAuthRequest};
use rusoto_core::credential::{AwsCredentials, StaticProvider};
use rusoto_core::request::HttpClient;
use serde;
use serde_json;
use tokio;

#[cfg(feature = "mocks")]
use mockito;

use super::request::chunked_http::ChunkedFilePayload;
use super::{request, response};
use crate::ps::config::{Config, Environment};
use crate::ps::model::upload::MultipartUploadId;
use crate::ps::model::{
	self, DatasetId, DatasetNodeId, FileUpload, ImportId, OrganizationId, PackageId, SessionToken,
	UploadId,
};
use crate::ps::util::futures::{into_future_trait, into_stream_trait};
use crate::ps::{Error, ErrorKind, Future, Result, Stream};

#[cfg(feature = "mocks")]
use url::Url;

// Pennsieve session authentication header:
const X_SESSION_ID: &str = "X-SESSION-ID";

const MAX_RETRIES: usize = 20;

lazy_static! {
	static ref ALL_METHODS: Vec<Method> = vec![
		Method::GET,
		Method::POST,
		Method::PUT,
		Method::DELETE,
		Method::HEAD,
		Method::OPTIONS,
		Method::CONNECT,
		Method::PATCH,
		Method::TRACE,
	];
	static ref NON_IDEMPOTENT_METHODS: Vec<Method> = vec![
		Method::POST, Method::DELETE
	];
	static ref IDEMPOTENT_METHODS: Vec<Method> = ALL_METHODS
		.clone()
		.into_iter()
		.filter(|method| !NON_IDEMPOTENT_METHODS.contains(method))
		.collect();

	/// A map of retryable status codes to the list of methods that we
	/// want to retry for those status codes.
	static ref RETRYABLE_STATUS_CODES: HashMap<StatusCode, Vec<Method>> = vec![
		// 4XX
		(StatusCode::TOO_MANY_REQUESTS, ALL_METHODS.clone()),
		// 5XX
		(StatusCode::SERVICE_UNAVAILABLE, ALL_METHODS.clone()),
		(StatusCode::BAD_GATEWAY, IDEMPOTENT_METHODS.clone()),
		(StatusCode::GATEWAY_TIMEOUT, IDEMPOTENT_METHODS.clone()),
	].into_iter().collect();

	/// A vec of status codes that cannot be resolved by retrying the
	/// request and should be bubbled up directly to the caller
	static ref NONRETRYABLE_STATUS_CODES: Vec<StatusCode> = vec![
		StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN
	];
}

/// Given the number of the current attempt, calculate the delay (in
/// milliseconds) for how long we should wait until the next retry
///
/// # Arguments
///
/// * `try_num` - The number of this attempt, indexed at 0
fn retry_delay(try_num: usize) -> u64 {
	500 * try_num as u64
}

struct PennsieveImpl {
	config: Config,
	http_client: Client<HttpsConnector<HttpConnector>>,
	session_token: Option<SessionToken>,
	current_organization: Option<OrganizationId>,
}

/// The Pennsieve client.
pub struct Pennsieve {
	// See https://users.rust-lang.org/t/best-pattern-for-async-update-of-self-object/15205
	// for notes on this pattern:
	inner: Arc<Mutex<PennsieveImpl>>,
}

impl Clone for Pennsieve {
	fn clone(&self) -> Self {
		Self {
			inner: Arc::clone(&self.inner),
		}
	}
}

// =============================================================================

// Request parameter
type RequestParam = (String, String);

// A useful alias when dealing with the fact that an endpoint does not expect
// a POST/PUT body, but a type is still expected:
type Nothing = serde_json::Value;

// =============================================================================

// Useful builder macros:
macro_rules! route {
	($uri:expr, $($var:ident),*) => (
		format!($uri, $($var = Into::<String>::into($var)),*)
	)
}

macro_rules! param {
	($key:expr, $value:expr) => {
		($key.into(), $value.into())
	};
}

// Based on https://docs.rs/maplit/1.0.1/maplit/
macro_rules! params {
	() => (vec![]); // For empty parameter lists
	($($key:expr => $value:expr),*) => {
		{
			let mut _p: Vec<RequestParam> = vec![];
			$(
				_p.push(param!($key, $value));
			)*
			_p
		}
	}
}

// Empty payload
macro_rules! payload {
	() => {
		None as Option<&Nothing>
	};
	($target:expr) => {
		Some($target).as_ref()
	};
}

macro_rules! get {
	($target:expr, $route:expr) => {
		$target.request($route, Method::GET, params!(), payload!())
	};
	($target:expr, $route:expr, $params:expr) => {
		$target.request($route, Method::GET, $params, payload!())
	};
}

macro_rules! post {
	($target:expr, $route:expr) => {
		$target.request($route, Method::POST, params!(), payload!())
	};
	($target:expr, $route:expr, $params:expr) => {
		$target.request($route, Method::POST, $params, payload!())
	};
	($target:expr, $route:expr, $params:expr, $payload:expr) => {
		$target.request($route, Method::POST, $params, payload!($payload))
	};
}

macro_rules! put {
	($target:expr, $route:expr) => {
		$target.request($route, Method::PUT, params!(), payload!())
	};
	($target:expr, $route:expr, $params:expr) => {
		$target.request($route, Method::PUT, $params, payload!())
	};
	($target:expr, $route:expr, $params:expr, $payload:expr) => {
		$target.request($route, Method::PUT, $params, payload!($payload))
	};
}

macro_rules! delete {
	($target:expr, $route:expr) => {
		$target.request($route, Method::DELETE, params!(), payload!())
	};
	($target:expr, $route:expr, $params:expr) => {
		$target.request($route, Method::DELETE, $params, payload!())
	};
	($target:expr, $route:expr, $params:expr, $payload:expr) => {
		$target.request($route, Method::DELETE, $params, payload!($payload))
	};
}

// ============================================================================s

impl Pennsieve {
	/// Create a new Pennsieve API client.
	pub fn new(config: Config) -> Self {
		let connector = HttpsConnector::new(4).expect("ps:couldn't create https connector");
		let http_client = Client::builder().build(connector.clone());
		Self {
			inner: Arc::new(Mutex::new(PennsieveImpl {
				config,
				http_client,
				session_token: None,
				current_organization: None,
			})),
		}
	}

	fn session_token(&self) -> Option<SessionToken> {
		self.inner.lock().unwrap().session_token.clone()
	}

	fn chunk_to_string(body: &hyper::Chunk) -> String {
		let as_bytes: Vec<u8> = body.to_vec();
		String::from_utf8_lossy(&as_bytes).to_string()
	}

	fn get_url(&self) -> url::Url {
		#[cfg(feature = "mocks")]
		let url = mockito::server_url().parse::<Url>().unwrap();

		#[cfg(not(feature = "mocks"))]
		let url = self.inner.lock().unwrap().config.env().url().clone();

		url
	}

	/// Make a request to the given route using the given json payload
	/// as a request body. This function will automatically retry if
	/// it receives a 429 response (rate limit exceeded) from the
	/// pennsieve API.
	///
	/// # Arguments
	///
	/// * `route` - The target Pennsieve API route
	/// * `method` - The HTTP method
	/// * `params` - Query params to include in the request
	/// * `payload` - A json-serializable payload to send to the platform
	///       along with the request
	fn request<I, P, Q, S>(
		&self,
		route: S,
		method: Method,
		params: I,
		payload: Option<&P>,
	) -> Future<Q>
	where
		P: serde::Serialize,
		I: IntoIterator<Item = RequestParam> + Send,
		Q: 'static + Send + serde::de::DeserializeOwned,
		S: Into<String> + Send,
	{
		let serialized_payload = payload
			.map(|p| {
				serde_json::to_string(p)
					.map(Into::into)
					.map_err(Into::<Error>::into)
			})
			.unwrap_or_else(|| Ok(vec![]))
			.map_err(Into::into);

		match serialized_payload {
			Ok(body) => self.request_with_body(
				route,
				method,
				params,
				body,
				vec![(
					hyper::header::CONTENT_TYPE,
					hyper::header::HeaderValue::from_str("application/json").unwrap(),
				)],
				true,
			),
			Err(err) => into_future_trait(futures::failed(err)),
		}
	}

	/// Make a request to the given route using the given byte payload
	/// as a request body. This is a more low-level function than the
	/// above `request` function, as it allows you to send raw bytes
	/// to the platform. This function is specifically useful when
	/// uploading files, for example.
	///
	/// If retry_on_failure is set, this function will retry the
	/// request. This means that when ever it sends a request, it must
	/// save a copy of the given byte payload in case it needs to
	/// retry again. Therefore, we try not to use retry_on_failure for
	/// requests with large byte payloads (such as uploads).
	///
	/// # Arguments
	///
	/// * `route` - The target Pennsieve API route
	/// * `method` - The HTTP method
	/// * `params` - Query params to include in the request
	/// * `body` - A byte array payload
	/// * `additional_headers` - Additional headers to include
	/// * `retry_on_failure` - Whether to retry the request on failure
	fn request_with_body<I, Q, S>(
		&self,
		route: S,
		method: Method,
		params: I,
		body1: Vec<u8>,
		additional_headers: Vec<(HeaderName, HeaderValue)>,
		retry_on_failure: bool,
	) -> Future<Q>
	where
		I: IntoIterator<Item = RequestParam>,
		Q: 'static + Send + serde::de::DeserializeOwned,
		S: Into<String>,
	{
		let route: String = route.into();
		let params: Vec<RequestParam> = params.into_iter().collect();

		let response = if retry_on_failure {
			//  A retry state object that is threaded through the
			//  retry loop in order to track state
			struct RetryState {
				ps: Pennsieve,
				route: String,
				params: Vec<RequestParam>,
				method: Method,
				body: Vec<u8>,
				additional_headers: Vec<(HeaderName, HeaderValue)>,
				try_num: usize,
			}

			let retry_state = RetryState {
				ps: self.clone(),
				route,
				params,
				method,
				body: body1, 
				additional_headers,
				try_num: 0,
			};

			let f = future::loop_fn(retry_state, move |mut retry_state| {
				retry_state
					.ps
					.single_request(
						retry_state.route.clone(),
						retry_state.params.clone(),
						retry_state.method.clone(),
						retry_state.body.clone().into(),
						retry_state.additional_headers.clone(),
					)
					.and_then(|(status_code, body)| {
						// if the status code is considered retryable, wait for a few seconds and
						// restart the loop to retry again.
						match RETRYABLE_STATUS_CODES.get(&status_code) {
							Some(retryable_methods)
								if retryable_methods.contains(&retry_state.method) =>
							{
								retry_state.try_num += 1;

								if retry_state.try_num > MAX_RETRIES {
									into_future_trait(future::err(Error::api_error(
										status_code,
										String::from_utf8_lossy(&body),
									)))
								} else {
									let delay = retry_delay(retry_state.try_num);
									debug!("Rate limit exceeded, retrying in {} ms...", delay);

									let deadline =
										time::Instant::now() + time::Duration::from_millis(delay);
									let continue_loop = tokio::timer::Delay::new(deadline)
										.map_err(Into::into)
										.map(move |_| future::Loop::Continue(retry_state));
									into_future_trait(continue_loop)
								}
							}
							_ if status_code.is_client_error() || status_code.is_server_error() => {
								into_future_trait(future::err(Error::api_error(
									status_code,
									String::from_utf8_lossy(&body),
								)))
							}
							_ => into_future_trait(future::ok(future::Loop::Break(body))),
						}
					})
			});
			into_future_trait(f)
		} else {
			let f = self
				.single_request(
					route,
					params,
					method,
					body1.into(),
					additional_headers.clone(),
				)
				.and_then(|(status_code, body)| {
					if status_code.is_client_error() || status_code.is_server_error() {
						future::err(Error::api_error(
							status_code,
							String::from_utf8_lossy(&body),
						))
					} else {
						future::ok(body)
					}
				});
			into_future_trait(f)
		};

		// Finally, attempt to parse the JSON response into a typeful
		// representation. serde_json::from_slice will fail if the
		// response body is empty, so we need to convert the empty
		// body into a valid "null" json string in that case.
		let json = response.and_then(|chunk| {
			let bytes = chunk.into_bytes();
			let bytes = if bytes.is_empty() {
				b"null"[..].into()
			} else {
				bytes
			};
			serde_json::from_slice(&bytes).map_err(Into::into)
		});

		into_future_trait(json)
	}

	/// Make a single request to the platform. This function is used
	/// by `request` and `request_with_body`, so those functions
	/// should be preferred over this one for making requests to the
	/// platform.
	///
	/// # Arguments
	///
	/// * `route` - The target Pennsieve API route
	/// * `params` - Query params to include in the request
	/// * `method` - The HTTP method
	/// * `body` - A byte array payload
	/// * `additional_headers` - Additional headers to include
	fn single_request(
		&self,
		route: String,
		params: Vec<RequestParam>,
		method: Method,
		body: hyper::Body,
		additional_headers: Vec<(HeaderName, HeaderValue)>,
	) -> Future<(StatusCode, hyper::Chunk)> {
		let token = self.session_token().clone();
		let client = self.inner.lock().unwrap().http_client.clone();

		let mut url = self.get_url();
		url.set_path(&route);

		// If query parameters are provided, add them to the constructed URL:
		for (k, v) in params {
			url.query_pairs_mut().append_pair(k.as_str(), v.as_str());
		}

		let f = url
			.to_string()
			.parse::<hyper::Uri>()
			.map_err(Into::<Error>::into)
			.into_future()
			.and_then(move |uri| {
				let mut req = hyper::Request::builder()
					.method(method.clone())
					.uri(uri)
					.body(body)
					.unwrap();

				// If a session token exists, use it to set the
				// "X-SESSION-ID" header to make subsequent requests,
				// and add it to the authorization header:
				if let Some(session_token) = token {
					req.headers_mut().insert(
						X_SESSION_ID,
						HeaderValue::from_str(session_token.borrow()).unwrap(),
					);
					req.headers_mut().insert(
						hyper::header::AUTHORIZATION,
						HeaderValue::from_str(&format!("Bearer {}", session_token.take())).unwrap(),
					);
				}

				for (header_name, header_value) in additional_headers {
					req.headers_mut().insert(header_name, header_value);
				}

				// Make the actual request:
				client
					.request(req)
					.map_err(Into::into)
					.and_then(|response| {
						let status_code = response.status();
						response
							.into_body()
							.concat2()
							.map(move |body: hyper::Chunk| {
								debug!(
									"ps:request<{method}:{url}>:serialize:payload = {payload}",
									method = method,
									url = url,
									payload = Self::chunk_to_string(&body)
								);
								(status_code, body)
							})
							.map_err(Into::into)
					})
			});

		into_future_trait(f)
	}

	/// Test if the user is logged into the Pennsieve platform.
	pub fn has_session(&self) -> bool {
		self.session_token().is_some()
	}

	/// Get the current organization the user is associated with.
	pub fn current_organization(&self) -> Option<OrganizationId> {
		self.inner.lock().unwrap().current_organization.clone()
	}

	/// Set the current organization the user is associated with.
	pub fn set_current_organization(&self, id: Option<&OrganizationId>) {
		self.inner.lock().unwrap().current_organization = id.cloned()
	}

	/// Set the session token the user is associated with.
	pub fn set_session_token(&self, token: Option<SessionToken>) {
		self.inner.lock().unwrap().session_token = token;
	}

	/// Set the active environment
	pub fn set_environment(&self, env: Environment) {
		self.inner.lock().unwrap().config = Config::new(env);
	}

	/// Log in to the Pennsieve API.
	///
	/// If successful, the Pennsieve client will store the resulting session
	/// token for subsequent API calls.
	#[allow(dead_code)]
	pub fn login<S: Into<String>>(
		&self,
		api_key: S,
		api_secret: S,
	) -> Future<response::ApiSession> {
		let cognito = rusoto_cognito_idp::CognitoIdentityProviderClient::new_with(
			HttpClient::new().expect("failed to create request dispatcher"),
			StaticProvider::from(AwsCredentials::default()),
			rusoto_core::region::Region::UsEast1,
		);

		let mut auth_parameters = HashMap::<String, String>::new();
		auth_parameters.insert("USERNAME".to_string(), api_key.into());
		auth_parameters.insert("PASSWORD".to_string(), api_secret.into());

		let this = self.clone();

		into_future_trait(get!(self, "/authentication/cognito-config").and_then(
			move |config_response: serde_json::Value| {
				let app_client_id = config_response.get("tokenPool")
					.ok_or(crate::ps::Error::initiate_auth_error("Pennsieve server Cognito config missing token pool."))?
					.get("appClientId")
					.ok_or(crate::ps::Error::initiate_auth_error("Pennsieve server Cognito config missing token pool client id."))?
					.as_str()
					.ok_or(crate::ps::Error::initiate_auth_error("Cognito application client ID is not a string"))?
					.to_string();

				let request = InitiateAuthRequest {
					analytics_metadata: None,
					auth_flow: "USER_PASSWORD_AUTH".to_string(),
					auth_parameters: Some(auth_parameters),
					client_id: app_client_id,
					client_metadata: None,
					user_context_data: None,
				};

				Ok(request)
			}
		)
		.and_then(move |request| {
			cognito
				.initiate_auth(request)
				.map_err(Into::into)
				.and_then(move |response| {
					let authentication_result = response.clone().authentication_result
						.ok_or(crate::ps::Error::initiate_auth_error("No authentication result, does another challenge need to be passed?"))?;

					let access_token = authentication_result.access_token
						.ok_or(crate::ps::Error::initiate_auth_error("No access token in the Cognito initiate auth response."))?;

					let id_token = authentication_result.id_token
						.ok_or(crate::ps::Error::initiate_auth_error(
							"No ID token in the Cognito initiate auth response."
						))?;

					let payload_parts: Vec<&str> = id_token.split(".").collect();
					let payload_b64 = base64_url::decode(payload_parts[1])?;
					let payload_str = std::str::from_utf8(&payload_b64).map_err(|err| {
						crate::ps::Error::initiate_auth_error(err.to_string())
					})?;
					let payload: serde_json::Value = serde_json::from_str(payload_str)?;

					let organization_node_id_value = payload.get("custom:organization_node_id")
						.ok_or(crate::ps::Error::initiate_auth_error("Cognito response payload does not have the `custom:organization_node_id` property"))?;

					let organization_node_id = organization_node_id_value.as_str()
						.ok_or(crate::ps::Error::initiate_auth_error("Cognito response payload `custom:organization_node_id` is not a string."))?;
					let exp = payload["exp"].as_i64()
						.ok_or(crate::ps::Error::initiate_auth_error("Cognito response payload does not have an expiration date `exp`."))?;

					this.set_current_organization(Some(&OrganizationId::new(
						organization_node_id,
					)));

					let session_token = SessionToken::new(access_token);
					this.set_session_token(Some(session_token.clone()));

					Ok(response::ApiSession::new(
						session_token,
						organization_node_id.to_string(),
						exp as i32
					))
				})
			},
		))
	}

	/// Get the current user.
	pub fn get_user(&self) -> Future<model::User> {
		get!(self, "/user/")
	}

	/// Sets the preferred organization of the current user.
	pub fn set_preferred_organization(
		&self,
		organization_id: Option<OrganizationId>,
	) -> Future<model::User> {
		let this = self.clone();
		let user = request::User::with_organization(organization_id);
		into_future_trait(put!(self, "/user/", params!(), &user).and_then(
			move |user_response: model::User| {
				this.set_current_organization(user_response.preferred_organization());
				Ok(user_response)
			},
		))
	}

	/// List the organizations the user is a member of.
	pub fn get_organizations(&self) -> Future<response::Organizations> {
		get!(self, "/organizations/")
	}

	/// Get a specific organization.
	pub fn get_organization_by_id(&self, id: OrganizationId) -> Future<response::Organization> {
		get!(self, route!("/organizations/{id}", id))
	}

	/// Get a listing of the datasets the current user has access to.
	pub fn get_datasets(&self) -> Future<Vec<response::Dataset>> {
		get!(self, "/datasets/")
	}

	/// Create a new dataset using full request object.
	pub fn create_dataset_with_request(
		&self,
		request: request::dataset::Create,
	) -> Future<response::Dataset> {
		post!(self, "/datasets/", params!(), payload!(request))
	}

	/// Create a new dataset with some request parameter defaults.
	pub fn create_dataset<N: Into<String>, D: Into<String>>(
		&self,
		name: N,
		description: Option<D>,
	) -> Future<response::Dataset> {
		self.create_dataset_with_request(request::dataset::Create::new(name, description))
	}

	/// Get a specific dataset by its ID.
	pub fn get_dataset_by_id(&self, id: DatasetNodeId) -> Future<response::Dataset> {
		get!(self, route!("/datasets/{id}", id))
	}

	/// Get a specific dataset by its name.
	pub fn get_dataset_by_name<N: Into<String>>(&self, name: N) -> Future<response::Dataset> {
		let name = name.into();
		let inner = self.clone();
		into_future_trait(self.get_datasets().and_then(move |datasets| {
			datasets
				.into_iter()
				.find(|ds| {
					let ds: &model::Dataset = ds.borrow();
					ds.name().to_lowercase() == name.to_lowercase()
				})
				.ok_or_else(|| Error::invalid_dataset_name(name))
				.into_future()
				.and_then(move |ds| {
					// NOTE: We must re-request the found dataset, as any dataset
					// returned by way of the `/datasets/` route will not include
					// child packages:
					inner.get_dataset_by_id(ds.id().clone())
				})
		}))
	}

	/// Get a dataset by ID or by name.
	pub fn get_dataset<N: Into<String>>(&self, id_or_name: N) -> Future<response::Dataset> {
		let id_or_name = id_or_name.into();
		let id = DatasetNodeId::from(id_or_name.clone());
		let name = id_or_name.clone();

		// Definitely not a dataset ID - only try to get by name
		if !id_or_name.starts_with("N:dataset:") {
			into_future_trait(self.get_dataset_by_name(name))

		// Even if it looks like an ID it could still be a name - try both methods
		} else {
			let inner = self.clone();
			into_future_trait(
				self.get_dataset_by_id(id)
					.or_else(move |_| inner.get_dataset_by_name(name)),
			)
		}
	}

	/// Get the user collaborators of the data set.
	pub fn get_dataset_user_collaborators(&self, id: DatasetNodeId) -> Future<Vec<model::User>> {
		get!(self, route!("/datasets/{id}/collaborators/users", id))
	}

	/// Get the team collaborators of the data set.
	pub fn get_dataset_team_collaborators(&self, id: DatasetNodeId) -> Future<Vec<model::Team>> {
		get!(self, route!("/datasets/{id}/collaborators/teams", id))
	}

	/// Get the organization role on the data set.
	pub fn get_dataset_organization_role(
		&self,
		id: DatasetNodeId,
	) -> Future<response::OrganizationRole> {
		get!(
			self,
			route!("/datasets/{id}/collaborators/organizations", id)
		)
	}

	/// Update an existing dataset.
	pub fn update_dataset<N: Into<String>, D: Into<String>>(
		&self,
		id: DatasetNodeId,
		name: N,
		description: Option<D>,
	) -> Future<response::Dataset> {
		put!(
			self,
			route!("/datasets/{id}", id),
			params!(),
			payload!(request::dataset::Update::new(name, description))
		)
	}

	/// Delete an existing dataset.
	pub fn delete_dataset(&self, id: DatasetNodeId) -> Future<()> {
		let f: Future<response::EmptyMap> = delete!(self, route!("/datasets/{id}", id));
		into_future_trait(f.map(|_| ()))
	}

	/// Create a new package.
	/// TODO: see https://github.com/Pennsieve/pennsieve-rust/pull/45/files#r265581502
	/// for a strategy for cleaning up API functions with many optional arguments.
	pub fn create_package<N, D, P, F>(
		&self,
		name: N,
		package_type: P,
		dataset: D,
		parent: Option<F>,
	) -> Future<response::Package>
	where
		D: Into<DatasetNodeId>,
		N: Into<String>,
		P: Into<String>,
		F: Into<String>,
	{
		post!(
			self,
			"/packages/",
			params!(),
			payload!(request::package::Create::new(
				name,
				package_type,
				dataset,
				parent
			))
		)
	}

	/// Get a specific package.
	pub fn get_package_by_id(&self, id: PackageId) -> Future<response::Package> {
		get!(self, route!("/packages/{id}", id))
	}

	/// Get the source files that are part of a package.
	pub fn get_package_sources(&self, id: PackageId) -> Future<response::Files> {
		get!(self, route!("/packages/{id}/sources", id))
	}

	/// Update an existing package.
	pub fn update_package<N: Into<String>>(
		&self,
		id: PackageId,
		name: N,
	) -> Future<response::Package> {
		put!(
			self,
			route!("/packages/{id}", id),
			params!(),
			payload!(request::package::Update::new(name))
		)
	}

	/// Process a package in the UPLOADED state.
	pub fn process_package(&self, id: PackageId) -> Future<()> {
		let f = put!(self, route!("/packages/{id}/process", id)).map(|_: Nothing| ());
		into_future_trait(f)
	}

	/// Move several packages to a destination package.
	/// If destination is None, the package is moved to the top level of the dataset.
	pub fn mv<T: Into<PackageId>, D: Into<PackageId>>(
		&self,
		things: Vec<T>,
		destination: Option<D>,
	) -> Future<response::MoveResponse> {
		post!(
			self,
			"/data/move",
			params!(),
			payload!(request::mv::Move::new(things, destination))
		)
	}

	/// Get the members that belong to the current users organization.
	pub fn get_members(&self) -> Future<Vec<model::User>> {
		into_future_trait(match self.current_organization() {
			Some(org) => self.get_members_by_organization(org),
			None => into_future_trait(future::err::<_, Error>(ErrorKind::NoOrganizationSet.into())),
		})
	}

	/// Get the members that belong to the specified organization.
	pub fn get_members_by_organization(&self, id: OrganizationId) -> Future<Vec<model::User>> {
		get!(self, route!("/organizations/{id}/members", id))
	}

	/// Get the members that belong to the current users organization.
	pub fn get_teams(&self) -> Future<Vec<response::Team>> {
		into_future_trait(match self.current_organization() {
			Some(org) => self.get_teams_by_organization(org),
			None => into_future_trait(future::err::<_, Error>(ErrorKind::NoOrganizationSet.into())),
		})
	}

	/// Get the teams that belong to the specified organization.
	pub fn get_teams_by_organization(&self, id: OrganizationId) -> Future<Vec<response::Team>> {
		get!(self, route!("/organizations/{id}/teams", id))
	}

	/// Generate a preview of the files to be uploaded.
	pub fn preview_upload<P, Q>(
		&self,
		organization_id: &OrganizationId,
		dataset_id: &DatasetId,
		path: Option<P>,
		files: &[(UploadId, Q)],
		append: bool,
		is_directory_upload: bool,
	) -> Future<response::UploadPreview>
	where
		P: AsRef<Path>,
		Q: AsRef<Path>,
	{
		let s3_files: Result<Vec<model::S3File>> = files
			.iter()
			.map(|(upload_id, file)| {
				let path = path.as_ref();
				if is_directory_upload {
					path.ok_or_else(|| {
						Error::invalid_arguments(
							"Path cannot be None when is_directory_upload is true",
						)
					})
					.and_then(|path| {
						FileUpload::new_recursive_upload(*upload_id, path, file.as_ref())
					})
				} else if let Some(path) = path {
					FileUpload::new_non_recursive_upload(*upload_id, path.as_ref().join(file))
				} else {
					FileUpload::new_non_recursive_upload(*upload_id, file)
				}
			})
			.collect::<Result<Vec<_>>>()
			.and_then(|file_uploads| {
				file_uploads
					.iter()
					.map(|file_upload| file_upload.to_s3_file())
					.collect()
			});

		let ps = self.clone();
		let organization_id = organization_id.clone();
		let dataset_id = dataset_id.clone();

		let post = s3_files.into_future().and_then(move |s3_files| {
			post!(
				ps,
				route!(
					"/upload/preview/organizations/{organization_id}",
					organization_id
				),
				params!(
					"append" => if append { "true" } else { "false" },
					"dataset_id" => String::from(dataset_id)
				),
				&request::UploadPreview::new(&s3_files)
			)
		});

		into_future_trait(post)
	}

	#[allow(clippy::too_many_arguments)]
	/// Upload a batch of files using the upload service.
	pub fn upload_file_chunks<P, C>(
		&self,
		organization_id: &OrganizationId,
		import_id: &ImportId,
		path: P,
		files: Vec<model::S3File>,
		missing_parts: Option<response::FilesMissingParts>,
		progress_callback: C,
		parallelism: usize,
	) -> Stream<ImportId>
	where
		P: 'static + AsRef<Path>,
		C: 'static + ProgressCallback + Clone,
	{
		let ps = self.clone();
		let organization_id = organization_id.clone();
		let import_id = import_id.clone();
		let progress_callback = progress_callback.clone();

		let missing_file_names: Option<Vec<String>> = missing_parts
			.clone()
			.map(|mp| mp.files.into_iter().map(|f| f.file_name).collect());

		let fs = stream::futures_unordered(
			files
				.into_iter()
				.filter(|file| match &missing_file_names {
					None => true,
					Some(mp) => mp.contains(file.file_name()),
				})
				.zip(iter::repeat(path.as_ref().to_path_buf()))
				.map(|file| future::ok::<(model::S3File, PathBuf), Error>(file.clone())),
		)
		.map(move |(file, path): (model::S3File, PathBuf)| {
			let mut file_path = path.clone();
			let file = file.clone();

			file_path.push(file.file_name());

			let file_missing_parts: Option<response::FileMissingParts> = match missing_parts {
				Some(ref mp) => mp
					.files
					.iter()
					.find(|p| &p.file_name == file.file_name())
					.cloned(),
				None => None,
			};

			let chunked_file_payload = if let Some(chunked_upload_properties) =
				file.chunked_upload()
			{
				debug!(
					"ps:upload_file_chunks<file = {file_name}> :: \
					 Chunk size received from the upload service: {chunk_size}.",
					file_name = file.file_name(),
					chunk_size = chunked_upload_properties.chunk_size
				);

				ChunkedFilePayload::new_with_chunk_size(
					import_id.clone(),
					file_path,
					chunked_upload_properties.chunk_size,
					file_missing_parts.as_ref(),
				)
			} else {
				debug!(
					"ps:upload_file_chunks<file = {file_name}> :: \
					 No chunk size received from the upload service. \
					 Falling back to default.",
					file_name = file.file_name()
				);
				ChunkedFilePayload::new(import_id.clone(), file_path, file_missing_parts.as_ref())
			};

			let ps = ps.clone();
			let organization_id = organization_id.clone();
			let import_id = import_id.clone();
			let progress_callback = progress_callback.clone();

			chunked_file_payload
				.map(move |(file_chunk, progress_update)| {
					if let Some(MultipartUploadId(multipart_upload_id)) = file.multipart_upload_id()
					{
						let import_id = import_id.clone();
						let import_id_clone = import_id.clone();
						let organization_id = organization_id.clone();
						let progress_callback = progress_callback.clone();

						into_future_trait(
							ps.request_with_body(
								route!(
									"/upload/chunk/organizations/{organization_id}/id/{import_id}",
									organization_id,
									import_id
								),
								Method::POST,
								params!(
									"filename" => file.file_name().to_string(),
									"multipartId" => multipart_upload_id.to_string(),
									"chunkChecksum" => file_chunk.checksum.0,
									"chunkNumber" => file_chunk.chunk_number.to_string()
								),
								file_chunk.bytes,
								vec![],
								false,
							)
							.and_then(
								move |response: response::UploadResponse| {
									if response.success {
										progress_callback.on_update(&progress_update.clone());
										future::ok(import_id_clone)
									} else {
										future::err(Error::upload_error(
											response.error.unwrap_or_else(|| {
												"no error message supplied".into()
											}),
										))
									}
								},
							),
						)
					} else {
						into_future_trait(future::err(Error::upload_error(format!(
							"no multipartId was provided for file: {}",
							file.file_name()
						))))
					}
				})
				.map_err(Into::into)
				.buffer_unordered(parallelism)
		})
		.flatten();

		into_stream_trait(fs)
	}

	/// Complete an upload to the upload service
	pub fn complete_upload(
		&self,
		organization_id: &OrganizationId,
		import_id: &ImportId,
		dataset_id: &DatasetNodeId,
		destination_id: Option<&PackageId>,
		append: bool,
	) -> Future<response::Manifests> {
		let mut params = params!(
			"datasetId" => dataset_id,
			"append" => if append { "true" } else { "false" }
		);
		if let Some(dest_id) = destination_id {
			params.push(param!("destinationId", dest_id.clone()));
		}

		post!(
			self,
			route!(
				"/upload/complete/organizations/{organization_id}/id/{import_id}",
				organization_id,
				import_id
			),
			params
		)
	}

	/// Get the upload status using the upload service
	pub fn get_upload_status(
		&self,
		organization_id: &OrganizationId,
		import_id: &ImportId,
	) -> Future<Option<response::FilesMissingParts>> {
		get!(
			self,
			route!(
				"/upload/status/organizations/{organization_id}/id/{import_id}",
				organization_id,
				import_id
			)
		)
	}

	/// Get the hash of an uploaded file from the upload service
	pub fn get_upload_hash<S>(
		&self,
		import_id: &ImportId,
		file_name: S,
	) -> Future<response::FileHash>
	where
		S: Into<String>,
	{
		get!(
			self,
			route!("/upload/hash/id/{import_id}", import_id),
			params!("fileName" => file_name)
		)
	}

	pub fn upload_file_chunks_with_retries<P, C>(
		&self,
		organization_id: &OrganizationId,
		import_id: &ImportId,
		path: &P,
		files: Vec<model::S3File>,
		progress_callback: C,
		parallelism: usize,
	) -> Stream<ImportId>
	where
		P: 'static + AsRef<Path> + Send,
		C: 'static + ProgressCallback + Clone,
	{
		#[derive(Clone)]
		struct LoopDependencies<C: ProgressCallback + Clone> {
			organization_id: OrganizationId,
			import_id: ImportId,
			path: PathBuf,
			files: Vec<model::S3File>,
			missing_parts: Option<response::FilesMissingParts>,
			result: Option<Vec<ImportId>>,
			progress_callback: C,
			try_num: usize,
			ps: Pennsieve,
			parallelism: usize,
		}

		impl<C: ProgressCallback + Clone> LoopDependencies<C> {
			pub fn increment_attempt_count(self) -> Self {
				Self {
					organization_id: self.organization_id,
					import_id: self.import_id,
					path: self.path,
					files: self.files,
					missing_parts: self.missing_parts,
					result: self.result,
					progress_callback: self.progress_callback,
					try_num: self.try_num + 1,
					ps: self.ps,
					parallelism: self.parallelism,
				}
			}
		}

		let ld = LoopDependencies {
			organization_id: organization_id.clone(),
			import_id: import_id.clone(),
			path: path.as_ref().to_path_buf(),
			files,
			missing_parts: None,
			result: None,
			progress_callback,
			try_num: 0,
			ps: self.clone(),
			parallelism,
		};

		let retry_loop = future::loop_fn(ld, |mut ld| {

			let ld_err = ld.clone();

			ld.ps
				.get_upload_status(&ld.organization_id, &ld.import_id)
				.map(|parts| {
					ld.missing_parts = parts;
					ld
				})
				.and_then(|ld| {
					ld.ps
						.upload_file_chunks(
							&ld.organization_id,
							&ld.import_id,
							ld.path.clone(),
							ld.files.clone(),
							ld.missing_parts.clone(),
							ld.progress_callback.clone(),
							ld.parallelism,
						)
						.collect()
						.map(future::Loop::Break)
				})
				.into_future()
				.or_else(move |err| {

					debug!("Upload encountered an error: {error}", error = err);
					match err.kind() {
						// error cannot be retried, bubble up the error
						ErrorKind::ApiError{ status_code, .. } if NONRETRYABLE_STATUS_CODES.contains(status_code) => {
							debug!("Upload received status {status} from API which cannot be retried", status = status_code);
							into_future_trait(future::err(err))
						}

						// error that should be retried (if we are under MAX_RETRIES), retry the upload
						_ if MAX_RETRIES > ld_err.try_num => {
							let delay = retry_delay(ld_err.try_num);

							debug!("Waiting {millis} millis to retry...", millis = delay);

							// delay
							let deadline = time::Instant::now() + time::Duration::from_millis(delay);
							let continue_loop = tokio::timer::Delay::new(deadline)
								.map_err(Into::<Error>::into)
								.map(move |_| {
									debug!(
										"Attempting to resume missing parts. Attempt {try_num}/{retries})...",
										try_num = ld_err.try_num, retries = MAX_RETRIES
									);
									future::Loop::Continue(ld_err.increment_attempt_count())
								});
							into_future_trait(continue_loop)
						}

						// MAX_RETRIES exceeded, bubble up the error
						_ => {
							error!("Retries exceeded during upload. Bubbling up error {error}", error = err);
							into_future_trait(future::err(err))
						}
					}
				})
		})
		.map(|import_ids| {
			future::ok::<Stream<ImportId>, Error>(
				into_stream_trait(stream::futures_unordered(
					import_ids
						.iter()
						.map(|import_id| future::ok(import_id.clone())),
				)),
			)
				.into_stream()
			.flatten()
		})
		.into_stream()
		.flatten();

		into_stream_trait(retry_loop)
	}
}

#[cfg(test)]
pub mod tests {
	use super::*;
	use std::{fs, path, result, sync, thread};

	use lazy_static::lazy_static;
	use mockito::mock;

	// use ps::api::{PSChildren, PSId, PSName};
	use crate::ps::config::Environment;
	use crate::ps::util::futures::into_future_trait;
	use crate::ps::util::rand_suffix;

	const TEST_ENVIRONMENT: Environment = Environment::NonProduction;
	const TEST_API_KEY: &str = env!("PENNSIEVE_API_KEY");
	const TEST_SECRET_KEY: &str = env!("PENNSIEVE_SECRET_KEY");

	// "Agent Testing"
	const FIXTURE_ORGANIZATION: &str = "N:organization:713eeb6e-c42c-445d-8a60-818c741ea87a";

	// Dedicated agent email
	#[allow(dead_code)]
	const FIXTURE_EMAIL: &str = "agent-test@pennsieve.com";

	// // Dedicated agent user
	// #[allow(dead_code)]
	// const FIXTURE_USER: &str = "N:user:6caa1955-c39e-4198-83c6-aa8fe3afbe93";

	const FIXTURE_DATASET: &str = "N:dataset:e5902b32-7954-463b-bb4c-2c9cf5b3bcfb";
	const FIXTURE_DATASET_NAME: &str = "AGENT-FIXTURE";

	const FIXTURE_PACKAGE: &str = "N:collection:c602852e-3cc0-4b24-a68a-dd84045dfa47";
	const FIXTURE_PACKAGE_NAME: &str = "AGENT-TEST-PACKAGE";

	lazy_static! {
		static ref CONFIG: Config = Config::new(TEST_ENVIRONMENT);
		static ref TEST_FILES: Vec<String> = test_data_files("/small");
		static ref TEST_DATA_DIR: String = test_data_dir("/small");
		pub static ref BIG_TEST_FILES: Vec<String> = test_data_files("/big");
		pub static ref BIG_TEST_DATA_DIR: String = test_data_dir("/big");
		pub static ref MEDIUM_TEST_FILES: Vec<String> = test_data_files("/medium");
		pub static ref MEDIUM_TEST_DATA_DIR: String = test_data_dir("/medium");
	}

	/// given a 'runner' function, run the given Pennsieve instance
	/// through that function and block until completion
	fn run<F, T>(ps: &Pennsieve, runner: F) -> Result<T>
	where
		F: Fn(Pennsieve) -> Future<T>,
		T: 'static + Send,
	{
		let mut rt = tokio::runtime::Runtime::new()?;
		let result = rt.block_on(runner(ps.clone()));
		rt.shutdown_on_idle();
		result
	}

	struct Inner(sync::Mutex<bool>);

	impl Inner {
		pub fn new() -> Self {
			Inner(sync::Mutex::new(false))
		}
	}

	pub struct ProgressIndicator {
		inner: sync::Arc<Inner>,
	}

	impl Clone for ProgressIndicator {
		fn clone(&self) -> Self {
			Self {
				inner: Arc::clone(&self.inner),
			}
		}
	}

	impl ProgressIndicator {
		pub fn new() -> Self {
			Self {
				inner: sync::Arc::new(Inner::new()),
			}
		}
	}

	impl ProgressCallback for ProgressIndicator {
		fn on_update(&self, _update: &ProgressUpdate) {
			*self.inner.0.lock().unwrap() = true;
		}
	}

	fn ps() -> Pennsieve {
		Pennsieve::new((*CONFIG).clone())
	}

	// Returns the test data directory `<project>/data/<data_dir>`:
	fn test_data_dir(data_dir: &str) -> String {
		concat!(env!("CARGO_MANIFEST_DIR"), "/test/data").to_string() + data_dir
	}

	// Returns a `Vec<String>` of test data filenames taken from the specified
	// test data directory:
	fn test_data_files(data_dir: &str) -> Vec<String> {
		match fs::read_dir(test_data_dir(data_dir)) {
			Ok(entries) => entries
				.map(|entry| entry.unwrap().file_name().into_string().clone())
				.collect::<result::Result<Vec<_>, _>>()
				.unwrap(),
			Err(e) => {
				eprintln!("{:?} :: {:?}", data_dir, e);
				vec![]
			}
		}
	}

	fn add_upload_ids(file_paths: &Vec<String>) -> Vec<(UploadId, String)> {
		file_paths
			.iter()
			.enumerate()
			.map(|(id, file)| (UploadId::from(id as u64), file.to_string()))
			.collect()
	}

	#[test]
	fn login_successfully_locally() {
		let ps = ps();
		let result = run(&ps, move |ps| ps.login(TEST_API_KEY, TEST_SECRET_KEY));
		assert!(result.is_ok());
		assert!(ps.session_token().is_some());
	}

	#[test]
	fn login_fails_locally() {
		let ps = ps();
		let result = run(&ps, move |ps| {
			ps.login(TEST_API_KEY, "this-is-a-bad-secret")
		});
		assert!(result.is_err());
		assert!(ps.session_token().is_none());
	}

	#[test]
	#[cfg_attr(not(feature = "mocks"), ignore)]
	fn login_returns_error_when_no_token_pool_config_present() {
		let ps = ps();

		let _mock = mock("GET", "/authentication/cognito-config")
			.with_status(200)
			.with_body("{}")
			.create();

		let result = run(&ps, move |ps| ps.login(TEST_API_KEY, TEST_SECRET_KEY));

		assert!(result.is_err());
		assert!(ps.session_token().is_none());

		if let Err(error) = result {
			assert_eq!(
				error.kind(),
				crate::ps::Error::initiate_auth_error(
					"Pennsieve server Cognito config missing token pool."
				)
				.kind()
			);
		}
	}

	#[test]
	#[cfg_attr(not(feature = "mocks"), ignore)]
	fn login_returns_error_when_no_token_pool_client_id_config_present() {
		let ps = ps();
		let body = " { \"tokenPool\": {} } ";

		let _mock = mock("GET", "/authentication/cognito-config")
			.with_status(200)
			.with_body(body)
			.create();

		let result = run(&ps, move |ps| ps.login(TEST_API_KEY, TEST_SECRET_KEY));

		assert!(result.is_err());
		assert!(ps.session_token().is_none());

		if let Err(error) = result {
			assert_eq!(
				error.kind(),
				crate::ps::Error::initiate_auth_error(
					"Pennsieve server Cognito config missing token pool client id."
				)
				.kind()
			);
		}
	}

	#[test]
	#[cfg_attr(not(feature = "mocks"), ignore)]
	fn login_returns_error_when_token_pool_client_id_is_not_a_string() {
		let ps = ps();
		let body = " { \"tokenPool\": { \"appClientId\": [] } } ";

		let _mock = mock("GET", "/authentication/cognito-config")
			.with_status(200)
			.with_body(body)
			.create();

		let result = run(&ps, move |ps| ps.login(TEST_API_KEY, TEST_SECRET_KEY));

		assert!(result.is_err());
		assert!(ps.session_token().is_none());

		if let Err(error) = result {
			assert_eq!(
				error.kind(),
				crate::ps::Error::initiate_auth_error(
					"Pennsieve server Cognito config token pool client id could not be interpreted as a string."
				)
					.kind()
			);
		}
	}

	#[test]
	fn fetching_organizations_after_login_is_successful() {
		let org = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_organizations()),
			)
		});

		if org.is_err() {
			panic!("{}", org.unwrap_err().to_string());
		}
	}

	#[test]
	fn fetching_user_after_login_is_successful() {
		let user = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_user()),
			)
		});

		if user.is_err() {
			panic!("{}", user.unwrap_err().to_string());
		}
	}

	#[test]
	fn updating_org_after_login_is_successful() {
		let user = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_user().map(|user| (user, ps)))
					.and_then(move |(user, ps)| {
						let org = user.preferred_organization().clone();
						ps.set_preferred_organization(org.cloned()).map(|_| ps)
					})
					.and_then(move |ps| ps.get_user()),
			)
		});

		if user.is_err() {
			panic!("{}", user.unwrap_err().to_string());
		}
	}

	#[test]
	fn fetching_organizations_fails_if_login_fails() {
		let org = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, "another-bad-secret")
					.and_then(move |_| ps.get_organizations()),
			)
		});

		assert!(org.is_err());
	}

	#[test]
	fn fetching_organization_by_id_is_successful() {
		let org = run(&ps(), move |ps| {
			into_future_trait(ps.login(TEST_API_KEY, TEST_SECRET_KEY).and_then(move |_| {
				ps.get_organization_by_id(OrganizationId::new(FIXTURE_ORGANIZATION))
			}))
		});

		if org.is_err() {
			panic!("{}", org.unwrap_err().to_string());
		}
	}

	#[test]
	fn fetching_datasets_after_login_is_successful() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_datasets()),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}
	}

	#[test]
	fn fetching_datasets_fails_if_login_fails() {
		let ds = run(&ps(), move |ps| into_future_trait(ps.get_datasets()));
		assert!(ds.is_err());
	}

	#[test]
	fn fetching_dataset_by_id_successful_if_logged_in_and_exists() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset_by_id(DatasetNodeId::new(FIXTURE_DATASET))),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}
	}

	#[test]
	fn fetching_dataset_by_name_successful_if_logged_in_and_exists() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset_by_name(FIXTURE_DATASET_NAME)),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}
	}

	#[test]
	fn fetching_dataset_generic_works_with_name() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset(FIXTURE_DATASET_NAME)),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}
	}

	#[test]
	fn fetching_dataset_generic_works_with_id() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset(DatasetNodeId::new(FIXTURE_DATASET))),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}
	}

	#[test]
	fn fetching_child_dataset_by_id_is_successful_can_contains_child_packages_if_found_by_id() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset_by_id(FIXTURE_DATASET.into())),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}

		assert!(ds
			.unwrap()
			.get_package_by_id(Into::<model::PackageId>::into(FIXTURE_PACKAGE))
			.is_some());
	}

	#[test]
	fn fetching_child_dataset_by_name_is_successful_can_contains_child_packages_if_found_by_id() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset_by_id(FIXTURE_DATASET.into())),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}

		assert!(ds
			.unwrap()
			.get_package_by_name(FIXTURE_PACKAGE_NAME)
			.is_some());
	}

	#[test]
	fn fetching_child_dataset_by_id_is_successful_can_contains_child_packages_if_found_by_name() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset_by_name(FIXTURE_DATASET_NAME)),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}

		assert!(ds
			.unwrap()
			.get_package_by_id(Into::<model::PackageId>::into(FIXTURE_PACKAGE))
			.is_some());
	}

	#[test]
	fn fetching_child_dataset_by_name_is_successful_can_contains_child_packages_if_found_by_name() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset_by_name(FIXTURE_DATASET_NAME)),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}

		assert!(ds
			.unwrap()
			.get_package_by_name(FIXTURE_PACKAGE_NAME)
			.is_some());
	}

	#[test]
	fn fetching_child_dataset_fails_if_it_does_not_exists() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset_by_name(FIXTURE_DATASET_NAME)),
			)
		});

		if ds.is_err() {
			panic!("{}", ds.unwrap_err().to_string());
		}

		assert!(ds.unwrap().get_package_by_name("doesnotexist").is_none());
	}

	#[test]
	fn fetching_dataset_by_name_fails_if_it_does_not_exist() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_dataset_by_name("doesnotexist")),
			)
		});

		assert!(ds.is_err());
	}

	#[test]
	fn fetching_package_by_id_successful_if_logged_in_and_exists() {
		let package = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_package_by_id(PackageId::new(FIXTURE_PACKAGE))),
			)
		});
		if package.is_err() {
			panic!("{}", package.unwrap_err().to_string());
		}
	}

	#[test]
	fn fetching_package_by_id_invalid_if_logged_in_and_exists() {
		let package = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_package_by_id(PackageId::new("invalid_package_id"))),
			)
		});

		if let Err(e) = package {
			match e.kind() {
				// pennsieve api returns 403 in this case..it should really be 404 I think
				ErrorKind::ApiError { status_code, .. } => assert_eq!(status_code.as_u16(), 404),
				_ => assert!(false),
			}
		}
	}

	#[test]
	fn fetching_dataset_by_id_fails_if_logged_in_but_doesnt_exists() {
		let ds = run(&ps(), move |ps| {
			into_future_trait(ps.login(TEST_API_KEY, TEST_SECRET_KEY).and_then(move |_| {
				ps.get_dataset_by_id(DatasetNodeId::new(
					"N:dataset:not-real-6803-4a67-ps20-83076774a5c7",
				))
			}))
		});
		assert!(ds.is_err());
	}

	#[test]
	fn fetch_dataset_user_collaborators() {
		let collaborators = run(&ps(), move |ps| {
			into_future_trait(ps.login(TEST_API_KEY, TEST_SECRET_KEY).and_then(move |_| {
				ps.get_dataset_user_collaborators(DatasetNodeId::new(FIXTURE_DATASET))
			}))
		})
		.unwrap();

		assert!(collaborators.iter().all(|c| c.role().is_some()));

		let mut collaborators: Vec<(String, String)> = collaborators
			.iter()
			.map(|u| (u.first_name().clone(), u.role().unwrap().clone()))
			.collect();
		collaborators.sort();

		let expected = ("Bo".to_string(), "owner".to_string());

		assert!(collaborators.contains(&expected));
	}

	#[test]
	fn fetch_dataset_team_collaborators() {
		let collaborators = run(&ps(), move |ps| {
			into_future_trait(ps.login(TEST_API_KEY, TEST_SECRET_KEY).and_then(move |_| {
				ps.get_dataset_team_collaborators(DatasetNodeId::new(FIXTURE_DATASET))
			}))
		})
		.unwrap();
		assert!(collaborators.iter().all(|c| c.role().is_some()));

		let mut collaborators: Vec<(String, String)> = collaborators
			.iter()
			.map(|t| (t.name().clone(), t.role().unwrap().clone()))
			.collect();
		collaborators.sort();

		let expected = vec![("Agent Devs".to_string(), "manager".to_string())];

		assert_eq!(collaborators, expected);
	}

	#[test]
	fn fetch_dataset_organization_role() {
		let organization_role = run(&ps(), move |ps| {
			into_future_trait(ps.login(TEST_API_KEY, TEST_SECRET_KEY).and_then(move |_| {
				ps.get_dataset_organization_role(DatasetNodeId::new(FIXTURE_DATASET))
			}))
		})
		.unwrap();

		let organization_role = (
			organization_role.name().clone(),
			organization_role.role().cloned(),
		);
		let expected = ("Test-Org".to_string(), None);

		assert_eq!(organization_role, expected);
	}

	#[test]
	fn fetch_members() {
		let members = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_user().map(|user| (user, ps)))
					.and_then(move |(user, ps)| {
						let org = user.preferred_organization().clone();
						ps.set_preferred_organization(org.cloned()).map(|_| ps)
					})
					.and_then(move |ps| ps.get_members()),
			)
		});
		assert!(members.is_ok());
	}

	#[test]
	fn fetch_teams() {
		let teams = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.get_user().map(|user| (user, ps)))
					.and_then(move |(user, ps)| {
						let org = user.preferred_organization().clone();
						ps.set_preferred_organization(org.cloned()).map(|_| ps)
					})
					.and_then(move |ps| ps.get_teams()),
			)
		});
		assert!(teams.is_ok());
	}

	#[test]
	fn creating_then_updating_then_delete_dataset_successful() {
		let new_dataset_name = rand_suffix("$new-test-dataset".to_string());
		let result = run(&ps(), move |ps| {
			let new_dataset_name = new_dataset_name.clone();
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| {
						let new_dataset_name = new_dataset_name.clone();
						ps.create_dataset(
							rand_suffix("__agent-test-dataset".to_string()),
							Some("A test dataset created by the agent".to_string()),
						)
						.map(|ds| (ps, ds, new_dataset_name))
					})
					.and_then(move |(ps, ds, new_dataset_name)| {
						Ok(ds.id().clone()).map(|id| (ps, id, new_dataset_name))
					})
					.and_then(move |(ps, id, new_dataset_name)| {
						ps.update_dataset(
							id.clone(),
							new_dataset_name.clone(),
							None as Option<String>,
						)
						.map(|_| (ps, id, new_dataset_name))
					})
					.and_then(move |(ps, id, new_dataset_name)| {
						let id = id.clone();
						ps.get_dataset_by_id(id.clone())
							.and_then(move |ds| {
								assert_eq!(ds.take().name().clone(), new_dataset_name);
								Ok(id)
							})
							.map(|id| (ps, id))
					})
					.and_then(move |(ps, id)| ps.delete_dataset(id)),
			)
		});

		if result.is_err() {
			panic!("{}", result.unwrap_err().to_string());
		}
	}

	#[test]
	fn creating_then_updating_then_delete_package_successful() {
		let result = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| {
						ps.create_dataset(
							rand_suffix("__agent-test-dataset".to_string()),
							Some("A test dataset created by the agent".to_string()),
						)
						.map(|ds| (ps, ds))
					})
					.and_then(move |(ps, ds)| Ok(ds.id().clone()).map(|id| (ps, id)))
					.and_then(move |(ps, ds_id)| {
						ps.create_package(
							rand_suffix("__agent-test-package"),
							"Text",
							ds_id.clone(),
							None as Option<String>,
						)
						.map(|pkg| (ps, ds_id, pkg))
					})
					.and_then(move |(ps, ds_id, pkg)| {
						let pkg_id = pkg.take().id().clone();
						ps.update_package(pkg_id.clone(), "new-package-name")
							.map(|_| (ps, pkg_id, ds_id))
					})
					.and_then(move |(ps, pkg_id, ds_id)| {
						ps.get_package_by_id(pkg_id).and_then(|pkg| {
							assert_eq!(pkg.take().name().clone(), "new-package-name".to_string());
							Ok((ps, ds_id))
						})
					})
					.and_then(move |(ps, ds_id)| ps.delete_dataset(ds_id)),
			)
		});

		if result.is_err() {
			panic!("{}", result.unwrap_err().to_string());
		}
	}

	#[test]
	fn process_package_failed() {
		let resp = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| ps.process_package(PackageId::new(FIXTURE_PACKAGE))),
			)
		});

		if let Err(e) = resp {
			match e.kind() {
				ErrorKind::ApiError { status_code, .. } => assert_eq!(status_code.as_u16(), 400),
				_ => assert!(false),
			}
		}
	}

	#[test]
	fn move_package_to_toplevel() {
		let result = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| {
						ps.create_dataset(
							rand_suffix("__agent-test-dataset".to_string()),
							Some("A test dataset created by the agent".to_string()),
						)
						.map(|ds| (ps, ds))
					})
					.and_then(move |(ps, ds)| Ok(ds.id().clone()).map(|id| (ps, id)))
					.and_then(move |(ps, ds_id)| {
						ps.create_package(
							rand_suffix("__agent-test-collection"),
							"Collection",
							ds_id.clone(),
							None as Option<String>,
						)
						.map(|col| (ps, ds_id, col))
					})
					.and_then(move |(ps, ds_id, col)| {
						ps.create_package(
							rand_suffix("__agent-test-package"),
							"Text",
							ds_id.clone(),
							Some(col.id().clone()),
						)
						.map(|pkg| (ps, ds_id, pkg, col))
					})
					.and_then(move |(ps, ds_id, pkg, col)| {
						// Move package to top-level of dataset
						ps.mv(vec![pkg.take().id().clone()], None as Option<PackageId>)
							.map(|_| (ps, ds_id, col))
					})
					.and_then(move |(ps, ds_id, col)| {
						ps.get_dataset_by_id(ds_id.clone()).and_then(|dataset| {
							// Dataset now has two children (__agent-test-collection and __agent-test-package)
							assert_eq!(dataset.children().unwrap().len(), 2);
							Ok((ps, ds_id, col))
						})
					})
					.and_then(move |(ps, ds_id, col)| {
						ps.get_package_by_id(col.id().clone())
							.and_then(|collection| {
								// Collection now has no children
								assert_eq!(collection.children().unwrap().len(), 0);
								Ok((ps, ds_id))
							})
					})
					.and_then(move |(ps, ds_id)| ps.delete_dataset(ds_id)),
			)
		});

		if result.is_err() {
			panic!("{}", result.unwrap_err().to_string());
		}
	}

	#[test]
	fn process_package_succeeds() {
		let file_name = "test-tiny.png";
		let file_paths = vec![format!("{}/{}", TEST_DATA_DIR.to_string(), file_name)];
		let enumerated_files = add_upload_ids(&file_paths);
		// create upload
		let result = run(&ps(), |ps| {
			upload_to_upload_service(ps, enumerated_files.clone())
		});

		let (_, dataset_id, _) = match result {
			Ok(results) => results,
			Err(err) => {
				println!("{}", err.to_string());
				panic!();
			}
		};
		let package = run(&ps(), |ps| {
			let ps_clone = ps.clone();
			let dataset_id_clone = dataset_id.clone();
			let f = ps
				.login(TEST_API_KEY, TEST_SECRET_KEY)
				.and_then(move |_| ps_clone.clone().get_dataset_by_id(dataset_id_clone))
				.map(move |ds| {
					let file_name_clone = file_name.clone();
					ds.get_package_by_name(file_name_clone)
				});
			into_future_trait(f)
		})
		.unwrap()
		.unwrap();

		let now = std::time::SystemTime::now();
		let sleep_duration = std::time::Duration::new(5, 0); // 5 seconds
		let timeout_duration = std::time::Duration::new(120, 0); // 2 minutes
		'infinite: loop {
			if now.elapsed().unwrap() < timeout_duration {
				let current_package = run(&ps(), |ps| {
					let ps_clone = ps.clone();
					let package = package.clone();
					let f = ps
						.login(TEST_API_KEY, TEST_SECRET_KEY)
						.and_then(move |_| ps_clone.get_package_by_id(package.id().clone()));
					into_future_trait(f)
				})
				.unwrap();
				if current_package.state().unwrap().clone() == "UPLOADED".to_string() {
					let result = run(&ps(), |ps| {
						let ps_clone = ps.clone();
						let current_package_clone = current_package.clone();
						let f = ps.login(TEST_API_KEY, TEST_SECRET_KEY).and_then(move |_| {
							ps_clone.process_package(current_package_clone.id().clone())
						});
						into_future_trait(f)
					});
					if let Err(err) = result {
						println!("{}", err.to_string());
						panic!()
					}
					break 'infinite;
				} else {
					thread::sleep(sleep_duration);
				}
			} else {
				panic!()
			}
		}
		run(&ps(), |ps| {
			let ps_clone = ps.clone();
			let dataset_id_clone = dataset_id.clone();
			let f = ps
				.login(TEST_API_KEY, TEST_SECRET_KEY)
				.and_then(move |_| ps_clone.delete_dataset(dataset_id_clone));
			into_future_trait(f)
		})
		.unwrap();
	}

	#[test]
	fn move_package_to_collection() {
		let result = run(&ps(), move |ps| {
			into_future_trait(
				ps.login(TEST_API_KEY, TEST_SECRET_KEY)
					.and_then(move |_| {
						ps.create_dataset(
							rand_suffix("__agent-test-dataset".to_string()),
							Some("A test dataset created by the agent".to_string()),
						)
						.map(|ds| (ps, ds))
					})
					.and_then(move |(ps, ds)| Ok(ds.id().clone()).map(|id| (ps, id)))
					.and_then(move |(ps, ds_id)| {
						ps.create_package(
							rand_suffix("__agent-test-collection"),
							"Collection",
							ds_id.clone(),
							None as Option<String>,
						)
						.map(|col| (ps, ds_id, col))
					})
					.and_then(move |(ps, ds_id, col)| {
						ps.create_package(
							rand_suffix("__agent-test-package"),
							"Text",
							ds_id.clone(),
							None as Option<String>,
						)
						.map(|pkg| (ps, ds_id, pkg, col))
					})
					.and_then(move |(ps, ds_id, pkg, col)| {
						// Move package into __agent-test-collection
						ps.mv(vec![pkg.take().id().clone()], Some(col.id().clone()))
							.map(|_| (ps, ds_id, col))
					})
					.and_then(move |(ps, ds_id, col)| {
						ps.get_dataset_by_id(ds_id.clone()).and_then(|dataset| {
							// Dataset now has one child
							assert_eq!(dataset.children().unwrap().len(), 1);
							Ok((ps, ds_id, col))
						})
					})
					.and_then(move |(ps, ds_id, col)| {
						ps.get_package_by_id(col.id().clone())
							.and_then(|collection| {
								// Collection now has one child
								assert_eq!(collection.children().unwrap().len(), 1);
								Ok((ps, ds_id))
							})
					})
					.and_then(move |(ps, ds_id)| ps.delete_dataset(ds_id)),
			)
		});

		if result.is_err() {
			panic!("{}", result.unwrap_err().to_string());
		}
	}

	fn upload_to_upload_service_with_delete(files: Vec<(UploadId, String)>) -> Result<()> {
		run(&ps(), move |ps| {
			let f = upload_to_upload_service(ps, files.clone())
				.and_then(move |(ps, dataset_id, _)| ps.delete_dataset(dataset_id));

			into_future_trait(f)
		})
	}

	fn upload_to_upload_service(
		ps: Pennsieve,
		files: Vec<(UploadId, String)>,
	) -> Future<(Pennsieve, DatasetNodeId, ImportId)> {
		let files = files.clone();
		let files_clone = files.clone();
		let f = ps
			.login(TEST_API_KEY, TEST_SECRET_KEY)
			.and_then(move |_| {
				ps.create_dataset(
					rand_suffix("__agent-test-dataset".to_string()),
					Some("A test dataset created by the agent".to_string()),
				)
				.map(move |ds| (ps, ds.id().clone(), ds.int_id().clone()))
			})
			.and_then(|(ps, dataset_id, dataset_int_id)| {
				ps.get_user().map(|user| {
					(
						ps,
						dataset_id,
						user.preferred_organization().unwrap().clone(),
						dataset_int_id,
					)
				})
			})
			.and_then(move |(ps, dataset_id, organization_id, dataset_int_id)| {
				ps.preview_upload(
					&organization_id.clone(),
					&dataset_int_id,
					Some((*MEDIUM_TEST_DATA_DIR).to_string()),
					&files,
					false,
					false,
				)
				.map(|preview| (ps, dataset_id, organization_id, preview))
			})
			.and_then(move |(ps, dataset_id, organization_id, preview)| {
				let ps = ps.clone();
				let ps_clone = ps.clone();
				let dataset_id = dataset_id.clone();
				let dataset_id_clone = dataset_id.clone();

				let upload_futures = preview.into_iter().map(move |package| {
					let import_id = package.import_id().clone();
					let ps = ps.clone();
					let ps_clone = ps.clone();
					let organization_id = organization_id.clone();

					let dataset_id = dataset_id.clone();
					let package = package.clone();

					let head_file: PathBuf = files_clone[0].1.clone().into();
					let file_path = head_file.parent().unwrap().canonicalize().unwrap();

					let progress_indicator = ProgressIndicator::new();

					// upload using the retries function
					ps.upload_file_chunks_with_retries(
						&organization_id,
						&import_id,
						&file_path,
						package.files().to_vec(),
						progress_indicator.clone(),
						1,
					)
					.collect()
					.map(|_| (ps_clone, dataset_id))
					.and_then(move |(ps, dataset_id)| {
						ps.complete_upload(&organization_id, &import_id, &dataset_id, None, false)
							.map(|_| import_id)
					})
				});

				futures::future::join_all(upload_futures)
					.map(|import_ids| (ps_clone, dataset_id_clone, import_ids[0].clone()))
			});

		into_future_trait(f)
	}

	#[test]
	fn upload_using_upload_service() {
		// create upload
		let result = run(&ps(), move |ps| {
			let f = ps
				.login(TEST_API_KEY, TEST_SECRET_KEY)
				.and_then(move |_| {
					ps.create_dataset(
						rand_suffix("__agent-test-dataset".to_string()),
						Some("A test dataset created by the agent".to_string()),
					)
					.map(move |ds| (ps, ds.id().clone(), ds.int_id().clone()))
				})
				.and_then(|(ps, dataset_id, dataset_int_id)| {
					ps.get_user().map(|user| {
						(
							ps,
							dataset_id,
							user.preferred_organization().unwrap().clone(),
							dataset_int_id,
						)
					})
				})
				.and_then(move |(ps, dataset_id, organization_id, dataset_int_id)| {
					let files: Vec<(UploadId, String)> = add_upload_ids(&*TEST_FILES)
						.iter()
						.map(|(id, file)| (*id, format!("{}/{}", *TEST_DATA_DIR, file)))
						.collect();
					ps.preview_upload(
						&organization_id,
						&dataset_int_id,
						None as Option<String>,
						&files,
						false,
						false,
					)
					.map(|preview| (ps, dataset_id, organization_id, preview))
				})
				.and_then(move |(ps, dataset_id, organization_id, preview)| {
					let ps = ps.clone();
					let ps_clone = ps.clone();
					let dataset_id = dataset_id.clone();
					let dataset_id_clone = dataset_id.clone();

					let upload_futures = preview.into_iter().map(move |package| {
						let import_id = package.import_id().clone();
						let ps = ps.clone();
						let ps_clone = ps.clone();
						let organization_id = organization_id.clone();

						let dataset_id = dataset_id.clone();
						let package = package.clone();

						let file_path = path::Path::new(&TEST_DATA_DIR.to_string())
							.to_path_buf()
							.canonicalize()
							.unwrap();

						let progress_indicator = ProgressIndicator::new();

						ps.upload_file_chunks(
							&organization_id,
							&import_id,
							file_path,
							package.files().to_vec(),
							None,
							progress_indicator,
							1,
						)
						.collect()
						.map(|_| (ps_clone, dataset_id))
						.and_then(move |(ps, dataset_id)| {
							ps.complete_upload(
								&organization_id,
								&import_id,
								&dataset_id,
								None,
								false,
							)
						})
					});

					futures::future::join_all(upload_futures).map(|_| (ps_clone, dataset_id_clone))
				})
				.and_then(move |(ps, dataset_id)| ps.delete_dataset(dataset_id));

			into_future_trait(f)
		});

		// check result
		if result.is_err() {
			println!("{}", result.unwrap_err().to_string());
			panic!();
		}
	}

	#[test]
	fn upload_missing_parts_using_upload_service() {
		// create upload
		let result = run(&ps(), move |ps| {
			let f = ps
				.login(TEST_API_KEY, TEST_SECRET_KEY)
				.and_then(move |_| {
					ps.create_dataset(
						rand_suffix("__agent-test-dataset".to_string()),
						Some("A test dataset created by the agent".to_string()),
					)
					.map(move |ds| (ps, ds.id().clone(), ds.int_id().clone()))
				})
				.and_then(|(ps, dataset_id, dataset_int_id)| {
					ps.get_user().map(|user| {
						(
							ps,
							dataset_id,
							user.preferred_organization().unwrap().clone(),
							dataset_int_id,
						)
					})
				})
				.and_then(move |(ps, dataset_id, organization_id, dataset_int_id)| {
					let enumerated_files = add_upload_ids(&*MEDIUM_TEST_FILES);
					ps.preview_upload(
						&organization_id,
						&dataset_int_id,
						Some((*MEDIUM_TEST_DATA_DIR).to_string()),
						&enumerated_files,
						false,
						false,
					)
					.map(|preview| (ps, dataset_id, organization_id, preview))
				})
				.and_then(move |(ps, dataset_id, organization_id, preview)| {
					let ps = ps.clone();
					let ps_clone = ps.clone();
					let dataset_id = dataset_id.clone();
					let dataset_id_clone = dataset_id.clone();

					let upload_futures = preview.into_iter().map(move |package| {
						let import_id = package.import_id().clone();
						let ps = ps.clone();
						let ps_clone = ps.clone();
						let organization_id = organization_id.clone();

						let dataset_id = dataset_id.clone();
						let package = package.clone();

						let file_path = path::Path::new(&MEDIUM_TEST_DATA_DIR.to_string())
							.to_path_buf()
							.canonicalize()
							.unwrap();

						let progress_indicator = ProgressIndicator::new();

						// only upload the first chunk
						ps.upload_file_chunks(
							&organization_id,
							&import_id,
							file_path.clone(),
							package.files().to_vec(),
							Some(response::FilesMissingParts {
								files: package
									.files()
									.to_vec()
									.iter()
									.map(|file| response::FileMissingParts {
										file_name: file.file_name().to_string(),
										missing_parts: vec![1],
										expected_total_parts: 2,
									})
									.collect(),
							}),
							progress_indicator.clone(),
							1,
						)
						.collect()
						.map(|_| (ps_clone, dataset_id))
						.and_then(move |(ps, dataset_id)| {
							ps.get_upload_status(&organization_id, &import_id)
								.map(|status| (ps, dataset_id, organization_id, import_id, status))
						})
						.and_then(
							move |(ps, dataset_id, organization_id, import_id, status)| {
								// upload the rest of the chunks based on the status response
								ps.upload_file_chunks(
									&organization_id,
									&import_id,
									file_path,
									package.files().to_vec(),
									status,
									progress_indicator,
									1,
								)
								.collect()
								.map(|_| (ps, dataset_id, organization_id, import_id))
							},
						)
						.and_then(
							move |(ps, dataset_id, organization_id, import_id)| {
								ps.complete_upload(
									&organization_id,
									&import_id,
									&dataset_id,
									None,
									false,
								)
							},
						)
					});

					futures::future::join_all(upload_futures).map(|_| (ps_clone, dataset_id_clone))
				})
				.and_then(move |(ps, dataset_id)| ps.delete_dataset(dataset_id));

			into_future_trait(f)
		});

		// check result
		if result.is_err() {
			println!("{}", result.unwrap_err().to_string());
			panic!();
		}
	}

	#[test]
	fn upload_to_upload_service_with_retries() {
		let file_paths: Vec<String> = MEDIUM_TEST_FILES
			.iter()
			.map(|file_name| {
				format!("{}/{}", MEDIUM_TEST_DATA_DIR.to_string(), file_name).to_string()
			})
			.collect();
		let enumerated_files = add_upload_ids(&file_paths);
		// create upload
		let result = upload_to_upload_service_with_delete(enumerated_files);

		// check result
		if result.is_err() {
			println!("{}", result.unwrap_err().to_string());
			panic!();
		}
	}

	#[test]
	fn upload_to_upload_service_and_get_hash() {
		let file_paths: Vec<String> = MEDIUM_TEST_FILES
			.iter()
			.map(|file_name| {
				format!("{}/{}", MEDIUM_TEST_DATA_DIR.to_string(), file_name).to_string()
			})
			.collect();
		let test_file_path = file_paths[0].clone();
		let test_file_name = test_file_path.split('/').last().unwrap().to_string();

		let enumerated_test_file: Vec<(UploadId, String)> =
			vec![(UploadId::new(0), test_file_path.clone())];

		let result = run(&ps(), move |ps| {
			let test_file_name = test_file_name.clone();

			let f = upload_to_upload_service(ps, enumerated_test_file.clone())
				.and_then(move |(ps, _, import_id)| ps.get_upload_hash(&import_id, test_file_name));
			into_future_trait(f)
		});

		// check result
		if result.is_err() {
			println!("{}", result.unwrap_err().to_string());
			panic!();
		}
	}

	#[test]
	#[cfg_attr(target_os = "windows", ignore)]
	fn upload_directory() {
		// preview upload and verify that it contains previewPath
		let result = run(&ps(), move |ps| {
			let upload_f = ps
				.login(TEST_API_KEY, TEST_SECRET_KEY)
				.and_then(move |_| {
					ps.create_dataset(
						rand_suffix("__agent-test-dataset".to_string()),
						Some("A test dataset created by the agent".to_string()),
					)
					.map(move |ds| (ps, ds.id().clone(), ds.int_id().clone()))
				})
				.and_then(|(ps, dataset_id, dataset_int_id)| {
					ps.get_user().map(|user| {
						(
							ps,
							dataset_id,
							user.preferred_organization().unwrap().clone(),
							dataset_int_id,
						)
					})
				})
				.and_then(move |(ps, dataset_id, organization_id, dataset_int_id)| {
					let files_with_path: Vec<String> = MEDIUM_TEST_FILES
						.iter()
						.map(|filename| format!("medium/{}", filename))
						.collect();
					let enumerated_files = add_upload_ids(&files_with_path);
					ps.preview_upload(
						&organization_id,
						&dataset_int_id,
						Some((*MEDIUM_TEST_DATA_DIR).to_string()),
						&enumerated_files,
						false,
						true,
					)
					.map(|preview| (ps, dataset_id, organization_id, preview))
				})
				.and_then(move |(ps, dataset_id, organization_id, preview)| {
					let ps = ps.clone();
					let ps_clone = ps.clone();
					let dataset_id = dataset_id.clone();
					let dataset_id_clone = dataset_id.clone();

					let upload_futures = preview.into_iter().map(move |package| {
						let package_copy = package.clone();
						// perview path should be expected uploaded directory
						assert_eq!(package.preview_path(), Some("medium".to_string()));

						let import_id = package_copy.import_id().clone();
						let ps = ps.clone();
						let ps_clone = ps.clone();
						let organization_id = organization_id.clone();

						let dataset_id = dataset_id.clone();

						let file_path = path::Path::new(&MEDIUM_TEST_DATA_DIR.to_string())
							.to_path_buf()
							.canonicalize()
							.unwrap();

						let progress_indicator = ProgressIndicator::new();

						// upload using the retries function
						ps.upload_file_chunks_with_retries(
							&organization_id,
							&import_id,
							&file_path,
							package_copy.files().to_vec(),
							progress_indicator.clone(),
							1,
						)
						.collect()
						.map(|_| (ps_clone, dataset_id))
						.and_then(move |(ps, dataset_id)| {
							ps.complete_upload(
								&organization_id,
								&import_id,
								&dataset_id,
								None,
								false,
							)
						})
					});

					futures::future::join_all(upload_futures).map(|_| (ps_clone, dataset_id_clone))
				})
				.and_then(move |(ps, dataset_id)| ps.delete_dataset(dataset_id));
			into_future_trait(upload_f)
		});

		// check result
		if result.is_err() {
			println!("{}", result.unwrap_err().to_string());
			panic!();
		}
	}
}
